//! `SchemaServer` — driving adapter that exposes [`crate::app`] services
//! over the MCP protocol via rmcp + stdio.
//!
//! Tools are registered via the `#[tool_router]` macro attribute. Each tool is
//! an `async fn` (or sync `fn`) on the server impl block decorated with
//! `#[tool(description = "...")]`.
//!
//! FASE 1.0 exposes:
//!   - `ping`             smoke test
//!   - `query`            generic top-K RAG
//!   - `find_decisions`   query restricted to kind = adr-madr
//!   - `glossary_lookup`  query restricted to kind = glossary
//!   - `cross_reference`  `artifact_id` → referencing chunks
//!   - `list_corpus`      debug listing
//!   - `reset_index`      DESTRUCTIVE — wipe index + manifest (ADR-0015)
//!   - `forget_source`    DESTRUCTIVE — drop one source path (ADR-0015)

use std::sync::Arc;

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::adapters::project_identity::ProjectIdentity;
use crate::adapters::toml_config::SchemaConfig;
use crate::app::cleanup::Cleanup;
use crate::app::query::Query;
use crate::domain::ChunkRecord;

/// Empty parameter set — `ping` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "schemars/serde derive shapes JSON `{}`, not `null`; converting to a unit struct would change MCP request schema"
)]
pub struct PingParams {}

/// Arguments for the generic `query` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Natural-language query, e.g. 'how does session lifetime work'.
    pub query: String,

    /// Override for K (default 8). Higher = more breadth, lower = more precision.
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// Arguments for `find_decisions` — semantic search restricted to ADR-MADR.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDecisionsParams {
    /// What you want to find decisions about, e.g. 'session lifetime', 'step-up authentication'.
    pub query: String,

    /// Override for K (default 8). Higher = more breadth, lower = more precision.
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// Arguments for `glossary_lookup` — semantic search restricted to glossary kinds.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GlossaryLookupParams {
    /// Term to look up, e.g. 'RBAC' or 'JWT'. Synonyms work.
    pub term: String,

    /// Override for K (default 8). Higher = more breadth, lower = more precision.
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// Empty parameter set — `list_corpus` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "schemars/serde derive shapes JSON `{}`, not `null`; converting to a unit struct would change MCP request schema"
)]
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
    /// Artifact id, e.g. 'ADR-0055' or 'GLOSS-0012'. Exact match required.
    pub artifact_id: String,

    /// Cap on defining chunks (default 16). Definitions are usually 1-3.
    #[serde(default)]
    pub definition_limit: Option<usize>,

    /// Cap on referencing chunks (default 32). Set higher for popular ADRs that are referenced widely.
    #[serde(default)]
    pub reference_limit: Option<usize>,
}

/// Empty parameter set — `reset_index` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "schemars/serde derive shapes JSON `{}`, not `null`; converting to a unit struct would change MCP request schema"
)]
pub struct ResetIndexParams {}

