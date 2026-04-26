//! `schema` — MCP server for indexing project specs, ADRs, and contracts.
//!
//! CLI entrypoint dispatches to subcommands. Default subcommand is `serve`,
//! starting the MCP server over stdio. `validate` checks `schema.toml` without
//! starting the server (FASE 1.0 stub).

#![allow(
    unused_crate_dependencies,
    reason = "binary target sees lib-only deps as unused; \
              `unused_crate_dependencies` is per-target. The lib's own \
              attribute covers the library; this allows the binary."
)]

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use tokio::sync::Mutex;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use schema::adapters::fastembed_embedder::FastembedEmbedder;
use schema::adapters::filesystem::{NotifyWatcher, WalkdirWalker};
use schema::adapters::markdown_chunker::MarkdownChunker;
use schema::adapters::mcp_server::{SchemaServer, ServerState};
use schema::adapters::metadata_store::TomlMetadataStore;
use schema::adapters::project_identity::ProjectIdentity;
use schema::adapters::sqlite_vec_store::{SqliteVecStore, migrate_legacy_lance_dir};
use schema::adapters::toml_config::SchemaConfig;
use schema::app::cleanup::Cleanup;
use schema::app::delta_sync::{DeltaSync, corpus_paths_from_config};
use schema::app::query::Query;
use schema::app::watcher_consumer::run_watcher_consumer;
use schema::ports::{Chunker, Embedder, MetadataStore, Persistence, Walker, Watcher};

const WATCHER_DEBOUNCE: Duration = Duration::from_millis(500);

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
                .about("Start the MCP server over stdio.")
                .arg(config_arg.clone()),
        )
        .subcommand(
            Command::new("validate")
                .about("Validate a `schema.toml` without starting the MCP server.")
                .arg(config_arg.clone()),
        )
        .subcommand(reset_subcommand(config_arg.clone()))
        .subcommand(forget_subcommand(config_arg))
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
        Some(("forget", sub)) => {
            let path = sub
                .get_one::<String>("path")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("--path is required for `schema forget`"))?;
            run_forget(config_from(sub), path).await
        }
        Some((other, _)) => Err(anyhow::anyhow!("unknown subcommand: {other}")),
        None => run_serve(PathBuf::from("schema.toml")).await,
    }
}

/// Configure `tracing-subscriber` to read filter from `RUST_LOG` (default: info).
/// Logs are written to stderr so stdio JSON-RPC is not contaminated.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(io::stderr))
        .init();
}

/// Bundle of port handles wired up for one project.
struct Wiring {
    persistence: Arc<dyn Persistence>,
    embedder: Arc<Mutex<dyn Embedder>>,
    walker: Arc<dyn Walker>,
    chunker: Arc<dyn Chunker>,
    metadata: Arc<dyn MetadataStore>,
}

async fn wire_ports(cfg: &SchemaConfig, identity: &ProjectIdentity) -> Result<Wiring> {
    // Persistence: SqliteVecStore (ADR-0011).
    migrate_legacy_lance_dir(&identity.cache_dir);
    let persistence: Arc<dyn Persistence> =
        Arc::new(SqliteVecStore::open(&identity.store_path).await?);
    persistence.ensure_ready().await?;

    // Embedder: fastembed BGE-M3 (ADR-0005).
    let embedder: Arc<Mutex<dyn Embedder>> = Arc::new(Mutex::new(FastembedEmbedder::new_bge_m3()?));

    // Walker + Chunker + MetadataStore — all sync, all wrapped in Arc<dyn>.
    let cfg_arc = Arc::new(cfg.clone());
    let walker: Arc<dyn Walker> = Arc::new(WalkdirWalker::new(cfg_arc, identity.root.clone()));
    let chunker: Arc<dyn Chunker> = Arc::new(MarkdownChunker::new(cfg.retrieval.chunk_size_max));
    let metadata: Arc<dyn MetadataStore> =
        Arc::new(TomlMetadataStore::new(identity.metadata_path.clone()));

    Ok(Wiring {
        persistence,
        embedder,
        walker,
        chunker,
        metadata,
    })
}

/// Built application services for one running session.
struct Services {
    sync: DeltaSync,
    query: Query,
    cleanup: Cleanup,
}

fn build_services(wiring: &Wiring) -> Services {
    let sync = DeltaSync::new(
        Arc::clone(&wiring.persistence),
        Arc::clone(&wiring.embedder),
        Arc::clone(&wiring.walker),
        Arc::clone(&wiring.chunker),
        Arc::clone(&wiring.metadata),
    );
    let query = Query::new(
        Arc::clone(&wiring.persistence),
        Arc::clone(&wiring.embedder),
    );
    let cleanup = Cleanup::new(
        Arc::clone(&wiring.persistence),
        Arc::clone(&wiring.metadata),
    );
    Services {
        sync,
        query,
        cleanup,
    }
}

async fn run_serve(config: PathBuf) -> Result<()> {
    let cfg = SchemaConfig::load(&config)?;
    let identity =
        ProjectIdentity::resolve(&cfg.project.name, &SchemaConfig::project_root(&config)?)?;
    identity.ensure_cache_dir()?;
    tracing::info!(
        project = %identity.id,
        cache_dir = %identity.cache_dir.display(),
        "schema config loaded; cache resolved",
    );

    let wiring = wire_ports(&cfg, &identity).await?;
    let Services {
        sync,
        query,
        cleanup,
    } = build_services(&wiring);

    let initial = sync.run().await?;
    tracing::info!(?initial, "initial delta-sync complete");

    spawn_watcher(&cfg, &identity, sync.clone())?;

    let state = ServerState {
        config: cfg,
        identity,
        query,
        cleanup,
    };
    SchemaServer::with_state(state).run_stdio().await
}

/// Wire the in-session filesystem watcher (ADR-0010 + ADR-0007 evidence
/// 2026-04-25): debounce `CorpusEvent`s and trigger delta-syncs on each batch.
fn spawn_watcher(cfg: &SchemaConfig, identity: &ProjectIdentity, sync: DeltaSync) -> Result<()> {
    let watch_paths = corpus_paths_from_config(cfg, &identity.root);
    let watcher: Box<dyn Watcher> = Box::new(NotifyWatcher::new(watch_paths.clone()));
    let (keep_alive, events) = watcher.start()?;

    tokio::spawn(async move {
        // Move the keep-alive into this task so the underlying notify
        // watcher lives as long as the consumer.
        let _watcher_alive = keep_alive;
        run_watcher_consumer(sync, WATCHER_DEBOUNCE, events).await;
    });
    tracing::info!(
        debounce_ms = u64::try_from(WATCHER_DEBOUNCE.as_millis()).unwrap_or(u64::MAX),
        watched_paths = watch_paths.len(),
        "in-session watcher consumer spawned",
    );
    Ok(())
}

fn run_validate(config: &Path) -> Result<()> {
    let cfg = SchemaConfig::load(config)?;
    let identity =
        ProjectIdentity::resolve(&cfg.project.name, &SchemaConfig::project_root(config)?)?;

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
    let cfg = SchemaConfig::load(config)?;
    let identity =
        ProjectIdentity::resolve(&cfg.project.name, &SchemaConfig::project_root(config)?)?;
    identity.ensure_cache_dir()?;
    let wiring = wire_ports(&cfg, &identity).await?;
    let cleanup = Cleanup::new(
        Arc::clone(&wiring.persistence),
        Arc::clone(&wiring.metadata),
    );
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
