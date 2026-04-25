//! `schema` — MCP server for indexing project specs, ADRs, and contracts.
//!
//! CLI entrypoint dispatches to subcommands. Default subcommand is `serve`,
//! starting the MCP server over stdio. `validate` checks `schema.toml` without
//! starting the server (FASE 1.0 stub).

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
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
}

async fn run_serve(_config: PathBuf) -> Result<()> {
    // FASE 1.0: ignore config; subsequent commit wires the loader.
    let server = SchemaServer::new();
    server.run_stdio().await
}

fn run_validate(_config: PathBuf) -> Result<()> {
    // FASE 1.0 stub. Real validation lands with the config loader commit.
    println!("schema.toml validation: not yet implemented (FASE 1.0 stub)");
    Ok(())
}
