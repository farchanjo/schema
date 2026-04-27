//! `bge-m3` embedder via fastembed 5.x (uses ONNX Runtime under the hood).
//!
//! BGE-M3 produces 1024-dimensional dense embeddings. fastembed downloads the
//! ONNX model from Hugging Face Hub on first use and caches it in a directory
//! the caller supplies; this crate is bounded-context-agnostic, so each
//! consumer (schema, recall) chooses where to cache.
//!
//! Per ADR-0013 + ADR-0034 this adapter implements the [`Embedder`] async
//! trait. Because `fastembed::TextEmbedding::embed` is synchronous and
//! CPU-bound, the implementation hops to `tokio::task::spawn_blocking` so
//! the Tokio runtime remains responsive while ONNX is computing.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use thiserror::Error;
use tokio::task;
use tracing::{debug, info};

use crate::embedder::{EMBEDDER_PASSAGE_PREFIX, EMBEDDER_QUERY_PREFIX, EmbedError, Embedder};

/// Output dimension of BGE-M3.
pub const BGE_M3_DIMENSIONS: usize = 1024;

#[derive(Debug, Error)]
pub enum FastembedEmbedderError {
    #[error("fastembed initialisation failed: {0}")]
    Init(String),
    #[error("embedding failed: {0}")]
    Embed(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Embedder wrapping a fastembed `TextEmbedding` instance.
///
/// Construction triggers the model download on first run; subsequent
/// constructions reuse the cached model file. Wrapped in an `Option` so the
/// async trait impl can move the model into `spawn_blocking` and put it back.
///
/// The `with_prefix` flag is threaded through at call time (ADR-0029)
/// rather than held by the adapter, so the daemon's single shared
/// embedder (ADR-0026) faithfully serves projects whose flag values
/// diverge. When `with_prefix` is `false` the adapter routes raw text
/// through to the model (status quo); when `true` it prepends the BAAI
/// bge-m3 query / passage prefix matching the call.
pub struct FastembedEmbedder {
    model: Option<TextEmbedding>,
}

impl fmt::Debug for FastembedEmbedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FastembedEmbedder")
            .field("model", &"<fastembed::TextEmbedding bge-m3>")
            .finish()
    }
}

impl FastembedEmbedder {
    /// Initialise the BGE-M3 embedder.
    ///
    /// `cache_dir` is where fastembed downloads and stores the ONNX model
    /// weights. The caller is responsible for choosing a stable path
    /// (typically a subdirectory of the consumer's cache root):
    ///
    /// - **schema** uses `~/Library/Caches/schema/models/` on macOS.
    /// - **recall** uses `~/Library/Caches/recall/models/` on macOS
    ///   (ADR-0033 + ADR-0035 path resolution).
    ///
    /// The directory is created if it does not exist.
    ///
    /// # Errors
    /// Returns [`FastembedEmbedderError::Io`] if the cache directory
    /// cannot be created and [`FastembedEmbedderError::Init`] if
    /// fastembed fails to load the BGE-M3 ONNX model.
    pub fn new_bge_m3(cache_dir: PathBuf) -> Result<Self, FastembedEmbedderError> {
        fs::create_dir_all(&cache_dir)?;

        info!(
            cache_dir = %cache_dir.display(),
            "initialising bge-m3 embedder (downloads on first run)",
        );

        let opts = InitOptions::new(EmbeddingModel::BGEM3)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false);

        let model = TextEmbedding::try_new(opts)
            .map_err(|e| FastembedEmbedderError::Init(format!("{e}")))?;

        debug!("bge-m3 embedder ready");
        Ok(Self { model: Some(model) })
    }

    /// Run a batch through the underlying fastembed model on the
    /// blocking pool. Single helper used by both [`Self::embed_query`]
    /// and [`Self::embed_passages`] so the `spawn_blocking` plumbing
    /// has one home.
    async fn embed_blocking(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut model = self.model.take().ok_or_else(|| {
            EmbedError::Backend("embedder model unavailable (poisoned)".to_string())
        })?;

        // fastembed::embed is sync + CPU-bound; offload to the blocking pool.
        let join = task::spawn_blocking(move || {
            let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
            let outcome = model.embed(borrowed, None);
            (model, outcome)
        })
        .await
        .map_err(|e| EmbedError::Backend(format!("blocking task join error: {e}")))?;
        let (model, outcome) = join;
        self.model = Some(model);

        outcome.map_err(|e| EmbedError::Backend(format!("{e}")))
    }
}

#[async_trait]
impl Embedder for FastembedEmbedder {
    async fn embed_query(
        &mut self,
        text: String,
        with_prefix: bool,
    ) -> Result<Vec<f32>, EmbedError> {
        let prepared = if with_prefix {
            format!("{EMBEDDER_QUERY_PREFIX}{text}")
        } else {
            text
        };
        let mut vectors = self.embed_blocking(vec![prepared]).await?;
        vectors
            .pop()
            .ok_or_else(|| EmbedError::Backend("fastembed returned no vector".to_string()))
    }

    async fn embed_passages(
        &mut self,
        texts: Vec<String>,
        with_prefix: bool,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared = if with_prefix {
            texts
                .into_iter()
                .map(|t| format!("{EMBEDDER_PASSAGE_PREFIX}{t}"))
                .collect()
        } else {
            texts
        };
        self.embed_blocking(prepared).await
    }
}
