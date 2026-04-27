//! Port traits — the application's boundary with the outside world.
//!
//! Per ADR-0013, every piece of I/O the app does goes through a trait declared
//! here. Adapters in `crate::adapters` implement these traits using concrete
//! technologies (`LanceDB`, fastembed, notify, walkdir, pulldown-cmark, TOML).
//! The application services in `crate::app` accept `Arc<dyn Trait>` so wiring
//! is the only place concrete types appear.

use std::any::Any;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::domain::{Chunk, ChunkRecord, CorpusEvent, CorpusKind, DiscoveredFile, Metadata};

// ─── Persistence ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("persistence backend error: {0}")]
    Backend(String),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// Vector + chunk store used by the retrieval pipeline.
///
/// The default implementation today is `LanceDB`
/// (`crate::adapters::lancedb_store`). ADR-0011 will swap this for sqlite-vec
/// without touching `crate::app`.
#[async_trait]
pub trait Persistence: Send + Sync {
    /// Open / create whatever underlying storage is needed.
    async fn ensure_ready(&self) -> Result<(), PersistenceError>;

    /// Append rows for a batch of chunks paired with their embeddings.
    async fn append_chunks(
        &self,
        chunks: &[Chunk],
        vectors: &[Vec<f32>],
    ) -> Result<(), PersistenceError>;

    /// Delete every chunk whose `source_path` matches one of `paths`.
    async fn delete_by_source(&self, paths: &[&str]) -> Result<(), PersistenceError>;

    /// Wipe every chunk from the store and reclaim the disk pages.
    ///
    /// Implementations should perform a bulk `DELETE FROM chunks` (which
    /// cascades to companion virtual tables via existing triggers) followed
    /// by `VACUUM` (or the equivalent reclaim step) so the on-disk file does
    /// not retain dead pages. The store file is kept open; only its contents
    /// are emptied.
    ///
    /// # Errors
    /// Returns an error if the underlying delete fails.
    async fn reset_all(&self) -> Result<(), PersistenceError>;

    /// Read the persisted embedding recipe marker (ADR-0029) — `None`
    /// when no recipe row exists yet (fresh store / pre-ADR-0029 store).
    ///
    /// # Errors
    /// Returns an error if the underlying metadata read fails.
    async fn read_embedding_recipe(&self) -> Result<Option<String>, PersistenceError>;

    /// Stamp the persisted embedding recipe marker. Idempotent.
    ///
    /// # Errors
    /// Returns an error if the underlying metadata write fails.
    async fn write_embedding_recipe(&self, recipe: &str) -> Result<(), PersistenceError>;

    /// Top-K nearest-neighbour query.
    ///
    /// `kind_filter` — when present, restricts the search to rows whose
    /// `kind` column matches. Per ADR-0028 this filter is pushed inside
    /// the `sqlite-vec` `MATCH` (via the `kind` partition key) so the
    /// engine returns the top-K **within** the requested kind, not the
    /// global top-K filtered down. Implementations that do not support
    /// partition keys must fall back to over-fetch + post-filter and
    /// document the recall caveat.
    ///
    /// `min_score` — when present, expresses a cosine-similarity floor
    /// (`0.0 = no overlap`, `1.0 = identical`); rows whose distance
    /// exceeds `1.0 - min_score` are dropped before `LIMIT k`. When
    /// absent, every row in top-K is returned regardless of distance
    /// (raw top-K).
    async fn query_nearest(
        &self,
        vector: &[f32],
        k: usize,
        kind_filter: Option<&str>,
        min_score: Option<f32>,
    ) -> Result<Vec<ChunkRecord>, PersistenceError>;

