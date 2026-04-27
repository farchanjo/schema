//! `schema` — MCP server for indexing project specs, ADRs, and contracts.
//!
//! CLI entrypoint dispatches to subcommands. Default subcommand is `serve`,
//! starting the MCP server over **Streamable HTTP** (ADR-0019; stdio dropped).
//! `validate` checks `schema.toml` without starting the server.

#![allow(
    unused_crate_dependencies,
    reason = "binary target sees lib-only deps as unused; \
              `unused_crate_dependencies` is per-target. The lib's own \
              attribute covers the library; this allows the binary."
)]

use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use chrono::Utc;
use clap::{Arg, ArgMatches, Command};
use tokio::net::TcpListener;
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use uuid::Uuid;

use schema::adapters::anthropic_provider::AnthropicProvider;
use schema::adapters::endpoint_toml::Endpoint;
use schema::adapters::fastembed_embedder::FastembedEmbedder;
use schema::adapters::mcp_server::{SchemaServer, build_router};
use schema::adapters::metadata_store::TomlMetadataStore;
use schema::adapters::openai_provider::OpenAiProvider;
use schema::adapters::project_identity::ProjectIdentity;
use schema::adapters::sqlite_vec_store::{SqliteVecStore, migrate_legacy_lance_dir};
use schema::adapters::toml_config::SchemaConfig;
use schema::app::cleanup::Cleanup;
use schema::app::daemon::Daemon;
use schema::app::project_instance::{ProjectInstance, spawn_project_watcher};
use schema::cli::install::{
    DAEMON_LAUNCHD_LABEL, DAEMON_SYSTEMD_UNIT_NAME, DaemonInstallInputs, InstallInputs,
    launchd_label, render_linux_daemon_unit, render_linux_unit, render_macos_daemon_plist,
    render_macos_plist, render_mcp_config_fragment, systemd_unit_name,
};
use schema::ports::{Embedder, LlmProvider, MetadataStore, Persistence};

/// Build the top-level CLI definition using the clap builder API.
///
/// We use the builder rather than `#[derive(Parser)]` because clap's derive
/// emits `#[allow(clippy::restriction)]` on its expansion, which is
/// incompatible with the restriction-group lints set to `forbid` by ADR-0012
/// (E0453: forbid cannot be downgraded).
fn cli() -> Command {
    let config_arg = config_arg();
    Command::new("schema")
        .version(env!("CARGO_PKG_VERSION"))
        .about("MCP server for indexing project specs, ADRs, and contracts")
        .subcommand_required(false)
        .arg_required_else_help(false)
        .subcommand(
            Command::new("serve")
                .about("Start the MCP server over Streamable HTTP (ADR-0019).")
                .arg(config_arg.clone()),
        )
        .subcommand(
            Command::new("validate")
                .about("Validate a `schema.toml` without starting the MCP server.")
                .arg(config_arg.clone()),
        )
        .subcommand(reset_subcommand(config_arg.clone()))
        .subcommand(forget_subcommand(config_arg.clone()))
        .subcommand(install_subcommand(config_arg.clone()))
        .subcommand(uninstall_subcommand(config_arg.clone()))
        .subcommand(service_subcommand(config_arg.clone()))
        .subcommand(mcp_config_subcommand(config_arg))
        .subcommand(daemon_subcommand())
}

/// `schema daemon` — start the shared multi-project daemon (ADR-0027).
///
/// Single `/mcp` mount, single workstation bearer. Project membership
/// is implicit and lazy: every retrieval / cleanup / synthesize tool
/// call carries a `working_directory` parameter the daemon walks up
/// to find a `schema.toml`, then wires the project on first request
/// and caches it for the daemon's lifetime. No `--config` flag and no
/// operator-curated registry (ADR-0027 supersedes ADR-0026's
/// `schema project register / unregister / list`).
fn daemon_subcommand() -> Command {
    Command::new("daemon").about(
        "Start the shared multi-project MCP daemon (ADR-0027). Single /mcp mount + single \
         workstation bearer. Project membership is lazy — every tool call carries a \
         working_directory parameter the daemon walks up to find schema.toml.",
    )
}

/// Shared `--config` argument used by every subcommand.
fn config_arg() -> Arg {
    Arg::new("config")
        .long("config")
        .value_name("PATH")
        .value_parser(clap::value_parser!(PathBuf))
        .default_value("schema.toml")
}

