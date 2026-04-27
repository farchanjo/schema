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

use std::path::Path;

use serde::de::Unexpected;
use serde_json::Value;

use crate::adapters::auth::BearerValidator;
use crate::app::daemon::Daemon;
use crate::app::project_instance::ProjectInstance;
use crate::app::synthesize::{Citation, SynthesizeOutput};
use crate::domain::ChunkRecord;

/// Permissive deserializer for `Option<usize>` on tool-call parameters
/// (ADR-0032 §"Decision" item 3).
///
/// MCP clients in the wild occasionally coerce numeric arguments to
/// strings — observed on Claude Code's `query` tool dispatch on
/// 2026-04-27, where `top_k: 3` arrived on the wire as
/// `"top_k": "3"` and serde rejected it with `invalid type: string
/// "3", expected usize`. This helper accepts integer or numeric
/// string for every `Option<usize>` field on a `*Params` struct so
/// the integer-override path stays unbroken regardless of which
/// client is wiring us.
///
/// Applied to: `QueryParams.top_k`, `FindDecisionsParams.top_k`,
/// `GlossaryLookupParams.top_k`, `SynthesizeParams.top_k`,
/// `CrossReferenceParams.definition_limit`,
/// `CrossReferenceParams.reference_limit`. Adding a new
/// `Option<usize>` field on a `*Params` struct without this helper
/// reopens the bug class — keep this list in sync.
fn deserialize_optional_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| {
                Error::invalid_value(
                    Unexpected::Other(&n.to_string()),
                    &"a non-negative integer fitting in usize",
                )
            }),
        Some(Value::String(s)) => s.parse::<usize>().map(Some).map_err(|err| {
            Error::custom(format!(
                "expected a usize-parseable string, got {s:?}: {err}"
            ))
        }),
        Some(other) => Err(Error::invalid_type(
            unexpected_for(&other),
            &"integer or numeric string",
        )),
    }
}

fn unexpected_for(v: &Value) -> Unexpected<'_> {
    match v {
        Value::Null => Unexpected::Unit,
        Value::Bool(b) => Unexpected::Bool(*b),
        Value::Number(_) => Unexpected::Other("number"),
        Value::String(s) => Unexpected::Str(s),
        Value::Array(_) => Unexpected::Seq,
        Value::Object(_) => Unexpected::Map,
    }
}

/// One-shot WARN logged the first time the deprecated `query` alias is
/// invoked in a process (ADR-0032 §"Decision" item 2). Mirrors the
/// `Once::call_once` shape used for the env-source secret WARN
/// (ADR-0031 §"Decision" item 6).
fn warn_query_alias_once() {
    use std::sync::Once;
    static WARN_ONCE: Once = Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            "tools: deprecated MCP tool `query` invoked — rewire to `search` \
             (ADR-0032). The `query` alias will be removed on 2026-05-27."
        );
    });
}

/// Empty parameter set — `ping` takes no arguments.
///
/// `ping` is the only project-less tool: it answers "is the daemon
/// up?" and does not need a `working_directory` because it never
/// touches any project's data.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "schemars/serde derive shapes JSON `{}`, not `null`; converting to a unit struct would change MCP request schema"
)]
pub struct PingParams {}

/// Arguments for `workspace_context` — see [`WorkspaceContext`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceContextParams {
    /// Absolute path the LLM is currently working in. The daemon
    /// walks up from this directory looking for a `schema.toml` to
    /// resolve the project (ADR-0027 §"Decision"). Required.
    pub working_directory: String,
}

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
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,

    /// Natural-language question to answer over the indexed corpus.
    /// Example: "What did ADR-0019 decide about transport?".
    pub query: String,

    /// How many retrieval hits to include in the synthesis prompt
    /// before calling the LLM. Default 8. Lower (3-5) for tightly-
    /// focused questions to save tokens; higher (12-16) for
    /// exploratory questions where the answer needs wider context.
    /// Hard-clamped to 1..=16.
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
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
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,

    /// Natural-language query, e.g. 'how does session lifetime work'.
    pub query: String,

    /// Override for K (default 8). Higher = more breadth, lower = more precision.
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    pub top_k: Option<usize>,
}

/// Arguments for `find_decisions` — semantic search restricted to ADR-MADR.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDecisionsParams {
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,

    /// What you want to find decisions about, e.g. 'session lifetime', 'step-up authentication'.
    pub query: String,

    /// Override for K (default 8). Higher = more breadth, lower = more precision.
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    pub top_k: Option<usize>,
}

/// Arguments for `glossary_lookup` — semantic search restricted to glossary kinds.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GlossaryLookupParams {
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,

    /// Term to look up, e.g. 'RBAC' or 'JWT'. Synonyms work.
    pub term: String,

    /// Override for K (default 8). Higher = more breadth, lower = more precision.
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    pub top_k: Option<usize>,
}

