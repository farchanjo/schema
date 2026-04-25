//! `bge-m3` embedder via fastembed 5.x (uses ONNX Runtime under the hood).
//!
//! BGE-M3 produces 1024-dimensional dense embeddings. fastembed downloads the
//! ONNX model from Hugging Face Hub on first use and caches it locally; we
//! point its cache to `~/.cache/schema/models/` so multiple projects share the
//! same downloaded weights.

use std::path::PathBuf;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use thiserror::Error;
use tracing::{debug, info};

use crate::config::cache_root;

/// Output dimension of BGE-M3.
pub const BGE_M3_DIMENSIONS: usize = 1024;

#[derive(Debug, Error)]
pub enum EmbedderError {
    #[error("fastembed initialisation failed: {0}")]
    Init(String),
    #[error("embedding failed: {0}")]
    Embed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// Embedder wrapping a fastembed `TextEmbedding` instance.
///
/// Construction triggers the model download on first run; subsequent
/// constructions reuse the cached model file.
pub struct Embedder {
    model: TextEmbedding,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("model", &"<fastembed::TextEmbedding bge-m3>")
            .finish()
    }
}

impl Embedder {
    /// Initialise the BGE-M3 embedder. Caches the model under
    /// `~/.cache/schema/models/`.
    pub fn new_bge_m3() -> Result<Self, EmbedderError> {
        let cache_dir = bge_m3_cache_dir()?;
        std::fs::create_dir_all(&cache_dir)?;

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
        Ok(Self { model })
    }

    /// Embed a batch of strings. Returns one vector per input, each of length
    /// [`BGE_M3_DIMENSIONS`].
    ///
    /// `batch_size` controls memory usage during encoding. `None` uses
    /// fastembed's default. Increase on M-series Macs with abundant RAM.
    pub fn embed(
        &mut self,
        documents: Vec<&str>,
        batch_size: Option<usize>,
    ) -> Result<Vec<Vec<f32>>, EmbedderError> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        self.model
            .embed(documents, batch_size)
            .map_err(|e| EmbedderError::Embed(format!("{e}")))
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