/// `schema reset --config X --yes` (ADR-0015).
fn reset_subcommand(config_arg: Arg) -> Command {
    Command::new("reset")
        .about(
            "DESTRUCTIVE — wipe every chunk and reset the manifest for this project (ADR-0015). Requires --yes.",
        )
        .arg(config_arg)
        .arg(
            Arg::new("yes")
                .long("yes")
                .action(clap::ArgAction::SetTrue)
                .help("Confirm the destructive operation. Required."),
        )
}

/// `schema forget --config X --path Y` (ADR-0015).
fn forget_subcommand(config_arg: Arg) -> Command {
    Command::new("forget")
        .about(
            "DESTRUCTIVE — drop chunks for one source path and remove it from the manifest (ADR-0015).",
        )
        .arg(config_arg)
        .arg(
            Arg::new("path")
                .long("path")
                .value_name("PATH")
                .required(true)
                .help("Source path (relative to project root) to drop from the index."),
        )
}

/// `schema install --service --config X [--binary-path Y]` (ADR-0020) or
/// `schema install --daemon [--binary-path Y] [--nice N]` (ADR-0027).
fn install_subcommand(config_arg: Arg) -> Command {
    Command::new("install")
        .about(
            "Render and install a service unit. Two mutually exclusive shapes: \
             --service installs the per-project unit (ADR-0020); --daemon installs \
             the workstation-level shared daemon unit (ADR-0027).",
        )
        .arg(config_arg)
        .arg(install_service_flag())
        .arg(install_daemon_flag())
        .arg(install_nice_arg())
        .arg(install_binary_path_arg())
}

fn install_service_flag() -> Arg {
    Arg::new("service")
        .long("service")
        .action(clap::ArgAction::SetTrue)
        .conflicts_with("daemon")
        .help("Install the per-project unit (ADR-0020). Requires --config.")
}

fn install_daemon_flag() -> Arg {
    Arg::new("daemon")
        .long("daemon")
        .action(clap::ArgAction::SetTrue)
        .conflicts_with("service")
        .help(
            "Install the workstation-level shared daemon unit (ADR-0027). \
             No --config flag — project membership is lazy via working_directory.",
        )
}

fn install_nice_arg() -> Arg {
    Arg::new("nice")
        .long("nice")
        .value_name("N")
        .value_parser(clap::value_parser!(u8))
        .default_value("5")
        .help(
            "ONNX-thread CPU politeness for the daemon unit (ADR-0018). \
             Per-project --service mode reads `[embedding].nice` from schema.toml \
             and ignores this flag.",
        )
}

fn install_binary_path_arg() -> Arg {
    Arg::new("binary-path")
        .long("binary-path")
        .value_name("PATH")
        .value_parser(clap::value_parser!(PathBuf))
        .help(
            "Override the binary path written into the unit \
             (default: /usr/local/bin/schema per ADR-0014).",
        )
}

/// `schema uninstall --service --config X` (ADR-0020) or
/// `schema uninstall --daemon` (ADR-0027).
fn uninstall_subcommand(config_arg: Arg) -> Command {
    Command::new("uninstall")
        .about(
            "Remove a service unit. --service removes a per-project unit (ADR-0020); \
             --daemon removes the workstation-level shared daemon unit (ADR-0027).",
        )
        .arg(config_arg)
        .arg(
            Arg::new("service")
                .long("service")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("daemon")
                .help("Uninstall the per-project unit (ADR-0020). Requires --config."),
        )
        .arg(
            Arg::new("daemon")
                .long("daemon")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("service")
                .help("Uninstall the workstation-level shared daemon unit (ADR-0027)."),
        )
}

/// `schema service status --config X` (ADR-0020).
fn service_subcommand(config_arg: Arg) -> Command {
    Command::new("service")
        .about("Inspect the running per-project service (ADR-0020).")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("status")
                .about("Print URL, token, and lifecycle hints for the running server.")
                .arg(config_arg),
        )
}

/// `schema mcp-config --config X` (ADR-0021 helper).
fn mcp_config_subcommand(config_arg: Arg) -> Command {
    Command::new("mcp-config")
        .about(
            "Print the `mcpServers` JSON fragment for the running server, ready to paste into a consumer's `.mcp.json` (ADR-0021).",
        )
        .arg(config_arg)
}

