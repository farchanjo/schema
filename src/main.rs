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
    let config_arg = Arg::new("config")
        .long("config")
        .value_name("PATH")
        .value_parser(clap::value_parser!(PathBuf))
        .default_value("schema.toml");

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
                .arg(config_arg),
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
    Services { sync, query }
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
    let Services { sync, query } = build_services(&wiring);

    let initial = sync.run().await?;
    tracing::info!(?initial, "initial delta-sync complete");

    spawn_watcher(&cfg, &identity, sync.clone())?;

    let state = ServerState {
        config: cfg,
        identity,
        query,
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
