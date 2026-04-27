//! Application services — orchestration that depends only on
//! [`crate::domain`] and [`crate::ports`].
//!
//! Per ADR-0013 this module ONLY declares submodules; callers reach in via
//! `crate::app::delta_sync::DeltaSync`, `crate::app::query::Query`, etc. No
//! `pub use` flattening (the `pub_use` lint blocks it project-wide).

pub mod cleanup;
pub mod daemon;
pub mod delta_sync;
pub mod project_instance;
pub mod query;
pub mod synthesize;
pub mod watcher_consumer;