async fn run_forget_dispatch(sub: &ArgMatches) -> Result<()> {
    let path = sub
        .get_one::<String>("path")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("--path is required for `schema forget`"))?;
    run_forget(config_from(sub), path).await
}

fn run_install_dispatch(sub: &ArgMatches) -> Result<()> {
    let binary_override = sub.get_one::<PathBuf>("binary-path").cloned();
    if sub.get_flag("daemon") {
        let nice = sub.get_one::<u8>("nice").copied().unwrap_or(5);
        run_install_daemon(binary_override, nice)
    } else if sub.get_flag("service") {
        run_install_service(&config_from(sub), binary_override)
    } else {
        Err(anyhow::anyhow!(
            "`schema install` requires either --service (per-project, ADR-0020) \
             or --daemon (workstation-level, ADR-0027)"
        ))
    }
}

fn run_uninstall_dispatch(sub: &ArgMatches) -> Result<()> {
    if sub.get_flag("daemon") {
        run_uninstall_daemon()
    } else if sub.get_flag("service") {
        run_uninstall_service(&config_from(sub))
    } else {
        Err(anyhow::anyhow!(
            "`schema uninstall` requires either --service (per-project, ADR-0020) \
             or --daemon (workstation-level, ADR-0027)"
        ))
    }
}

fn config_from(matches: &ArgMatches) -> PathBuf {
    matches
        .get_one::<PathBuf>("config")
        .cloned()
        .unwrap_or_else(|| PathBuf::from("schema.toml"))
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let matches = cli().get_matches();

    match matches.subcommand() {
        Some(("serve", sub)) => run_serve(config_from(sub)).await,
        Some(("validate", sub)) => run_validate(&config_from(sub)),
        Some(("reset", sub)) => run_reset(config_from(sub), sub.get_flag("yes")).await,
        Some(("forget", sub)) => run_forget_dispatch(sub).await,
        Some(("install", sub)) => run_install_dispatch(sub),
        Some(("uninstall", sub)) => run_uninstall_dispatch(sub),
        Some(("service", sub)) => match sub.subcommand() {
            Some(("status", inner)) => run_service_status(&config_from(inner)),
            _ => Err(anyhow::anyhow!(
                "unknown `service` subcommand; expected `status`"
            )),
        },
        Some(("mcp-config", sub)) => run_mcp_config(&config_from(sub)),
        Some(("daemon", _)) => run_daemon().await,
        Some((other, _)) => Err(anyhow::anyhow!("unknown subcommand: {other}")),
        None => run_serve(PathBuf::from("schema.toml")).await,
    }
}

/// Configure `tracing-subscriber` to read filter from `RUST_LOG` (default: info).
/// Logs are written to stderr so launchd / systemd capture them via the
/// `StandardErrorPath` / `StandardError=` knobs (ADR-0020).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(io::stderr))
        .init();
}

/// Pick the concrete `LlmProvider` adapter implementing ADR-0025's
/// selection rules. Returns `None` when no provider is configured
/// or when the explicit `[llm].provider = "none"` is set.
fn resolve_llm_provider(cfg: &SchemaConfig) -> Result<Option<Arc<dyn LlmProvider>>> {
    use std::env;

    let anthropic_key = env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty());
    let openai_key = env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty());
    let model = cfg.llm.model.clone();
    match cfg.llm.provider.as_str() {
        "none" => {
            tracing::info!("llm: provider=none — synthesize tool disabled");
            Ok(None)
        }
        "anthropic" => build_anthropic(anthropic_key, model, "explicit"),
        "openai" => build_openai(openai_key, model, "explicit"),
        "auto" => resolve_auto(anthropic_key, openai_key, model),
        other => Err(anyhow::anyhow!(
            "[llm] provider = {other:?} is not supported (must be one of \
             \"anthropic\", \"openai\", \"none\", \"auto\")"
        )),
    }
}

fn build_anthropic(
    key: Option<String>,
    model: String,
    selection: &'static str,
) -> Result<Option<Arc<dyn LlmProvider>>> {
    let key = key.ok_or_else(|| {
        anyhow::anyhow!("[llm] provider = \"anthropic\" but ANTHROPIC_API_KEY is unset")
    })?;
    tracing::info!(provider = "anthropic", selection, "llm: provider selected");
    Ok(Some(Arc::new(AnthropicProvider::new(key, model)?)))
}