    /// Find every chunk whose `artifact_id` exactly matches the given id.
    async fn find_by_artifact_id(
        &self,
        artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>, PersistenceError>;

    /// Find every chunk whose `content` mentions the given substring.
    async fn find_mentioning(
        &self,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>, PersistenceError>;

    /// List every distinct `source_path` currently in the index.
    async fn list_source_paths(&self) -> Result<Vec<String>, PersistenceError>;
}

// ─── Embedder ────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("embedder backend error: {0}")]
    Backend(String),
}

/// BAAI bge-m3 dense-retrieval query prefix (ADR-0029).
///
/// Prepended to query-side text when the per-project flag
/// `[embedding] query_passage_prefix` is set. When the flag is `false`
/// the adapter ignores this constant and routes raw text to the model.
pub const EMBEDDER_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// BAAI bge-m3 dense-retrieval passage prefix (ADR-0029).
///
/// Prepended to passage-side text under the same flag as
/// [`EMBEDDER_QUERY_PREFIX`]. Mixing prefixed and raw passage embeddings
/// in one store degrades recall, so the flag flip triggers a full
/// per-project re-embed (see [`Persistence`] migration semantics in
/// `crate::adapters::sqlite_vec_store`).
pub const EMBEDDER_PASSAGE_PREFIX: &str = "Represent this passage: ";

/// Text → dense-vector embedding port. The fastembed-backed adapter wraps
/// the synchronous embed call in `tokio::task::spawn_blocking` so callers
/// can `.await` on it without blocking the runtime.
///
/// ADR-0029 splits the original single `embed` method into asymmetric
/// query / passage variants. Callers must pick the right method:
///
/// - **Indexing** (delta-sync, re-embed migrations) → [`embed_passages`].
/// - **Retrieval** (every MCP tool that turns a string into a vector
///   for nearest-neighbour search) → [`embed_query`].
///
/// Misuse — calling [`embed_passages`] for a query, or vice versa —
/// silently degrades recall when the per-project prefix flag is on.
///
/// `with_prefix` is the per-project `[embedding] query_passage_prefix`
/// knob threaded through at call time (rather than captured at
/// construction) so a single shared `Embedder` instance — one ONNX
/// session per workstation per ADR-0026 — can faithfully serve
/// multiple projects whose flag values diverge. The adapter applies
/// the matching prefix internally based on this flag.
///
/// [`embed_passages`]: Embedder::embed_passages
/// [`embed_query`]: Embedder::embed_query
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a single retrieval query. Returns one 1024-dim vector.
    ///
    /// When `with_prefix` is `true`, the fastembed adapter prepends
    /// [`EMBEDDER_QUERY_PREFIX`]; when `false`, the text is passed
    /// through verbatim (status quo before ADR-0029).
    async fn embed_query(
        &mut self,
        text: String,
        with_prefix: bool,
    ) -> Result<Vec<f32>, EmbedError>;

    /// Embed a batch of indexable passages. Returns one vector per input.
    ///
    /// When `with_prefix` is `true`, the fastembed adapter prepends
    /// [`EMBEDDER_PASSAGE_PREFIX`] to every element; when `false`, the
    /// texts pass through verbatim.
    async fn embed_passages(
        &mut self,
        texts: Vec<String>,
        with_prefix: bool,
    ) -> Result<Vec<Vec<f32>>, EmbedError>;
}

// ─── Walker ──────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum WalkerError {
    #[error("corpus path {0} does not exist")]
    PathNotFound(PathBuf),
    #[error("io error walking {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Synchronous filesystem walker that yields every corpus file.
pub trait Walker: Send + Sync {
    /// Visit every file matching every corpus entry. Returns a flat list.
    ///
    /// # Errors
    /// Returns an error if a corpus path is missing or filesystem traversal fails.
    fn discover(&self) -> Result<Vec<DiscoveredFile>, WalkerError>;
}

// ─── Chunker ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ChunkerError {
    #[error("unsupported corpus kind: {0:?}")]
    UnsupportedKind(CorpusKind),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Per-kind chunking strategy port.
pub trait Chunker: Send + Sync {
    /// Read the file at `absolute_path` and split it into chunks per the
    /// strategy associated with `kind`.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or the kind is unsupported.
    fn chunk(
        &self,
        relative_path: &Path,
        absolute_path: &Path,
        kind: CorpusKind,
    ) -> Result<Vec<Chunk>, ChunkerError>;
}

// ─── Watcher ─────────────────────────────────────────────────────────────

/// Owned handle to whatever resource keeps the underlying watcher alive.
/// Holding this alive keeps the watch running. Move it into a long-lived
/// task / struct; drop it to stop the watcher.
///
/// The held value is opaque (`dyn Any + Send`) on purpose — `ports.rs` must
/// not name any concrete adapter type (the `notify` crate, in our current
/// adapter). Adapters construct this via [`WatcherKeepAlive::new`].
pub struct WatcherKeepAlive {
    #[expect(
        dead_code,
        reason = "field exists solely to extend the watcher's lifetime; \
                  dropping this handle drops the adapter-specific watcher \
                  guard inside, which stops the watch"
    )]
    pub(crate) inner: Box<dyn Any + Send>,
}

