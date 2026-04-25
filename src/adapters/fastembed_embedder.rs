//! `bge-m3` embedder via fastembed 5.x (uses ONNX Runtime under the hood).
//!
//! BGE-M3 produces 1024-dimensional dense embeddings. fastembed downloads the
//! ONNX model from Hugging Face Hub on first use and caches it locally; we
//! point its cache to `~/.cache/schema/models/` so multiple projects share the
//! same downloaded weights.
//!
//! Per ADR-0013 this adapter implements the [`crate::ports::Embedder`] async
//! trait. Because `fastembed::TextEmbedding::embed` is synchronous and CPU-
//! bound, the implementation hops to `tokio::task::spawn_blocking` so the
//! Tokio runtime remains responsive while ONNX is computing.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use thiserror::Error;
use tokio::task;
use tracing::{debug, info};

use crate::adapters::project_identity::cache_root;
use crate::ports::{EmbedError, Embedder};

/// Output dimension of BGE-M3.
pub const BGE_M3_DIMENSIONS: usize = 1024;

#[derive(Debug, Error)]
pub enum EmbedderError {
    #[error("fastembed initialisation failed: {0}")]
    Init(String),
    #[error("embedding failed: {0}")]
    Embed(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// Embedder wrapping a fastembed `TextEmbedding` instance.
///
/// Construction triggers the model download on first run; subsequent
/// constructions reuse the cached model file. Wrapped in an `Option` so the
/// async trait impl can move the model into `spawn_blocking` and put it back.
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
    /// Initialise the BGE-M3 embedder. Caches the model under
    /// `~/.cache/schema/models/`.
    ///
    /// # Errors
    /// Returns an error if the cache directory cannot be created or fastembed
    /// fails to load the BGE-M3 ONNX model.
    pub fn new_bge_m3() -> Result<Self, EmbedderError> {
        let cache_dir = bge_m3_cache_dir()?;
        fs::create_dir_all(&cache_dir)?;

        info!(
            cache_dir = %cache_dir.display(),
            "initialising bge-m3 embedder (downloads on first run)",
        );

        let opts = InitOptions::new(EmbeddingModel::BGEM3)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false);

        let model =
            TextEmbedding::try_new(opts).map_err(|e| EmbedderError::Init(format!("{e}")))?;

        debug!("bge-m3 embedder ready");
        Ok(Self { model: Some(model) })
    }
}

#[async_trait]
impl Embedder for FastembedEmbedder {
    async fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
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

/// `~/.cache/schema/models/`.
fn bge_m3_cache_dir() -> Result<PathBuf, EmbedderError> {
    let root = cache_root()?;
    Ok(root.join("models"))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]
    use super::*;

    #[test]
    fn cache_dir_under_schema_root() {
        let dir = bge_m3_cache_dir().unwrap();
        assert!(dir.ends_with("schema/models"));
    }

    // NOTE: the live embedder test is intentionally omitted here. Constructing
    // an Embedder downloads ~2 GB on first run and is unsuitable for `cargo test`.
    // Integration tests that exercise the embedder live in `tests/integration.rs`
    // and are gated behind the `schema-online` env flag (commit 11 wires it).
}
