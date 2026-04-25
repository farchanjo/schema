//! `SchemaServer` — root rmcp server struct.
//!
//! Tools are registered via the `#[tool_router]` macro attribute. Each tool is an
//! `async fn` (or sync `fn`) on the server impl block decorated with `#[tool(description = "...")]`.
//!
//! FASE 1.0 exposes only `ping` — a smoke-test endpoint that proves the stdio
//! JSON-RPC pipeline is alive. Subsequent commits add: `query`, `find_decisions`,
//! `glossary_lookup`, `cross_reference`, `list_corpus`.

use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use tracing::info;

/// Empty parameter set — `ping` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PingParams {}

/// The MCP server instance.
///
/// `Clone` is required by rmcp because each in-flight request gets a fresh handle
/// to the server. State that must persist across requests (corpus, retrieval,
/// embeddings) will live behind `Arc<...>` fields added in subsequent commits.
#[derive(Clone, Debug, Default)]
pub struct SchemaServer;

#[tool_router(server_handler)]
impl SchemaServer {
    /// Smoke-test tool. Returns the literal string `"pong"`.
    ///
    /// Validates that the rmcp server is wired correctly to stdio JSON-RPC and
    /// that Claude Code (or any MCP client) can list and invoke tools on this
    /// server. Should be removed or kept as a debug aid in FASE 1.1.
    #[tool(description = "Health probe for the schema MCP server. Returns \"pong\".")]
    fn ping(&self, _params: Parameters<PingParams>) -> String {
        info!("ping tool invoked");
        "pong".to_string()
    }
}

impl SchemaServer {
    /// Construct a new server. Today this is identical to `default()`; the
    /// signature exists to prepare for state injection in upcoming commits
    /// (config, corpus, embeddings, retrieval).
    pub fn new() -> Self {
        Self
    }

    /// Run the MCP server over stdio until the client disconnects.
    pub async fn run_stdio(self) -> anyhow::Result<()> {
        info!("starting schema MCP server (stdio transport)");
        let service = self.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        info!("schema MCP server shut down cleanly");
        Ok(())
    }
}
