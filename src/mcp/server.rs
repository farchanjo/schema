//! `SchemaServer` — root rmcp server struct.
//!
//! Tools are registered via the `#[tool_router]` macro attribute. Each tool is
//! an `async fn` (or sync `fn`) on the server impl block decorated with
//! `#[tool(description = "...")]`. Mutable state (the embedder requires `&mut`
//! to embed) lives behind `Arc<...>` (immutable sharing) and `tokio::sync::Mutex`
//! (async-safe interior mutability) so the server type stays `Clone` — rmcp
//! clones it per in-flight request.
//!
//! FASE 1.0 exposes:
//!   - `ping`             smoke test
//!   - `query`            generic top-K RAG
//!   - `find_decisions`   query restricted to kind = adr-madr (commit 8)
//!   - `glossary_lookup`  query restricted to kind = glossary (commit 8)
//!   - `cross_reference`  artifact_id → referencing chunks (commit 9)
//!   - `list_corpus`      debug listing (commit 8)

use std::sync::Arc;

use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use crate::config::{ProjectIdentity, SchemaConfig};
use crate::embeddings::Embedder;
use crate::retrieval::{ChunkRecord, VectorStore};

/// Empty parameter set — `ping` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PingParams {}

/// Arguments for the generic `query` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Natural-language query. Will be embedded and matched against the
    /// project's chunks via top-K nearest-neighbour search.
    pub query: String,

    /// Optional override for top-K. Defaults to `retrieval.top_k_default`
    /// from `schema.toml` (typically 8).
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// One row of a query response.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ToolChunk {
    pub source_path: String,
    pub line_start: i32,
    pub line_end: i32,
    pub artifact_id: Option<String>,
    pub title: Option<String>,
    pub kind: String,
    pub content: String,
    pub score: Option<f32>,
}

impl From<ChunkRecord> for ToolChunk {
    fn from(r: ChunkRecord) -> Self {
        Self {
            source_path: r.source_path,
            line_start: r.line_start,
            line_end: r.line_end,
            artifact_id: r.artifact_id,
            title: r.title,
            kind: r.kind,
            content: r.content,
            score: r.score,
        }
    }
}

/// Server-side state shared by all tool handlers.
#[derive(Debug)]
pub struct ServerState {
    pub config: SchemaConfig,
    pub identity: ProjectIdentity,
    pub store: VectorStore,
    pub embedder: Mutex<Embedder>,
}

/// The MCP server instance.
///
/// Cloning is cheap (clones the Arc); rmcp clones the server per request.
#[derive(Clone, Debug)]
pub struct SchemaServer {
    state: Arc<ServerState>,
}

#[tool_router(server_handler)]
impl SchemaServer {
    /// Smoke-test tool. Returns the literal string `"pong"`.
    #[tool(description = "Health probe for the schema MCP server. Returns \"pong\".")]
    fn ping(&self, _params: Parameters<PingParams>) -> String {
        info!("ping tool invoked");
        "pong".to_string()
    }

    /// Generic semantic-search tool. Embeds the query string, returns top-K
    /// closest chunks.
    #[tool(
        description = "Generic top-K semantic search over the project's indexed corpus. Returns the closest chunks (with source path + line range) for a natural-language query."
    )]
    async fn query(&self, Parameters(params): Parameters<QueryParams>) -> String {
        match self.run_query(&params).await {
            Ok(chunks) => match serde_json::to_string(&chunks) {
                Ok(json) => json,
                Err(e) => format!("{{\"error\": \"serialise: {e}\"}}"),
            },
            Err(e) => format!("{{\"error\": \"{e}\"}}"),
        }
    }
}

impl SchemaServer {
    /// Construct a server with fully-resolved state.
    pub fn with_state(state: ServerState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }

    /// Run the MCP server over stdio until the client disconnects.
    pub async fn run_stdio(self) -> anyhow::Result<()> {
        info!("starting schema MCP server (stdio transport)");
        let service = self.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        info!("schema MCP server shut down cleanly");
        Ok(())
    }

    async fn run_query(&self, params: &QueryParams) -> anyhow::Result<Vec<ToolChunk>> {
        let top_k = params
            .top_k
            .unwrap_or(self.state.config.retrieval.top_k_default);
        let vector = {
            let mut emb = self.state.embedder.lock().await;
            let mut vectors = emb.embed(vec![params.query.as_str()], None)?;
            vectors
                .pop()
                .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))?
        };
        let records = self.state.store.query_nearest(&vector, top_k, None).await?;
        Ok(records.into_iter().map(ToolChunk::from).collect())
    }
}
