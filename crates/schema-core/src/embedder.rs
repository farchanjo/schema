//! `Embedder` outbound port shared between the **schema** and **recall**
//! bounded contexts.
//!
//! ADR-0034 (Cargo workspace migration) extracted this module from
//! `crates/schema/src/ports.rs` so both contexts can implement and consume
//! the same text → dense-vector contract without copying. ADR-0029
//! defines the asymmetric query / passage prefix policy implemented by
//! the [`FastembedEmbedder`](crate::FastembedEmbedder) adapter.

use async_trait::async_trait;
use thiserror::Error;

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
/// per-project re-embed (see schema's `Persistence` adapter migration
/// semantics in `crates/schema/src/adapters/sqlite_vec_store.rs`).
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
    ///
    /// # Errors
    /// Returns [`EmbedError::Backend`] when the backend call fails
    /// (model load failure, OS-level resource exhaustion, etc.).
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
    ///
    /// # Errors
    /// Returns [`EmbedError::Backend`] when the backend call fails.
    async fn embed_passages(
        &mut self,
        texts: Vec<String>,
        with_prefix: bool,
    ) -> Result<Vec<Vec<f32>>, EmbedError>;
}
