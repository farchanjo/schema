//! Vector store + delta-sync orchestrator.
//!
//! [`VectorStore`] wraps a LanceDB instance scoped to a single project. The
//! [`DeltaSync`] runs on startup (and after filesystem events): it compares
//! the current state of disk against the persisted [`Metadata`] manifest and
//! re-embeds only the files that changed. Files removed from disk have their
//! chunks pruned from the index.

mod metadata;
mod store;
mod sync;

pub use metadata::{FileMeta, Metadata};
pub use store::{ChunkRecord, VectorStore, VectorStoreError};
pub use sync::{DeltaSync, SyncReport};
