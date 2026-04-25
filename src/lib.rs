//! `schema` library — internals exposed to integration tests and the binary entry.

#![allow(
    unused_crate_dependencies,
    reason = "Cargo.toml is shared between the library and the `schema` binary. \
              Some deps (clap, chrono, fd-lock, pulldown-cmark, tracing-subscriber) \
              are used only by the binary or reserved for upcoming commits, but \
              `unused_crate_dependencies` runs per-target and flags them on the \
              lib build. Layer C `deny` for this lint stays — only narrowed here."
)]

pub mod config;
pub mod corpus;
pub mod embeddings;
pub mod mcp;
pub mod retrieval;