fn build_openai(
    key: Option<String>,
    model: String,
    selection: &'static str,
) -> Result<Option<Arc<dyn LlmProvider>>> {
    let key = key.ok_or_else(|| {
        anyhow::anyhow!("[llm] provider = \"openai\" but OPENAI_API_KEY is unset")
    })?;
    tracing::info!(provider = "openai", selection, "llm: provider selected");
    Ok(Some(Arc::new(OpenAiProvider::new(key, model)?)))
}

fn resolve_auto(
    anthropic_key: Option<String>,
    openai_key: Option<String>,
    model: String,
) -> Result<Option<Arc<dyn LlmProvider>>> {
    match (anthropic_key, openai_key) {
        (Some(key), None) => build_anthropic(Some(key), model, "auto"),
        (None, Some(key)) => build_openai(Some(key), model, "auto"),
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "[llm] provider = \"auto\" but both ANTHROPIC_API_KEY and \
             OPENAI_API_KEY are set; pin one with [llm] provider = \"anthropic\" \
             or \"openai\" (or SCHEMA_LLM_PROVIDER)"
        )),
        (None, None) => {
            tracing::info!("llm: no provider key set — synthesize tool disabled");
            Ok(None)
        }
    }
}

async fn run_serve(config: PathBuf) -> Result<()> {
    let (cfg, identity) = resolve_serve_context(&config)?;
    let embedder: Arc<Mutex<dyn Embedder>> = Arc::new(Mutex::new(FastembedEmbedder::new_bge_m3()?));
    let llm_provider = resolve_llm_provider(&cfg)?;
    let project = ProjectInstance::wire(cfg, identity, &embedder, llm_provider.clone()).await?;
    let initial = project.sync.run().await?;
    tracing::info!(?initial, "initial delta-sync complete");
    spawn_project_watcher(&project)?;
    let endpoint_path = project.identity.cache_dir.join("endpoint.toml");
    let daemon = Arc::new(Daemon::new_pre_wired(embedder, llm_provider, project));
    let server = SchemaServer::with_daemon(daemon);
    serve_http(server, endpoint_path).await
}

fn resolve_serve_context(config: &Path) -> Result<(SchemaConfig, ProjectIdentity)> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(config))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;
    identity.ensure_cache_dir()?;
    tracing::info!(
        project = %identity.id,
        cache_dir = %identity.cache_dir.display(),
        "schema config loaded; cache resolved",
    );
    Ok((cfg, identity))
}

/// Start the Streamable HTTP MCP server (ADR-0019).
///
/// Bind `127.0.0.1:0` (kernel chooses port), generate a fresh bearer token
/// (ADR-0021), write `endpoint.toml` with `0600` permissions, then run
/// `axum::serve` until SIGTERM / Ctrl-C drains the in-flight sessions.
async fn serve_http(server: SchemaServer, endpoint_path: PathBuf) -> Result<()> {
    let listener = bind_localhost_listener().await?;
    let bound = listener.local_addr()?;
    tracing::info!(address = %bound, "HTTP MCP listener bound");

    let token = Uuid::new_v4().to_string();
    write_endpoint_file(&endpoint_path, &bound, &token)?;

    let cancellation = CancellationToken::new();
    let router = build_router(server, token, cancellation.clone());
    let serve_result = run_axum_until_shutdown(listener, router, cancellation).await;

    cleanup_endpoint_file(&endpoint_path);

    serve_result
}

/// `schema daemon` — start the shared multi-project daemon (ADR-0026 slice 4b).
///
/// Reads the project registry from `Registry::default_path()`, wires
/// every project against one shared `Embedder`, binds **one** localhost
/// port, mounts `/mcp/<project_id>` for every project (each gated by
/// the project's bearer), runs initial delta-sync + watcher per
/// project, and writes a per-project `endpoint.toml` pointing at the
/// shared URL with the project's path and token. On SIGTERM / Ctrl-C
/// the in-flight sessions are drained and every `endpoint.toml` is
/// removed.
async fn run_daemon() -> Result<()> {
    let embedder: Arc<Mutex<dyn Embedder>> = Arc::new(Mutex::new(FastembedEmbedder::new_bge_m3()?));
    let llm_provider = resolve_llm_provider_for_daemon()?;
    let daemon = Arc::new(Daemon::new(embedder, llm_provider));
    tracing::info!("daemon: empty (lazy resolve via working_directory per ADR-0027)");

    let listener = bind_localhost_listener().await?;
    let bound = listener.local_addr()?;
    tracing::info!(address = %bound, "daemon HTTP listener bound");

    let token = Uuid::new_v4().to_string();
    let endpoint_path = global_endpoint_path()?;
    write_global_endpoint_file(&endpoint_path, &bound, &token)?;

    let cancellation = CancellationToken::new();
    let server = SchemaServer::with_daemon(daemon);
    let router = build_router(server, token, cancellation.clone());
    let serve_result = run_axum_until_shutdown(listener, router, cancellation).await;

    cleanup_endpoint_file(&endpoint_path);
    serve_result
}

