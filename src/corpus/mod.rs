//! Corpus walking and chunking.
//!
//! Given a [`SchemaConfig`](crate::config::SchemaConfig), the [`Walker`] visits
//! every file matching the declared corpus entries. Each file is dispatched to
//! a [`Chunker`] keyed on the [`CorpusKind`](crate::config::CorpusKind), which
//! produces a vector of [`Chunk`]s ready for embedding and persistence.
//!
//! Re-indexing on save is handled by [`watcher`], which thinly wraps `notify`
//! with the `kqueue` backend on macOS (per ADR-0010) and the default Linux
//! backend (inotify, no feature flag).

mod chunker;
mod walker;
mod watcher;

pub use chunker::{Chunk, Chunker, ChunkerError};
pub use walker::{Walker, WalkerError};
pub use watcher::{CorpusEvent, CorpusWatcher};
