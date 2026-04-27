//! `SchemaServer` — driving adapter that exposes [`crate::app`] services
//! over the MCP protocol via rmcp's Streamable HTTP transport (ADR-0019).
//!
//! Tools are registered via the `#[tool_router]` macro attribute. Each tool is
//! an `async fn` (or sync `fn`) on the server impl block decorated with
//! `#[tool(description = "...")]`. The HTTP transport is mounted by
//! [`build_router`] which composes [`StreamableHttpService`] from rmcp with
//! the `axum` router, the bearer-token auth layer (ADR-0021), the
//! sensitive-headers redaction layer, and a `TraceLayer` for request spans.
//!
//! FASE 1.0 exposes:
//!   - `ping`               smoke test
//!   - `workspace_context`  "where am I?" — project + corpus + embedding (ADR-0009 amendment)
//!   - `query`              generic top-K RAG
//!   - `find_decisions`     query restricted to kind = adr-madr
//!   - `glossary_lookup`    query restricted to kind = glossary
//!   - `cross_reference`    `artifact_id` → referencing chunks
//!   - `list_corpus`        debug listing
//!   - `reset_index`        DESTRUCTIVE — wipe index + manifest (ADR-0015)
//!   - `forget_source`      DESTRUCTIVE — drop one source path (ADR-0015)

use std::iter;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use http::header;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::trace::TraceLayer;
use tower_http::validate_request::ValidateRequestHeaderLayer;
use tracing::info;

use crate::adapters::auth::BearerValidator;
use crate::adapters::project_identity::ProjectIdentity;
use crate::adapters::toml_config::SchemaConfig;
use crate::app::cleanup::Cleanup;
use crate::app::query::Query;
use crate::app::synthesize::{Citation, Synthesize, SynthesizeOutput};
use crate::domain::ChunkRecord;

/// Empty parameter set — `ping` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "schemars/serde derive shapes JSON `{}`, not `null`; converting to a unit struct would change MCP request schema"
)]
pub struct PingParams {}

/// Empty parameter set — `workspace_context` takes no arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "schemars/serde derive shapes JSON `{}`, not `null`; converting to a unit struct would change MCP request schema"
)]
pub struct WorkspaceContextParams {}

/// Reply for `workspace_context` — answers "where am I?".
///
/// The LLM does not have to guess project boundaries (ADR-0009 amendment,
/// motivated by ADR-0023 walk-up + ENV overlay making the active config
/// non-obvious). Per ADR-0025 the `llm` slice tells the calling LLM
/// whether the `synthesize` MCP tool is wired (provider + model) or
/// disabled (no API key configured).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WorkspaceContext {
    pub project: ProjectContext,
    pub corpus: Vec<CorpusEntry>,
    pub embedding: EmbeddingContext,
    pub llm: LlmContext,
}

/// LLM provider availability surfaced by `workspace_context`.
/// `active = false` ⇒ `synthesize` is registered but disabled and
/// will return an error if called (ADR-0025 §"silent degrade evidence").
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LlmContext {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Project identity slice surfaced by `workspace_context`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectContext {
    pub name: String,
    pub id: String,
    pub version: String,
    pub root: String,
    pub cache_dir: String,
}

/// One entry from `[[corpus]]` in `schema.toml`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CorpusEntry {
    pub path: String,
    pub kind: String,
}

/// Embedding model + output dimension surfaced by `workspace_context`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EmbeddingContext {
    pub model: String,
    pub dims: usize,
}

/// Arguments for the `synthesize` MCP tool (ADR-0025).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SynthesizeParams {
    /// Natural-language question to answer over the indexed corpus.
    /// Example: "What did ADR-0019 decide about transport?".
    pub query: String,

    /// How many retrieval hits to include in the synthesis prompt
    /// before calling the LLM. Default 8. Lower (3-5) for tightly-
    /// focused questions to save tokens; higher (12-16) for
    /// exploratory questions where the answer needs wider context.
    /// Hard-clamped to 1..=16.
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// Reply shape for `synthesize` (ADR-0025).
///
/// Mirrors `crate::app::synthesize::SynthesizeOutput` 1-to-1; declared
/// in this adapter so the JSON schema sits next to the rmcp tool
/// registration and ADR-0016 lives close to the schema.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SynthesizeResult {
    pub answer: String,
    pub model: String,
    pub citations: Vec<SynthesizeCitation>,
    pub usage: SynthesizeUsage,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SynthesizeCitation {
    pub source_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    pub line_start: i32,
    pub line_end: i32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SynthesizeUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
}

impl From<SynthesizeOutput> for SynthesizeResult {
    fn from(value: SynthesizeOutput) -> Self {
        let citations = value.citations.into_iter().map(Into::into).collect();
        let usage = SynthesizeUsage {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
        };
        Self {
            answer: value.answer,
            model: value.model,
            citations,
            usage,
        }
    }
}