/// Resolve the platform-default global endpoint.toml path (ADR-0027).
///
/// macOS: `~/Library/Application Support/schema/endpoint.toml`.
/// Linux: `~/.local/state/schema/endpoint.toml`.
fn global_endpoint_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        anyhow::anyhow!("could not resolve home directory for global endpoint.toml")
    })?;
    let suffix: &Path = if cfg!(target_os = "macos") {
        Path::new("Library/Application Support/schema/endpoint.toml")
    } else {
        Path::new(".local/state/schema/endpoint.toml")
    };
    Ok(home.join(suffix))
}

/// Write the daemon's **global** endpoint.toml — one file for the
/// whole workstation. The URL points at the shared `/mcp` mount
/// (no per-project path segment under ADR-0027).
fn write_global_endpoint_file(path: &Path, bound: &SocketAddr, token: &str) -> Result<()> {
    let endpoint = Endpoint {
        version: 1,
        url: format!("http://{bound}/mcp"),
        token: token.to_string(),
        pid: process::id(),
        started_at: Utc::now().to_rfc3339(),
    };
    endpoint.write_atomic(path)?;
    tracing::info!(path = %path.display(), "daemon: global endpoint.toml written (mode 0600)");
    Ok(())
}

/// LLM provider resolution for the shared daemon. Reads
/// `[llm].provider` indirectly via `SCHEMA_LLM_PROVIDER` env var
/// (the registry has no `[llm]` section — that's per-project and
/// would conflict if two projects pinned different providers).
/// `auto` is the default; explicit pinning is via the env knob,
/// matching ADR-0023's overlay semantics.
fn resolve_llm_provider_for_daemon() -> Result<Option<Arc<dyn LlmProvider>>> {
    use std::env;
    let provider = env::var("SCHEMA_LLM_PROVIDER").unwrap_or_else(|_| "auto".to_string());
    let model = env::var("SCHEMA_LLM_MODEL").unwrap_or_default();
    let anthropic_key = env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty());
    let openai_key = env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty());
    match provider.as_str() {
        "none" => {
            tracing::info!("daemon llm: provider=none — synthesize tool disabled across projects");
            Ok(None)
        }
        "anthropic" => build_anthropic(anthropic_key, model, "explicit"),
        "openai" => build_openai(openai_key, model, "explicit"),
        "auto" => resolve_auto(anthropic_key, openai_key, model),
        other => Err(anyhow::anyhow!(
            "SCHEMA_LLM_PROVIDER={other:?} is not supported (use anthropic / openai / none / auto)"
        )),
    }
}

async fn bind_localhost_listener() -> Result<TcpListener> {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = TcpListener::bind(bind_addr).await?;
    Ok(listener)
}

fn write_endpoint_file(path: &Path, bound: &SocketAddr, token: &str) -> Result<()> {
    let endpoint = Endpoint {
        version: 1,
        url: format!("http://{bound}"),
        token: token.to_string(),
        pid: process::id(),
        started_at: Utc::now().to_rfc3339(),
    };
    endpoint.write_atomic(path)?;
    tracing::info!(path = %path.display(), "endpoint.toml written (mode 0600)");
    Ok(())
}

async fn run_axum_until_shutdown(
    listener: TcpListener,
    router: Router,
    cancellation: CancellationToken,
) -> Result<()> {
    let shutdown_signal = async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown signal received; cancelling sessions");
        cancellation.cancel();
    };
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .map_err(anyhow::Error::from)
}

