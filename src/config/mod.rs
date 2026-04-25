//! Configuration loader and project identity resolver.
//!
//! A *project consumer* drops a `schema.toml` at its repo root. This module
//! parses that TOML, resolves the project's stable identifier, and computes
//! the cache directory under `~/.cache/schema/projects/<id>/`.

mod project;
mod schema_toml;

pub use project::{ProjectId, ProjectIdentity};
pub use schema_toml::{
    Corpus, CorpusKind, EmbeddingConfig, ProjectMeta, RetrievalConfig, SchemaConfig, SecurityConfig,
};