impl From<Citation> for SynthesizeCitation {
    fn from(value: Citation) -> Self {
        Self {
            source_path: value.source_path,
            artifact_id: value.artifact_id,
            line_start: value.line_start,
            line_end: value.line_end,
        }
    }
}

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
    /// `Some` when ADR-0025 [`LlmProvider`](crate::ports::LlmProvider) is
    /// wired (Anthropic / `OpenAI`); `None` triggers the `synthesize`
    /// runtime-disabled path.
    pub synthesize: Option<Synthesize>,
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
    /// Reports the project this MCP server is bound to, so the LLM
    /// (Claude Code) can orient itself when ENV overrides or
    /// `.mcp.json` misconfiguration would otherwise leave it
    /// guessing (ADR-0009 amendment, motivated by ADR-0023).
    #[tool(
        description = "Returns project context for this schema MCP server: project name + id + version, project root path, cache directory, registered corpus paths, and embedding model. Useful as a sanity check at session start ('what am I connected to?') and for debugging .mcp.json wiring. No arguments."
    )]
    async fn workspace_context(&self, _params: Parameters<WorkspaceContextParams>) -> String {
        use crate::adapters::fastembed_embedder::BGE_M3_DIMENSIONS;

        let state = &self.state;
        let project = ProjectContext {
            name: state.config.project.name.clone(),
            id: state.identity.id.to_string(),
            version: state.config.project.version.clone(),
            root: state.identity.root.display().to_string(),
            cache_dir: state.identity.cache_dir.display().to_string(),
        };
        let corpus: Vec<CorpusEntry> = state
            .config
            .corpus
            .iter()
            .map(|c| CorpusEntry {
                path: c.path.display().to_string(),
                kind: format!("{:?}", c.kind),
            })
            .collect();
        let embedding = EmbeddingContext {
            model: state.config.embedding.model.clone(),
            dims: BGE_M3_DIMENSIONS,
        };
        let llm = state.synthesize.as_ref().map_or(
            LlmContext {
                active: false,
                provider: None,
                model: None,
            },
            |synth| LlmContext {
                active: true,
                provider: Some(synth.provider_name().to_string()),
                model: Some(synth.model()),
            },
        );
        json_or_error(&WorkspaceContext {
            project,
            corpus,
            embedding,
            llm,
        })
    }

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

    /// `synthesize` — RAG answering over the indexed corpus (ADR-0025).
    ///
    /// Description must follow ADR-0016 style: action verb opening +
    /// concrete example + differentiation hint + availability note.
    /// Tool count is 8-or-9: when no `*_API_KEY` is set, the call
    /// returns an error envelope; `workspace_context.llm.active` is
    /// the canonical signal the calling LLM checks before invoking.
    #[tool(
        description = "Answer a natural-language question over the indexed corpus by retrieving the top semantically-similar chunks and composing a cited answer through a configured cloud LLM (Anthropic Claude or OpenAI GPT). Returns {answer, citations[], model, usage}; each citation carries source_path, line range, and optional artifact_id (e.g. \"ADR-0019\"). Example call: synthesize {\"query\": \"How does ADR-0019 handle session liveness?\", \"top_k\": 6} → narrative answer plus 2-3 citations into the actual ADR file. Differs from `query`, `find_decisions`, `glossary_lookup` (which return raw chunks for the **calling** LLM to read) — use `synthesize` when the caller wants a ready answer, not chunks; use the retrieval tools when the caller wants to read the source material directly. Requires a provider API key (ANTHROPIC_API_KEY or OPENAI_API_KEY) at server startup; check `workspace_context.llm.active = true` before calling. When disabled, this tool returns an error envelope explaining how to enable it. One outbound HTTPS call to the configured provider per invocation."
    )]
    async fn synthesize(&self, Parameters(params): Parameters<SynthesizeParams>) -> String {
        let Some(synth) = self.state.synthesize.as_ref() else {
            return format_error(
                "synthesize is disabled: set ANTHROPIC_API_KEY or OPENAI_API_KEY \
                 (or [llm] provider = \"anthropic\" / \"openai\" with the \
                 matching key) and restart schema serve.",
            );
        };
        let top_k = params.top_k.unwrap_or(8).clamp(1, 16);
        match synth.run(&params.query, top_k).await {
            Ok(output) => json_or_error(&SynthesizeResult::from(output)),
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
}

/// Trivial `200 OK` health probe handler.
///
/// Mounted at `GET /health` *without* the bearer-auth layer so external
/// supervisors (launchd, systemd, smoke scripts) can probe liveness without
/// needing the token. Body is empty by design — the endpoint must not leak
/// project name, token, or any cache path.
async fn health_handler() -> &'static str {
    ""
}

/// Compose the full `axum::Router` for the HTTP MCP transport (ADR-0019).
///
/// Layout:
/// - `POST /mcp` mounts rmcp's `StreamableHttpService`, gated by
///   [`BearerValidator`] (ADR-0021).
/// - `GET /health` unauthenticated liveness probe.
///
/// Cross-cutting layers (applied to the *whole* router so trace spans cover
/// `/health` too):
/// - [`SetSensitiveRequestHeadersLayer`] marks `Authorization` sensitive,
///   so [`TraceLayer`] never serialises the bearer token to logs.
/// - [`TraceLayer::new_for_http`] emits one `tracing` span per request.
///
/// `cancellation_token` is the token rmcp's session loop watches; cancelling
/// it from the caller (graceful SIGTERM) drains in-flight sessions.
pub fn build_router(
    server: SchemaServer,
    token: String,
    cancellation_token: CancellationToken,
) -> Router {
    info!("building HTTP MCP router (Streamable HTTP transport, ADR-0019)");

    // ADR-0019 §"Session liveness" — values match
    // `StreamableHttpServerConfig::default()` from rmcp 1.5; pinned
    // explicitly here so a reader sees the contract without diving into
    // the rmcp source. Changing any of these is a behaviour change that
    // belongs in the ADR.
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancellation_token)
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        .with_sse_retry(Some(Duration::from_secs(3)))
        .with_stateful_mode(true)
        .with_allowed_hosts(["localhost", "127.0.0.1", "::1"]);
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let mcp_router =
        Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(ValidateRequestHeaderLayer::custom(BearerValidator::new(
                token,
            )));

    Router::new()
        .merge(mcp_router)
        .route("/health", get(health_handler))
        .layer(SetSensitiveRequestHeadersLayer::new(iter::once(
            header::AUTHORIZATION,
        )))
        .layer(TraceLayer::new_for_http())
}