/// Wait for the first of SIGINT (Ctrl-C) or SIGTERM (`kill <pid>`,
/// launchd `bootout`, systemd `stop`). `tokio::signal::ctrl_c()` only
/// covers SIGINT; without explicit SIGTERM handling the kernel kills
/// the process with default action and `endpoint.toml` cleanup never
/// runs (ADR-0019 evidence amendment 2026-04-26).
async fn wait_for_shutdown_signal() {
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to install SIGTERM handler; falling back to SIGINT-only"
            );
            if let Err(e) = ctrl_c().await {
                tracing::warn!(error = %e, "ctrl_c handler failed; cancelling immediately");
            }
            return;
        }
    };
    tokio::select! {
        result = ctrl_c() => {
            if let Err(e) = result {
                tracing::warn!(error = %e, "ctrl_c handler failed; cancelling immediately");
            } else {
                tracing::info!("SIGINT received");
            }
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM received");
        }
    }
}

fn cleanup_endpoint_file(path: &Path) {
    if let Err(e) = Endpoint::remove_quiet(path) {
        tracing::warn!(error = %e, path = %path.display(), "failed to remove endpoint.toml on shutdown");
    } else {
        tracing::info!(path = %path.display(), "endpoint.toml removed on shutdown");
    }
}

fn run_validate(config: &Path) -> Result<()> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(config))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "schema.toml is valid.")?;
    writeln!(out, "  project name : {}", cfg.project.name)?;
    writeln!(out, "  project id   : {}", identity.id)?;
    writeln!(out, "  project root : {}", identity.root.display())?;
    writeln!(out, "  cache dir    : {}", identity.cache_dir.display())?;
    writeln!(out, "  corpus       : {} entries", cfg.corpus.len())?;
    for c in &cfg.corpus {
        writeln!(out, "    - {} ({:?})", c.path.display(), c.kind)?;
    }
    Ok(())
}

/// Build a one-shot [`Cleanup`] for off-session subcommands.
///
/// Mirrors `run_serve`'s wiring (load config, resolve identity, ensure cache
/// dir, open the store) but stops short of starting the MCP server, the
/// watcher, or the initial delta-sync — those are not meaningful for a
/// destructive cleanup verb.
async fn build_cleanup(config: &Path) -> Result<(ProjectIdentity, Cleanup)> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(config))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;
    identity.ensure_cache_dir()?;
    // Cleanup needs only persistence + metadata — skip the embedder /
    // walker / chunker that `ProjectInstance::wire` would build, since
    // a one-shot `reset` / `forget` should not pay the ~2 GB ONNX
    // load cost.
    migrate_legacy_lance_dir(&identity.cache_dir);
    let persistence: Arc<dyn Persistence> =
        Arc::new(SqliteVecStore::open(&identity.store_path).await?);
    persistence.ensure_ready().await?;
    let metadata: Arc<dyn MetadataStore> =
        Arc::new(TomlMetadataStore::new(identity.metadata_path.clone()));
    let cleanup = Cleanup::new(persistence, metadata);
    Ok((identity, cleanup))
}

/// `schema reset --config X --yes` — wipe every chunk + manifest entry for
/// this project (ADR-0015). Without `--yes`, abort with a friendly message.
async fn run_reset(config: PathBuf, confirmed: bool) -> Result<()> {
    if !confirmed {
        tracing::error!(
            "`schema reset` is destructive; rerun with --yes to confirm wiping the index for this project",
        );
        return Err(anyhow::anyhow!("missing --yes confirmation"));
    }
    let (identity, cleanup) = build_cleanup(&config).await?;
    tracing::info!(
        project = %identity.id,
        cache_dir = %identity.cache_dir.display(),
        "schema reset: wiping persistence + manifest",
    );
    cleanup.reset_index().await?;
    tracing::info!(project = %identity.id, "schema reset complete");
    Ok(())
}

/// `schema forget --config X --path Y` — drop one source path from the
/// index + manifest (ADR-0015).
async fn run_forget(config: PathBuf, path: String) -> Result<()> {
    let (identity, cleanup) = build_cleanup(&config).await?;
    tracing::info!(
        project = %identity.id,
        path = %path,
        "schema forget: dropping path from index + manifest",
    );
    cleanup.forget_source(&path).await?;
    tracing::info!(project = %identity.id, path = %path, "schema forget complete");
    Ok(())
}

