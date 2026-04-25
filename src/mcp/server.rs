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

/// Arguments for `find_decisions` — semantic search restricted to ADR-MADR.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDecisionsParams {
    /// What you want to find decisions about (e.g. "session lifetime",
    /// "step-up authentication").
    pub query: String,

    #[serde(default)]
    pub top_k: Option<usize>,
}

/// Arguments for `glossary_lookup` — semantic search restricted to glossary kinds.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GlossaryLookupParams {
    /// Term to look up. The match is semantic, so synonyms work
    /// (e.g. "RBAC" finds "role-based access control").
    pub term: String,

    #[serde(default)]
    pub top_k: Option<usize>,
}

/// Empty parameter set — `list_corpus` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCorpusParams {}

/// Result of `list_corpus`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CorpusListing {
    pub project: String,
    pub source_paths: Vec<String>,
}

/// Arguments for `cross_reference` — given an artifact id, find chunks that
/// define it and chunks that reference it.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossReferenceParams {
    /// Artifact identifier, e.g. `"ADR-0055"`. Matched exactly against
    /// the `artifact_id` column for definitional hits, and against
    /// `content LIKE '%id%'` (excluding the artifact's own chunks) for
    /// referencing hits.
    pub artifact_id: String,

    /// Cap on returned definitional chunks. Default 16.
    #[serde(default)]
    pub definition_limit: Option<usize>,

    /// Cap on returned referencing chunks. Default 32.
    #[serde(default)]
    pub reference_limit: Option<usize>,
}

/// Two-sided result returned by `cross_reference`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CrossReferenceResult {
    /// Chunks whose `artifact_id` matches exactly.
    pub definition: Vec<ToolChunk>,
    /// Chunks whose content mentions the artifact id (excluding the
    /// artifact's own chunks).
    pub references: Vec<ToolChunk>,
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
        match self.run_query(&params.query, params.top_k, None).await {
            Ok(chunks) => json_or_error(&chunks),
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Semantic search restricted to ADRs (kind = adr-madr).
    #[tool(
        description = "Top-K semantic search restricted to architectural decisions (ADRs). Use when you specifically want decisions and their rationale, not general docs."
    )]
    async fn find_decisions(&self, Parameters(params): Parameters<FindDecisionsParams>) -> String {
        match self
            .run_query(&params.query, params.top_k, Some("AdrMadr"))
            .await
        {
            Ok(chunks) => json_or_error(&chunks),
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Term lookup restricted to glossary entries.
    #[tool(
        description = "Look up a term in the project's glossary. Returns the closest matching glossary chunks (semantic match, so synonyms and related terms surface)."
    )]
    async fn glossary_lookup(
        &self,
        Parameters(params): Parameters<GlossaryLookupParams>,
    ) -> String {
        match self
            .run_query(&params.term, params.top_k, Some("Glossary"))
            .await
        {
            Ok(chunks) => json_or_error(&chunks),
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Cross-reference: artifact id → definition + referencing chunks.
    #[tool(
        description = "Given an artifact id (e.g. \"ADR-0055\"), return both the chunks that define it (artifact_id match) and chunks that mention it elsewhere (content match). Useful for navigating decision relationships and impact analysis."
    )]
    async fn cross_reference(
        &self,
        Parameters(params): Parameters<CrossReferenceParams>,
    ) -> String {
        let def_limit = params.definition_limit.unwrap_or(16);
        let ref_limit = params.reference_limit.unwrap_or(32);

        let store = &self.state.store;
        let definition = match store
            .find_by_artifact_id(&params.artifact_id, def_limit)
            .await
        {
            Ok(records) => records.into_iter().map(ToolChunk::from).collect(),
            Err(e) => return format_error(&format!("definition lookup failed: {e}")),
        };
        let references = match store.find_mentioning(&params.artifact_id, ref_limit).await {
            Ok(records) => records.into_iter().map(ToolChunk::from).collect(),
            Err(e) => return format_error(&format!("references lookup failed: {e}")),
        };

        json_or_error(&CrossReferenceResult {
            definition,
            references,
        })
    }

    /// Debug — list every distinct source path indexed for this project.
    #[tool(
        description = "List every source file currently in the project's index. Useful for verifying what schema sees vs what the project ships."
    )]
    async fn list_corpus(&self, _params: Parameters<ListCorpusParams>) -> String {
        match self.state.store.list_source_paths().await {
            Ok(paths) => {
                let listing = CorpusListing {
                    project: self.state.config.project.name.clone(),
                    source_paths: paths,
                };
                json_or_error(&listing)
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }
}

/// Serialise any `Serialize` value to JSON; on failure return a JSON-encoded
/// error string suitable for the MCP response slot.
fn json_or_error<T: Serialize>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(json) => json,
        Err(e) => format_error(&format!("serialise: {e}")),
    }
}

fn format_error(message: &str) -> String {
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{{\"error\":\"{escaped}\"}}")
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

    async fn run_query(
        &self,
        query_text: &str,
        top_k_override: Option<usize>,
        kind_filter: Option<&str>,
    ) -> anyhow::Result<Vec<ToolChunk>> {
        let top_k = top_k_override.unwrap_or(self.state.config.retrieval.top_k_default);
        let vector = {
            let mut emb = self.state.embedder.lock().await;
            let mut vectors = emb.embed(vec![query_text], None)?;
            vectors
                .pop()
                .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))?
        };
        let records = self
            .state
            .store
            .query_nearest(&vector, top_k, kind_filter)
            .await?;
        Ok(records.into_iter().map(ToolChunk::from).collect())
    }
}