/// Arguments for `list_corpus`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCorpusParams {
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,
}

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
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,

    /// Artifact id, e.g. 'ADR-0055' or 'GLOSS-0012'. Exact match required.
    pub artifact_id: String,

    /// Cap on defining chunks (default 16). Definitions are usually 1-3.
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    pub definition_limit: Option<usize>,

    /// Cap on referencing chunks (default 32). Set higher for popular ADRs that are referenced widely.
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    pub reference_limit: Option<usize>,
}

/// Arguments for `reset_index` (ADR-0015 + ADR-0027).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResetIndexParams {
    /// Absolute path the LLM is currently working in. The reset
    /// scopes to the project resolved from this directory — other
    /// projects' indexes are untouched (ADR-0008 isolation
    /// preserved).
    pub working_directory: String,
}

/// Arguments for `forget_source` — see ADR-0015.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetSourceParams {
    /// Absolute path the LLM is currently working in. See
    /// [`WorkspaceContextParams::working_directory`].
    pub working_directory: String,

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

/// MCP server instance — workstation-level under ADR-0027.
///
/// Holds an `Arc<Daemon>` (the daemon owns the shared `Embedder`,
/// optional shared `LlmProvider`, and the lazy
/// `Map<ProjectId, Arc<ProjectInstance>>`). Each tool call carries
/// a `working_directory` parameter the handler walks up via
/// [`Daemon::resolve_or_wire`] to dispatch to the matching project.
///
/// Cloning is cheap (clones the `Arc`); rmcp clones the server per
/// request.
#[derive(Clone, Debug)]
pub struct SchemaServer {
    daemon: Arc<Daemon>,
}