/// Resolve the path the rendered service unit lives at on the current OS.
fn service_unit_path(project_id: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home dir not resolvable"))?;
    if cfg!(target_os = "macos") {
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", launchd_label(project_id))))
    } else {
        Ok(home
            .join(".config/systemd/user")
            .join(systemd_unit_name(project_id)))
    }
}

/// Render the per-OS template and write it to disk; print the operator
/// command needed to actually load the unit (we deliberately do *not* shell
/// out to `launchctl bootstrap` / `systemctl --user` so this verb stays
/// runnable in headless / CI contexts).
fn run_install_service(config: &Path, binary_override: Option<PathBuf>) -> Result<()> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(config))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;
    identity.ensure_cache_dir()?;
    let project_id = identity.id.to_string();
    let inputs = InstallInputs::resolve(&resolved_config, &cfg, &identity, binary_override)?;
    let unit_path = service_unit_path(&project_id)?;
    write_unit_file(&unit_path, &inputs)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "wrote service unit: {}", unit_path.display())?;
    print_install_hint(&mut out, &project_id, &unit_path)
}

fn write_unit_file(unit_path: &Path, inputs: &InstallInputs) -> Result<()> {
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating service unit directory {}", parent.display()))?;
    }
    let rendered = if cfg!(target_os = "macos") {
        render_macos_plist(inputs)
    } else {
        render_linux_unit(inputs)
    };
    fs::write(unit_path, rendered).with_context(|| format!("writing {}", unit_path.display()))?;
    Ok(())
}

fn print_install_hint(out: &mut impl Write, project_id: &str, unit_path: &Path) -> Result<()> {
    let label = if cfg!(target_os = "macos") {
        launchd_label(project_id)
    } else {
        systemd_unit_name(project_id)
    };
    print_load_hint(out, &label, unit_path)
}

/// `schema install --daemon` (ADR-0027). Render the workstation-level
/// unit (no `--config` flag, no `{project_id}` slot, single label
/// `com.farchanjo.schema.daemon`), write it to the per-OS
/// `LaunchAgents` / systemd location, print the operator command to
/// load it.
fn run_install_daemon(binary_override: Option<PathBuf>, nice: u8) -> Result<()> {
    let inputs = DaemonInstallInputs::resolve(binary_override, nice)?;
    if let Some(parent) = inputs.stderr_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating daemon state dir {}", parent.display()))?;
    }
    let unit_path = daemon_unit_path()?;
    write_daemon_unit_file(&unit_path, &inputs)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "wrote daemon unit: {}", unit_path.display())?;
    print_load_hint(&mut out, daemon_unit_label(), &unit_path)
}

/// Mirror of [`run_uninstall_service`] for the workstation-level
/// daemon unit (ADR-0027). Removes the unit file from disk and prints
/// the operator command to unload it.
fn run_uninstall_daemon() -> Result<()> {
    let unit_path = daemon_unit_path()?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    print_unload_hint(&mut out, daemon_unit_label())?;
    if unit_path.exists() {
        fs::remove_file(&unit_path).with_context(|| format!("removing {}", unit_path.display()))?;
        writeln!(out, "removed daemon unit: {}", unit_path.display())?;
    } else {
        writeln!(out, "daemon unit absent: {}", unit_path.display())?;
    }
    Ok(())
}

const fn daemon_unit_label() -> &'static str {
    if cfg!(target_os = "macos") {
        DAEMON_LAUNCHD_LABEL
    } else {
        DAEMON_SYSTEMD_UNIT_NAME
    }
}

fn daemon_unit_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home dir not resolvable"))?;
    if cfg!(target_os = "macos") {
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{DAEMON_LAUNCHD_LABEL}.plist")))
    } else {
        Ok(home
            .join(".config/systemd/user")
            .join(DAEMON_SYSTEMD_UNIT_NAME))
    }
}

fn write_daemon_unit_file(unit_path: &Path, inputs: &DaemonInstallInputs) -> Result<()> {
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating daemon unit directory {}", parent.display()))?;
    }
    let rendered = if cfg!(target_os = "macos") {
        render_macos_daemon_plist(inputs)
    } else {
        render_linux_daemon_unit(inputs)
    };
    fs::write(unit_path, rendered).with_context(|| format!("writing {}", unit_path.display()))?;
    Ok(())
}