impl WatcherKeepAlive {
    /// Wrap any adapter-specific guard so its `Drop` runs when this
    /// keep-alive is dropped.
    pub fn new<T: Send + 'static>(guard: T) -> Self {
        Self {
            inner: Box::new(guard),
        }
    }
}

impl fmt::Debug for WatcherKeepAlive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatcherKeepAlive")
            .field("inner", &"<dyn Watcher>")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("watcher backend error: {0}")]
    Backend(String),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// Filesystem-watcher port. `start` consumes the watcher (one-shot
/// activation) and returns a keep-alive plus the event receiver.
pub trait Watcher: Send {
    /// Start the watch. The keep-alive must be retained for the duration of
    /// the watch; dropping it stops event delivery.
    ///
    /// # Errors
    /// Returns an error if the underlying watcher backend cannot be created
    /// or fails to register one of the requested paths.
    fn start(
        self: Box<Self>,
    ) -> Result<(WatcherKeepAlive, mpsc::Receiver<CorpusEvent>), WatcherError>;
}

// ─── MetadataStore ───────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MetadataStoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serialise: {0}")]
    Serialise(String),
    #[error("parse: {0}")]
    Parse(String),
}

/// Persistence for the [`Metadata`] manifest. The default adapter writes TOML
/// to disk; tests can supply an in-memory implementation.
pub trait MetadataStore: Send + Sync {
    /// Load the persisted manifest, returning [`Metadata::default`] when no
    /// file exists yet.
    ///
    /// # Errors
    /// Returns an error if the on-disk manifest cannot be read or parsed.
    fn load(&self) -> Result<Metadata, MetadataStoreError>;

    /// Persist the manifest atomically.
    ///
    /// # Errors
    /// Returns an error if the parent directory cannot be created, the
    /// manifest cannot be serialised, or the file cannot be written.
    fn save(&self, metadata: &Metadata) -> Result<(), MetadataStoreError>;

    /// Overwrite the manifest with [`Metadata::default`].
    ///
    /// Equivalent to `save(&Metadata::default())`; exposed as a dedicated
    /// method so the cleanup use case has a single, intention-revealing
    /// call site that adapters can specialise (e.g. to delete the file
    /// instead of writing an empty one) without changing callers.
    ///
    /// # Errors
    /// Returns an error if the manifest cannot be written.
    fn reset(&self) -> Result<(), MetadataStoreError>;
}

// ─── LlmProvider (ADR-0025) ──────────────────────────────────────────────

/// Outbound port driving a cloud LLM for the `synthesize` MCP tool.
///
/// Implementations live in `crate::adapters::{anthropic_provider,
/// openai_provider}`. Only the composition root knows which concrete
/// adapter is wired; the [`crate::app::synthesize::Synthesize`] use case
/// holds an `Arc<dyn LlmProvider>` and never imports either adapter.
///
/// When no adapter is wired (no `*_API_KEY` set), the MCP `synthesize`
/// tool is hidden from `tools/list` per ADR-0025 §"silent degrade".
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider identifier surfaced in `workspace_context` and
    /// `synthesize` responses (`"anthropic"` / `"openai"`).
    fn name(&self) -> &'static str;

    /// Resolved model id (e.g. `"claude-haiku-4-5-20251001"`,
    /// `"gpt-5"`) used by this provider. May come from `[llm].model`,
    /// `SCHEMA_LLM_MODEL`, or the per-provider compiled-in default.
    fn model(&self) -> &str;

    /// One-shot synthesis call. Maps `req` → provider HTTPS POST →
    /// returns the parsed response.
    ///
    /// Implementations must:
    /// - never log API keys, prompts, or responses at `info` or above
    /// - timeout at 60 s and return `LlmError::Timeout`
    /// - return `LlmError::Unauthorized` on HTTP 401/403
    /// - return `LlmError::Backend` on any other transport failure
    ///
    /// # Errors
    /// Returns the variant of [`LlmError`] matching the failure mode.
    async fn synthesize(&self, req: SynthesisRequest) -> Result<SynthesisResponse, LlmError>;
}