#[tool_router(server_handler)]
impl SchemaServer {
    /// Reports project context resolved from `working_directory`,
    /// so the LLM (Claude Code) can confirm the daemon picked up
    /// the right `schema.toml` (ADR-0009 amendment, motivated by
    /// ADR-0027 walk-up resolution).
    #[tool(
        description = "Returns project context resolved from the supplied working_directory: project name + id + version, project root path, cache directory, registered corpus paths, and embedding model. Useful as a sanity check at session start ('what am I connected to?') and for confirming the daemon walked up to the right schema.toml."
    )]
    async fn workspace_context(
        &self,
        Parameters(params): Parameters<WorkspaceContextParams>,
    ) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        json_or_error(&build_workspace_context(&project))
    }

    /// Smoke-test tool. Returns the literal string `"pong"`. The only
    /// project-less tool — useful as a daemon liveness probe before
    /// the LLM has a `working_directory` to supply.
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
    /// closest chunks. ADR-0032 canonical name.
    #[tool(
        description = "Semantic search across the project corpus resolved from working_directory. Use for general questions when you don't know the kind, or when you want hits across ADRs, glossary, and prose at once. Example queries: 'how does session lifetime work', 'what is the chunking strategy for markdown'. Replaces the deprecated `query` tool — same shape, clearer name (ADR-0032)."
    )]
    async fn search(&self, Parameters(params): Parameters<QueryParams>) -> String {
        self.run_search_inner(params).await
    }

    /// Deprecated alias — kept registered until the 2026-05-27 cutover so
    /// consumer-side wiring that still names `query` keeps working.
    /// Forwards to `search`. Emits a one-shot WARN per process on first
    /// invocation (ADR-0032 §"Decision" item 2).
    #[tool(
        description = "**Deprecated**: use `search`. Retained until 2026-05-27 for wiring compatibility (ADR-0032). Same shape and semantics as `search`."
    )]
    async fn query(&self, Parameters(params): Parameters<QueryParams>) -> String {
        warn_query_alias_once();
        self.run_search_inner(params).await
    }

    async fn run_search_inner(&self, params: QueryParams) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        let top_k = params
            .top_k
            .unwrap_or(project.config.retrieval.top_k_default);
        match project.query.run(&params.query, top_k, None).await {
            Ok(records) => {
                let chunks: Vec<ToolChunk> = records.into_iter().map(ToolChunk::from).collect();
                json_or_error(&chunks)
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// Semantic search restricted to ADRs (kind = adr-madr).
    #[tool(
        description = "Semantic search restricted to architectural decisions (ADRs) within the project resolved from working_directory. Use to retrieve a decision and its rationale. Returns the ADR body plus surrounding context. Example queries: 'why did we pick LanceDB', 'what is our session lifetime policy'."
    )]
    async fn find_decisions(&self, Parameters(params): Parameters<FindDecisionsParams>) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        let top_k = params
            .top_k
            .unwrap_or(project.config.retrieval.top_k_default);
        match project
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
        description = "Look up a term in the project glossary (project resolved from working_directory). Match is semantic, so synonyms and related phrases surface — you don't need the exact word the glossary uses. Examples: 'RBAC' → 'role-based access control'. 'JWT' → 'JSON Web Token'."
    )]
    async fn glossary_lookup(
        &self,
        Parameters(params): Parameters<GlossaryLookupParams>,
    ) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        let top_k = params
            .top_k
            .unwrap_or(project.config.retrieval.top_k_default);
        match project
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
        description = "Given an artifact id (e.g. 'ADR-0055') in the project resolved from working_directory, return its defining chunks PLUS every chunk elsewhere in the corpus that references it. Useful for impact analysis: 'what depends on this decision'. Example: artifact_id='ADR-0055' → returns the ADR's own body and every other chunk that mentions ADR-0055 inline."
    )]
    async fn cross_reference(
        &self,
        Parameters(params): Parameters<CrossReferenceParams>,
    ) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        let def_limit = params.definition_limit.unwrap_or(16);
        let ref_limit = params.reference_limit.unwrap_or(32);

        match project
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
        description = "Debug — list every source path currently in the index for the project resolved from working_directory. Useful for verifying that schema.toml corpus paths expanded into the files you expected."
    )]
    async fn list_corpus(&self, Parameters(params): Parameters<ListCorpusParams>) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        match project.query.list_source_paths().await {
            Ok(paths) => {
                let listing = CorpusListing {
                    project: project.config.project.name.clone(),
                    source_paths: paths,
                };
                json_or_error(&listing)
            }
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// DESTRUCTIVE — wipe every chunk + the manifest for this project.
    /// Scoped strictly to the project resolved from `working_directory`;
    /// other projects' indexes are untouched (ADR-0008 isolation,
    /// ADR-0027 §"Decision drivers").
    #[tool(
        description = "DESTRUCTIVE — wipe every chunk and reset the manifest for the project resolved from working_directory. Ask the operator to confirm before calling. Use after a chunking strategy change or model swap when a full re-index is wanted. Next tool call against this project rebuilds from scratch. Other projects' indexes are untouched."
    )]
    async fn reset_index(&self, Parameters(params): Parameters<ResetIndexParams>) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        match project.cleanup.reset_index().await {
            Ok(()) => json_or_error(&serde_json::json!({
                "status": "ok",
                "action": "reset_index",
                "project_id": project.identity.id.to_string(),
            })),
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// DESTRUCTIVE — drop chunks for one source path + drop it from the
    /// manifest.
    #[tool(
        description = "DESTRUCTIVE — drop every chunk for one source path from the index of the project resolved from working_directory. The file on disk is NOT deleted; only its chunks vanish from the index. Use when a doc has gone stale or noisy. Example: path='docs/decisions/0042-deprecated.md' removes only that file's chunks."
    )]
    async fn forget_source(&self, Parameters(params): Parameters<ForgetSourceParams>) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        match project.cleanup.forget_source(&params.path).await {
            Ok(()) => json_or_error(&serde_json::json!({
                "status": "ok",
                "action": "forget_source",
                "project_id": project.identity.id.to_string(),
                "path": params.path,
            })),
            Err(e) => format_error(&format!("{e}")),
        }
    }

    /// `synthesize` — RAG answering over the indexed corpus (ADR-0025).
    ///
    /// Description must follow ADR-0016 style: action verb opening +
    /// concrete example + differentiation hint + availability note.
    /// Tool count is 9-or-10: when no `*_API_KEY` is set, the call
    /// returns an error envelope; `workspace_context.llm.active` is
    /// the canonical signal the calling LLM checks before invoking.
    #[tool(
        description = "Answer a natural-language question over the indexed corpus of the project resolved from working_directory by retrieving the top semantically-similar chunks and composing a cited answer through a configured cloud LLM (Anthropic Claude or OpenAI GPT). Returns {answer, citations[], model, usage}; each citation carries source_path, line range, and optional artifact_id (e.g. \"ADR-0019\"). Differs from `query`, `find_decisions`, `glossary_lookup` (which return raw chunks for the **calling** LLM to read) — use `synthesize` when the caller wants a ready answer, not chunks; use the retrieval tools when the caller wants to read the source material directly. Requires a provider API key (ANTHROPIC_API_KEY or OPENAI_API_KEY) at daemon startup; check `workspace_context.llm.active = true` before calling. When disabled, this tool returns an error envelope explaining how to enable it."
    )]
    async fn synthesize(&self, Parameters(params): Parameters<SynthesizeParams>) -> String {
        let project = match self.resolve(&params.working_directory).await {
            Ok(project) => project,
            Err(message) => return format_error(&message),
        };
        let Some(synth) = project.synthesize.as_ref() else {
            return format_error(
                "synthesize is disabled: set ANTHROPIC_API_KEY or OPENAI_API_KEY \
                 (or [llm] provider = \"anthropic\" / \"openai\" with the \
                 matching key) and restart the schema daemon.",
            );
        };
        let top_k = params.top_k.unwrap_or(8).clamp(1, 16);
        match synth.run(&params.query, top_k).await {
            Ok(output) => json_or_error(&SynthesizeResult::from(output)),
            Err(e) => format_error(&format!("{e}")),
        }
    }
}

