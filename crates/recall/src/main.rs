//! `recall` — session-transcript retrieval MCP server (ADR-0033).
//!
//! ADR-0034 step 3 ships a minimal skeleton: a `mcp-server` subcommand
//! that registers a single `ping` tool returning `"pong"`. Future commits
//! land the six retrieval verbs (`recall`, `recall_files`, `recall_diffs`,
//! `recall_errors`, `recall_thread`, `recall_artifacts`) and the
//! operator-side CLI subcommands (ADR-0035).

#![allow(
    unused_crate_dependencies,
    reason = "binary target sees lib-only deps as unused; \
              `unused_crate_dependencies` is per-target. Schema-core \
              lands as a real dependency in a follow-up commit when \
              the embedder + chunker get wired."
)]

use std::io;

use anyhow::Result;
use clap::Command;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::io::stdio;
use rmcp::{ServiceExt, schemars, tool, tool_router};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let matches = cli().get_matches();
    match matches.subcommand() {
        Some(("mcp-server", _)) => run_mcp_server().await,
        Some((other, _)) => Err(anyhow::anyhow!("unknown subcommand: {other}")),
        None => Err(anyhow::anyhow!(
            "subcommand required (try `recall mcp-server`)"
        )),
    }
}

fn cli() -> Command {
    Command::new("recall")
        .version(env!("CARGO_PKG_VERSION"))
        .about(
            "Session-transcript retrieval MCP server (ADR-0033). \
             Skeleton: only `ping` registered today; the six retrieval \
             tools land in upcoming commits.",
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(Command::new("mcp-server").about(
            "Run the recall MCP stdio server. Spawned by Claude Code via \
                 `.mcp.json` as a `\"type\": \"stdio\"` server.",
        ))
}

async fn run_mcp_server() -> Result<()> {
    tracing::info!(
        mode = "mcp-server",
        "recall: stdio MCP skeleton starting (ADR-0034 step 3)"
    );
    let server = RecallServer;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Stdio MCP server skeleton. Future commits add `Arc<Daemon>`-style state
/// (jsonl cursor, in-memory HNSW, embedder handle) once the chunker +
/// embedder wiring lands.
#[derive(Debug, Clone)]
struct RecallServer;

/// Empty parameter set — `ping` takes no arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PingParams;

#[tool_router(server_handler)]
impl RecallServer {
    /// Liveness probe; returns the literal string `"pong"`.
    ///
    /// Mirrors schema's `ping` (ADR-0033 §"Tool surface" placeholder
    /// before the six retrieval verbs land).
    #[tool(description = "Liveness probe; returns 'pong'. Use as a connectivity smoke test.")]
    #[expect(
        clippy::same_name_method,
        reason = "rmcp tool_router macro generates an inner method with the same name as the user-declared tool fn"
    )]
    #[expect(
        clippy::unused_self,
        reason = "rmcp #[tool] handlers must be methods on the server type; making this an associated fn would break tool registration"
    )]
    fn ping(&self, _params: Parameters<PingParams>) -> String {
        tracing::debug!("ping tool invoked");
        "pong".to_string()
    }
}

/// Configure `tracing-subscriber` to read filter from `RUST_LOG` (default: info).
/// Logs go to stderr only — stdio JSON-RPC frames live on stdout.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(io::stderr))
        .init();
}
