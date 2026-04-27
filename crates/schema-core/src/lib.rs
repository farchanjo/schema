//! Shared kernel between the **schema** and **recall** bounded contexts
//! (ADR-0033 + ADR-0034).
//!
//! This crate exports infrastructure both contexts use without leaking
//! either context's domain vocabulary:
//!
//! - The [`Embedder`] port — text → dense-vector trait.
//! - The [`FastembedEmbedder`] outbound adapter wrapping
//!   `fastembed`'s ONNX-backed BGE-M3 model with a tokio-friendly
//!   `spawn_blocking` shim.
//! - The BGE-M3 asymmetric query / passage prefix constants (ADR-0029).
//! - The output dimension constant [`BGE_M3_DIMENSIONS`].
//!
//! Per ADR-0034, schema and recall add this crate via
//! `schema-core = { path = "../schema-core" }` in their member
//! `Cargo.toml`. Vocabulary that is corpus-specific (`CorpusKind`,
//! `Chunk`, `Persistence`) stays in the **schema** crate; vocabulary
//! that is transcript-specific (`Turn`, `ToolResult`, `Artifact`) lives
//! in the **recall** crate. Anything genuinely shared lands here.

//! Per ADR-0012's `pub_use` deny rule, callers reach modules directly:
//! `use schema_core::embedder::Embedder` and
//! `use schema_core::fastembed_embedder::FastembedEmbedder`. The crate
//! root does not flatten the namespace.

pub mod embedder;
pub mod fastembed_embedder;
