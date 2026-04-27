//! `schema` library — internals exposed to integration tests and the binary entry.
//!
//! Layout follows ADR-0013 (hexagonal-lite):
//! - [`domain`] — pure types
//! - [`ports`]  — async/sync trait boundaries
//! - [`app`]    — orchestration services depending only on `domain` + `ports`
//! - [`adapters`] — concrete adapters implementing the ports

#![allow(
    unused_crate_dependencies,
    reason = "Cargo.toml is shared between the library and the `schema` binary. \
              Some deps (clap, chrono, fd-lock, pulldown-cmark, tracing-subscriber) \
              are used only by the binary or reserved for upcoming commits, but \
              `unused_crate_dependencies` runs per-target and flags them on the \
              lib build. Layer C `deny` for this lint stays — only narrowed here."
)]

pub mod adapters;
pub mod app;
pub mod cli;
pub mod domain;
pub mod ports;