impl SchemaServer {
    /// Resolve `working_directory` to a wired [`ProjectInstance`],
    /// returning a string the handler can pipe straight into
    /// [`format_error`] when resolution fails.
    async fn resolve(&self, working_directory: &str) -> Result<Arc<ProjectInstance>, String> {
        let path = Path::new(working_directory);
        self.daemon
            .resolve_or_wire(path)
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

fn build_workspace_context(project: &ProjectInstance) -> WorkspaceContext {
    WorkspaceContext {
        project: build_project_context(project),
        corpus: build_corpus_entries(project),
        embedding: build_embedding_context(project),
        llm: build_llm_context(project),
    }
}

fn build_project_context(project: &ProjectInstance) -> ProjectContext {
    ProjectContext {
        name: project.config.project.name.clone(),
        id: project.identity.id.to_string(),
        version: project.config.project.version.clone(),
        root: project.identity.root.display().to_string(),
        cache_dir: project.identity.cache_dir.display().to_string(),
    }
}

fn build_corpus_entries(project: &ProjectInstance) -> Vec<CorpusEntry> {
    project
        .config
        .corpus
        .iter()
        .map(|c| CorpusEntry {
            path: c.path.display().to_string(),
            kind: format!("{:?}", c.kind),
        })
        .collect()
}

fn build_embedding_context(project: &ProjectInstance) -> EmbeddingContext {
    use schema_core::fastembed_embedder::BGE_M3_DIMENSIONS;
    EmbeddingContext {
        model: project.config.embedding.model.clone(),
        dims: BGE_M3_DIMENSIONS,
    }
}

fn build_llm_context(project: &ProjectInstance) -> LlmContext {
    project.synthesize.as_ref().map_or(
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
    )
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
    /// Construct a server backed by the supplied [`Daemon`]. The daemon
    /// owns the shared `Embedder` + optional shared `LlmProvider` and
    /// the lazy `Map<ProjectId, Arc<ProjectInstance>>`. Every tool
    /// handler dispatches per-call via `daemon.resolve_or_wire`.
    #[must_use]
    pub const fn with_daemon(daemon: Arc<Daemon>) -> Self {
        Self { daemon }
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Envelope {
        #[serde(default, deserialize_with = "deserialize_optional_usize")]
        top_k: Option<usize>,
    }

    fn parse(raw: &str) -> Result<Envelope, serde_json::Error> {
        serde_json::from_str(raw)
    }

    #[test]
    fn deserialize_optional_usize_accepts_integer() {
        assert_eq!(parse(r#"{"top_k":3}"#).unwrap().top_k, Some(3));
    }

    #[test]
    fn deserialize_optional_usize_accepts_numeric_string() {
        assert_eq!(parse(r#"{"top_k":"3"}"#).unwrap().top_k, Some(3));
    }

    #[test]
    fn deserialize_optional_usize_accepts_null() {
        assert_eq!(parse(r#"{"top_k":null}"#).unwrap().top_k, None);
    }

    #[test]
    fn deserialize_optional_usize_accepts_absent() {
        assert_eq!(parse("{}").unwrap().top_k, None);
    }

    #[test]
    fn deserialize_optional_usize_rejects_non_numeric_string() {
        assert!(parse(r#"{"top_k":"abc"}"#).is_err());
    }

    #[test]
    fn deserialize_optional_usize_rejects_negative_numeric_string() {
        assert!(parse(r#"{"top_k":"-1"}"#).is_err());
    }

    #[test]
    fn deserialize_optional_usize_rejects_boolean() {
        assert!(parse(r#"{"top_k":true}"#).is_err());
    }

    #[test]
    fn deserialize_optional_usize_rejects_array() {
        assert!(parse(r#"{"top_k":[3]}"#).is_err());
    }

    #[test]
    fn deserialize_optional_usize_rejects_object() {
        assert!(parse(r#"{"top_k":{"value":3}}"#).is_err());
    }

    #[test]
    fn deserialize_optional_usize_rejects_negative_integer() {
        // serde_json represents -1 as Number; as_u64 returns None.
        assert!(parse(r#"{"top_k":-1}"#).is_err());
    }
}