/// Input to [`LlmProvider::synthesize`]. Built by the `Synthesize` use
/// case after retrieving and packing the chunks.
///
/// `temperature` and `max_tokens` are `Option` so the adapter can
/// substitute its own provider-specific default when the operator did
/// not pin a value. ADR-0025 §"Provider-specific defaults" amendment
/// (2026-04-27) moves the defaults into the outbound adapters because
/// each provider's accepted range differs (e.g. gpt-5 rejects
/// `temperature = 0.0` and burns `max_tokens` on reasoning).
#[derive(Debug, Clone)]
pub struct SynthesisRequest {
    /// System prompt — instructs the LLM how to behave (cite-only,
    /// refuse-on-empty-context, prompt-injection defence). Same shape
    /// across providers.
    pub system_prompt: String,
    /// User prompt — the natural-language question plus the
    /// retrieval context block.
    pub user_prompt: String,
    /// Cap on output tokens. `None` ⇒ use the adapter's
    /// `DEFAULT_MAX_TOKENS` constant (Anthropic 1024, `OpenAI` 8192).
    pub max_tokens: Option<u32>,
    /// Sampling temperature. `None` ⇒ use the adapter's
    /// `DEFAULT_TEMPERATURE` constant (Anthropic 0.0, `OpenAI` 1.0).
    pub temperature: Option<f32>,
}

/// Output from [`LlmProvider::synthesize`]. The use case wraps the
/// `answer` field with citation rows derived from the retrieval hits.
#[derive(Debug, Clone)]
pub struct SynthesisResponse {
    /// Free-text answer produced by the LLM.
    pub answer: String,
    /// Resolved model id (echoed for trace/debug; client may rely on
    /// it for caching keys).
    pub model: String,
    /// Tokens billed for the input prompt, when the provider returns
    /// it. `None` when the provider doesn't expose usage in this call.
    pub input_tokens: Option<u32>,
    /// Tokens billed for the output. `None` semantics same as above.
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("llm provider {provider} timed out after {timeout_secs}s")]
    Timeout {
        provider: &'static str,
        timeout_secs: u64,
    },
    #[error("llm provider {provider} rejected the credentials (HTTP {status})")]
    Unauthorized { provider: &'static str, status: u16 },
    #[error("llm provider {provider} backend error: {message}")]
    Backend {
        provider: &'static str,
        message: String,
    },
    #[error("llm provider {provider} returned an unexpected payload: {message}")]
    Decode {
        provider: &'static str,
        message: String,
    },
}

// ─── SecretStore (ADR-0031) ──────────────────────────────────────────────

/// Identifier for a provider whose API key the daemon may need.
///
/// Kept as a small enum (rather than `&str`) so the [`SecretStore`]
/// implementation can match exhaustively and the compiler flags any
/// future provider that gets added without a corresponding lookup
/// branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    /// Anthropic (`ANTHROPIC_API_KEY` legacy env / `[llm.anthropic]`
    /// in `secrets.toml`).
    Anthropic,
    /// `OpenAI` (`OPENAI_API_KEY` legacy env / `[llm.openai]` in
    /// `secrets.toml`).
    OpenAi,
}

impl ProviderId {
    /// Stable string identifier used in `tracing` logs and TOML keys.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

/// Outbound port for fetching daemon-scope secrets (LLM provider keys).
///
/// ADR-0031 introduces this port so the [`LlmProvider`] factory in the
/// composition root no longer reads `ANTHROPIC_API_KEY` /
/// `OPENAI_API_KEY` directly from the process environment. The default
/// adapter ([`crate::adapters::secrets_toml::FileSecretStore`]) reads
/// a mode-`0600` `secrets.toml` next to the daemon's `endpoint.toml`;
/// future adapters may consult macOS Keychain or Linux libsecret.
///
/// Implementations must:
/// - return `Ok(None)` when the secret is simply absent (fast path);
///   never panic or block on missing files
/// - never log the secret value at any level
/// - tolerate concurrent re-reads (`SIGHUP`-driven rotation)
pub trait SecretStore: Send + Sync {
    /// Look up the API key for `provider`. Returns `Ok(None)` when the
    /// store is empty / has no entry for that provider; only
    /// configuration / I/O failures yield `Err`.
    ///
    /// # Errors
    /// Returns an error when the underlying file/keystore is present
    /// but cannot be parsed or accessed (e.g., wrong file mode,
    /// unsupported version, permission-denied on Keychain).
    fn provider_key(&self, provider: ProviderId) -> Result<Option<String>, SecretStoreError>;
}

#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("toml parse: {0}")]
    Parse(String),
    #[error("unsupported secrets.toml version {0} (this build understands version 1)")]
    UnsupportedVersion(u32),
    #[error(
        "secrets.toml has mode {mode:#o} but must be 0600 — \
         run `chmod 600 {path}` (or re-run `schema secrets migrate`)"
    )]
    InsecureMode { path: String, mode: u32 },
}
