//! Driven adapters — concrete implementations of the [`crate::ports`] traits.
//!
//! Per ADR-0013, this module ONLY declares its submodules; it does not
//! re-export their contents (the `pub_use` lint blocks internal flattening).
//! Callers reach in via `crate::adapters::sqlite_vec_store::SqliteVecStore`,
//! `crate::adapters::filesystem::WalkdirWalker`, etc. The [`crate::adapters::mcp_server`]
//! module is a *driving* adapter (it adapts incoming MCP RPCs); the rest are
//! driven adapters.

pub mod fastembed_embedder;
pub mod filesystem;
pub mod markdown_chunker;
pub mod mcp_server;
pub mod metadata_store;
pub mod project_identity;
pub mod sqlite_vec_store;
pub mod toml_config;
