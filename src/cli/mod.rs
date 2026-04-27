//! CLI subcommand implementations split off `main.rs`.
//!
//! Each module exposes one or more verbs that `main.rs::main()` dispatches.
//! Per ADR-0013, the CLI is a driving adapter for the underlying app
//! services — keeping the dispatch tree shallow makes the binary entry
//! easy to read.

pub mod install;
pub mod mcp_shim;