/// Arguments for `forget_source` — see ADR-0015.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetSourceParams {
    /// Source path to drop, relative to the project root. Format matches `list_corpus` output. Example: 'docs/decisions/0042.md'.
    pub path: String,
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
///
/// Holds the read-side [`Query`] service plus the destructive
/// [`Cleanup`] service plus project metadata. The mutating
/// [`crate::app::delta_sync::DeltaSync`] still belongs to the watcher task
/// spawned in `main`; the MCP layer holds only the cleanup verbs that need
/// to be reachable from the LLM (see ADR-0015).
#[derive(Debug)]
pub struct ServerState {
    pub config: SchemaConfig,
    pub identity: ProjectIdentity,
    pub query: Query,
    pub cleanup: Cleanup,
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
        info!("ping tool invoked");
        "pong".to_string()
    }

    /// Generic semantic-search tool. Embeds the query string, returns top-K
    /// closest chunks.
    #[tool(
        description = "Semantic search across the whole project corpus. Use for general questions when you don't know the kind, or when you want hits across ADRs, glossary, and prose at once. Example queries: 'how does session lifetime work', 'what is the chunking strategy for markdown'."
    )]
    async fn query(&self, Parameters(params): Parameters<QueryParams>) -> String {
        let top_k = params
            .top_k
            .unwrap_or(self.state.config.retrieval.top_k_default);
        match self.state.query.run(&params.query, top_k, None).await {
            Ok(records) => {
                let chunks: Vec<ToolChunk> = records.into_iter().map(ToolChunk::from).collect();
                json_or_error(&chunks)
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Semantic search restricted to ADRs (kind = adr-madr).
    #[tool(
        description = "Semantic search restricted to architectural decisions (ADRs). Use to retrieve a decision and its rationale. Returns the ADR body plus surrounding context. Example queries: 'why did we pick LanceDB', 'what is our session lifetime policy'."
    )]
    async fn find_decisions(&self, Parameters(params): Parameters<FindDecisionsParams>) -> String {
        let top_k = params
            .top_k
            .unwrap_or(self.state.config.retrieval.top_k_default);
        match self
            .state
            .query
            .run(&params.query, top_k, Some("AdrMadr"))
            .await
        {
            Ok(records) => {
                let chunks: Vec<ToolChunk> = records.into_iter().map(ToolChunk::from).collect();
                json_or_error(&chunks)
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Term lookup restricted to glossary entries.
    #[tool(
        description = "Look up a term in the project glossary. Match is semantic, so synonyms and related phrases surface — you don't need the exact word the glossary uses. Examples: 'RBAC' → 'role-based access control'. 'JWT' → 'JSON Web Token'."
    )]
    async fn glossary_lookup(
        &self,
        Parameters(params): Parameters<GlossaryLookupParams>,
    ) -> String {
        let top_k = params
            .top_k
            .unwrap_or(self.state.config.retrieval.top_k_default);
        match self
            .state
            .query
            .run(&params.term, top_k, Some("Glossary"))
            .await
        {
            Ok(records) => {
                let chunks: Vec<ToolChunk> = records.into_iter().map(ToolChunk::from).collect();
                json_or_error(&chunks)
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Cross-reference: artifact id → definition + referencing chunks.
    #[tool(
        description = "Given an artifact id (e.g. 'ADR-0055'), return its defining chunks PLUS every chunk elsewhere in the corpus that references it. Useful for impact analysis: 'what depends on this decision'. Example: artifact_id='ADR-0055' → returns the ADR's own body and every other chunk that mentions ADR-0055 inline."
    )]
    async fn cross_reference(
        &self,
        Parameters(params): Parameters<CrossReferenceParams>,
    ) -> String {
        let def_limit = params.definition_limit.unwrap_or(16);
        let ref_limit = params.reference_limit.unwrap_or(32);

        match self
            .state
            .query
            .cross_reference(&params.artifact_id, def_limit, ref_limit)
            .await
        {
            Ok(xref) => {
                let definition = xref.definition.into_iter().map(ToolChunk::from).collect();
                let references = xref.references.into_iter().map(ToolChunk::from).collect();
                json_or_error(&CrossReferenceResult {
                    definition,
                    references,
                })
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Debug — list every distinct source path indexed for this project.
    #[tool(
        description = "Debug — list every source path currently in the project's index. Useful for verifying that schema.toml corpus paths expanded into the files you expected. No arguments."
    )]
    async fn list_corpus(&self, _params: Parameters<ListCorpusParams>) -> String {
        match self.state.query.list_source_paths().await {
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

    /// DESTRUCTIVE — wipe every chunk + the manifest for this project.
    #[tool(
        description = "DESTRUCTIVE — wipe every chunk and reset the manifest for this project. Ask the operator to confirm before calling. Use after a chunking strategy change or model swap when a full re-index is wanted. Next `schema serve` rebuilds from scratch (~30-60s for a typical corpus)."
    )]
    async fn reset_index(&self, _params: Parameters<ResetIndexParams>) -> String {
        match self.state.cleanup.reset_index().await {
            Ok(()) => json_or_error(&serde_json::json!({
                "status": "ok",
                "action": "reset_index",
            })),
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// DESTRUCTIVE — drop chunks for one source path + drop it from the
    /// manifest.
    #[tool(
        description = "DESTRUCTIVE — drop every chunk for one source path from the index. The file on disk is NOT deleted; only its chunks vanish from the index. Use when a doc has gone stale or noisy. Example: path='docs/decisions/0042-deprecated.md' removes only that file's chunks."
    )]
    async fn forget_source(&self, Parameters(params): Parameters<ForgetSourceParams>) -> String {
        match self.state.cleanup.forget_source(&params.path).await {
            Ok(()) => json_or_error(&serde_json::json!({
                "status": "ok",
                "action": "forget_source",
                "path": params.path,
            })),
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
    #[must_use]
    pub fn with_state(state: ServerState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }

    /// Run the MCP server over stdio until the client disconnects.
    ///
    /// # Errors
    /// Returns an error if rmcp fails to bind the stdio transport or the
    /// service exits abnormally.
    pub async fn run_stdio(self) -> Result<()> {
        info!("starting schema MCP server (stdio transport)");
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        info!("schema MCP server shut down cleanly");
        Ok(())
    }
}
