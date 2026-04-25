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
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use schema::mcp::SchemaServer;

/// Top-level CLI. Run with no subcommand to start the MCP server (default).
#[derive(Parser, Debug)]
#[command(
    name = "schema",
    version,
    about = "MCP server for indexing project specs, ADRs, and contracts",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the MCP server over stdio.
    Serve {
        /// Path to the project's `schema.toml`. Defaults to `./schema.toml`.
        #[arg(long, value_name = "PATH", default_value = "schema.toml")]
        config: PathBuf,
    },

    /// Validate a `schema.toml` without starting the MCP server.
    Validate {
        /// Path to the project's `schema.toml`.
        #[arg(long, value_name = "PATH", default_value = "schema.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve {
        config: PathBuf::from("schema.toml"),
    }) {
        Command::Serve { config } => run_serve(config).await,
        Command::Validate { config } => run_validate(config),
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

async fn run_serve(config: PathBuf) -> Result<()> {
    use std::sync::Arc;
    use std::time::Duration;

    use schema::config::{ProjectIdentity, SchemaConfig};
    use schema::corpus::CorpusWatcher;
    use schema::embeddings::Embedder;
    use schema::mcp::ServerState;
    use schema::retrieval::{
        DeltaSync, VectorStore, WatcherConsumerInputs, corpus_paths_from_config,
        run_watcher_consumer,
    };
    use tokio::sync::Mutex;

    const WATCHER_DEBOUNCE: Duration = Duration::from_millis(500);

    let cfg = SchemaConfig::load(&config)?;
    let identity =
        ProjectIdentity::resolve(&cfg.project.name, &SchemaConfig::project_root(&config)?)?;
    identity.ensure_cache_dir()?;
    tracing::info!(
        project = %identity.id,
        cache_dir = %identity.cache_dir.display(),
        "schema config loaded; cache resolved",
    );

    // Initialise vector store + embedder behind Arc so the watcher consumer
    // task and the MCP tool handlers share the same instances.
    let store = Arc::new(VectorStore::open(&identity.lance_dir).await?);
    store.ensure_table().await?;
    let embedder = Arc::new(Mutex::new(Embedder::new_bge_m3()?));

    // Initial delta-sync (per ADR-0007).
    {
        let mut emb_guard = embedder.lock().await;
        let sync = DeltaSync {
            config: &cfg,
            project_root: &identity.root,
            metadata_path: &identity.metadata_path,
        };
        let report = sync.run(&store, &mut emb_guard).await?;
        tracing::info!(?report, "initial delta-sync complete");
    }

    // Wire the in-session filesystem watcher (ADR-0010 + ADR-0007 evidence
    // 2026-04-25). The watcher emits CorpusEvent on a tokio mpsc channel;
    // run_watcher_consumer debounces them and triggers a delta-sync on
    // each batch. The watcher's WatcherKeepAlive must outlive the consumer
    // task — we move both into the same spawned task.
    let watch_paths = corpus_paths_from_config(&cfg, &identity.root);
    let watch_path_refs: Vec<&std::path::Path> = watch_paths.iter().map(AsRef::as_ref).collect();
    let watcher = CorpusWatcher::new(&watch_path_refs)?;
    let (keep_alive, events) = watcher.into_parts();

    let consumer_inputs = WatcherConsumerInputs {
        config: cfg.clone(),
        project_root: identity.root.clone(),
        metadata_path: identity.metadata_path.clone(),
        store: Arc::clone(&store),
        embedder: Arc::clone(&embedder),
        debounce_window: WATCHER_DEBOUNCE,
    };
    tokio::spawn(async move {
        // Move the keep-alive into this task so the underlying notify
        // watcher lives as long as the consumer.
        let _watcher_alive = keep_alive;
        run_watcher_consumer(consumer_inputs, events).await;
    });
    tracing::info!(
        debounce_ms = u64::try_from(WATCHER_DEBOUNCE.as_millis()).unwrap_or(u64::MAX),
        watched_paths = watch_paths.len(),
        "in-session watcher consumer spawned",
    );

    let state = ServerState {
        config: cfg,
        identity,
        store,
        embedder,
    };
    SchemaServer::with_state(state).run_stdio().await
}

fn run_validate(config: PathBuf) -> Result<()> {
    use schema::config::{ProjectIdentity, SchemaConfig};

    let cfg = SchemaConfig::load(&config)?;
    let identity =
        ProjectIdentity::resolve(&cfg.project.name, &SchemaConfig::project_root(&config)?)?;

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