fn print_load_hint(out: &mut impl Write, label: &str, unit_path: &Path) -> Result<()> {
    writeln!(out)?;
    if cfg!(target_os = "macos") {
        writeln!(
            out,
            "to load (idempotent — bootout first if already loaded):"
        )?;
        writeln!(
            out,
            "  launchctl bootout gui/$(id -u)/{label} 2>/dev/null; \\"
        )?;
        writeln!(
            out,
            "  launchctl bootstrap gui/$(id -u) {}",
            unit_path.display()
        )?;
    } else {
        writeln!(out, "to load:")?;
        writeln!(
            out,
            "  systemctl --user daemon-reload && systemctl --user enable --now {label}"
        )?;
    }
    Ok(())
}

fn print_unload_hint(out: &mut impl Write, label: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        writeln!(out, "to unload first (idempotent):")?;
        writeln!(out, "  launchctl bootout gui/$(id -u)/{label} 2>/dev/null")?;
    } else {
        writeln!(out, "to disable + stop first:")?;
        writeln!(out, "  systemctl --user disable --now {label}")?;
    }
    Ok(())
}

/// Remove the service unit file from disk and print the operator command to
/// unload it. Same headless-safe split as `run_install_service`.
fn run_uninstall_service(config: &Path) -> Result<()> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(config))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;
    let project_id = identity.id.to_string();
    let unit_path = service_unit_path(&project_id)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    print_uninstall_hint(&mut out, &project_id)?;
    if unit_path.exists() {
        fs::remove_file(&unit_path).with_context(|| format!("removing {}", unit_path.display()))?;
        writeln!(out, "removed service unit: {}", unit_path.display())?;
    } else {
        writeln!(out, "service unit absent: {}", unit_path.display())?;
    }
    Ok(())
}

fn print_uninstall_hint(out: &mut impl Write, project_id: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        let label = launchd_label(project_id);
        writeln!(out, "to unload first (idempotent):")?;
        writeln!(out, "  launchctl bootout gui/$(id -u)/{label} 2>/dev/null")?;
    } else {
        writeln!(out, "to disable + stop first:")?;
        writeln!(
            out,
            "  systemctl --user disable --now {}",
            systemd_unit_name(project_id)
        )?;
    }
    Ok(())
}

/// Print URL + lifecycle hints for the running per-project server.
fn run_service_status(config: &Path) -> Result<()> {
    let (cfg, resolved_config) = SchemaConfig::resolve(Some(config))?;
    let identity = ProjectIdentity::resolve(
        &cfg.project.name,
        &SchemaConfig::project_root(&resolved_config)?,
    )?;
    let endpoint_path = identity.cache_dir.join("endpoint.toml");
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "project    : {}", identity.id)?;
    writeln!(out, "cache dir  : {}", identity.cache_dir.display())?;
    writeln!(out, "endpoint   : {}", endpoint_path.display())?;
    match Endpoint::load(&endpoint_path) {
        Ok(endpoint) => {
            writeln!(out, "status     : RUNNING")?;
            writeln!(out, "  url      : {}", endpoint.url)?;
            writeln!(out, "  pid      : {}", endpoint.pid)?;
            writeln!(out, "  started  : {}", endpoint.started_at)?;
            writeln!(
                out,
                "  token    : <redacted; run `schema mcp-config` to see it>"
            )?;
        }
        Err(e) => {
            writeln!(out, "status     : NOT RUNNING ({e})")?;
        }
    }
    Ok(())
}

/// Print the `mcpServers` JSON fragment for the running server.
///
/// ADR-0027 makes the daemon's `endpoint.toml` global (one per
/// workstation), so the verb no longer needs `--config <path>`. The
/// `--config` argument is still accepted for back-compat with older
/// muscle memory but ignored; the snippet always describes the
/// shared daemon endpoint.
fn run_mcp_config(_config: &Path) -> Result<()> {
    let endpoint_path = global_endpoint_path()?;
    let endpoint = Endpoint::load(&endpoint_path).with_context(|| {
        format!(
            "reading global {}; is the schema daemon running? (start it with `schema daemon` \
             or via the launchd / systemd unit installed by `schema install --daemon`)",
            endpoint_path.display()
        )
    })?;
    let snippet = render_mcp_config_fragment(&endpoint);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{{")?;
    writeln!(out, "  \"mcpServers\": {{")?;
    writeln!(out, "{snippet}")?;
    writeln!(out, "  }}")?;
    writeln!(out, "}}")?;
    Ok(())
}
