//! Text embeddings via [`fastembed`].
//!
//! FASE 1.0 supports only `bge-m3` (BAAI/bge-m3). Switching models requires an
//! ADR amendment. The model file is downloaded on first use into the global
//! schema cache (`~/.cache/schema/models/`), shared across projects.

mod bge_m3;

pub use bge_m3::{BGE_M3_DIMENSIONS, Embedder, EmbedderError};
