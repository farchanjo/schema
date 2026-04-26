//! Port traits — the application's boundary with the outside world.
//!
//! Per ADR-0013, every piece of I/O the app does goes through a trait declared
//! here. Adapters in `crate::adapters` implement these traits using concrete
//! technologies (`LanceDB`, fastembed, notify, walkdir, pulldown-cmark, TOML).
//! The application services in `crate::app` accept `Arc<dyn Trait>` so wiring
//! is the only place concrete types appear.

use std::any::Any;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::domain::{Chunk, ChunkRecord, CorpusEvent, CorpusKind, DiscoveredFile, Metadata};

// ─── Persistence ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("persistence backend error: {0}")]
    Backend(String),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// Vector + chunk store used by the retrieval pipeline.
///
/// The default implementation today is `LanceDB`
/// (`crate::adapters::lancedb_store`). ADR-0011 will swap this for sqlite-vec
/// without touching `crate::app`.
#[async_trait]
pub trait Persistence: Send + Sync {
    /// Open / create whatever underlying storage is needed.
    async fn ensure_ready(&self) -> Result<(), PersistenceError>;

    /// Append rows for a batch of chunks paired with their embeddings.
    async fn append_chunks(
        &self,
        chunks: &[Chunk],
        vectors: &[Vec<f32>],
    ) -> Result<(), PersistenceError>;

    /// Delete every chunk whose `source_path` matches one of `paths`.
    async fn delete_by_source(&self, paths: &[&str]) -> Result<(), PersistenceError>;

    /// Wipe every chunk from the store and reclaim the disk pages.
    ///
    /// Implementations should perform a bulk `DELETE FROM chunks` (which
    /// cascades to companion virtual tables via existing triggers) followed
    /// by `VACUUM` (or the equivalent reclaim step) so the on-disk file does
    /// not retain dead pages. The store file is kept open; only its contents
    /// are emptied.
    ///
    /// # Errors
    /// Returns an error if the underlying delete fails.
    async fn reset_all(&self) -> Result<(), PersistenceError>;

    /// Top-K nearest-neighbour query. Optionally filtered by `kind`.
    async fn query_nearest(
        &self,
        vector: &[f32],
        k: usize,
        kind_filter: Option<&str>,
    ) -> Result<Vec<ChunkRecord>, PersistenceError>;

    /// Find every chunk whose `artifact_id` exactly matches the given id.
    async fn find_by_artifact_id(
        &self,
        artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>, PersistenceError>;

    /// Find every chunk whose `content` mentions the given substring.
    async fn find_mentioning(
        &self,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>, PersistenceError>;

    /// List every distinct `source_path` currently in the index.
    async fn list_source_paths(&self) -> Result<Vec<String>, PersistenceError>;
}

// ─── Embedder ────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("embedder backend error: {0}")]
    Backend(String),
}

/// Text → dense-vector embedding port. The fastembed-backed adapter wraps
/// the synchronous embed call in `tokio::task::spawn_blocking` so callers can
/// `.await` on it without blocking the runtime.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of strings. Returns one vector per input.
    async fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError>;
}

// ─── Walker ──────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum WalkerError {
    #[error("corpus path {0} does not exist")]
    PathNotFound(PathBuf),
    #[error("io error walking {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Synchronous filesystem walker that yields every corpus file.
pub trait Walker: Send + Sync {
    /// Visit every file matching every corpus entry. Returns a flat list.
    ///
    /// # Errors
    /// Returns an error if a corpus path is missing or filesystem traversal fails.
    fn discover(&self) -> Result<Vec<DiscoveredFile>, WalkerError>;
}

// ─── Chunker ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ChunkerError {
    #[error("unsupported corpus kind: {0:?}")]
    UnsupportedKind(CorpusKind),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Per-kind chunking strategy port.
pub trait Chunker: Send + Sync {
    /// Read the file at `absolute_path` and split it into chunks per the
    /// strategy associated with `kind`.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or the kind is unsupported.
    fn chunk(
        &self,
        relative_path: &Path,
        absolute_path: &Path,
        kind: CorpusKind,
    ) -> Result<Vec<Chunk>, ChunkerError>;
}

// ─── Watcher ─────────────────────────────────────────────────────────────

/// Owned handle to whatever resource keeps the underlying watcher alive.
/// Holding this alive keeps the watch running. Move it into a long-lived
/// task / struct; drop it to stop the watcher.
///
/// The held value is opaque (`dyn Any + Send`) on purpose — `ports.rs` must
/// not name any concrete adapter type (the `notify` crate, in our current
/// adapter). Adapters construct this via [`WatcherKeepAlive::new`].
pub struct WatcherKeepAlive {
    #[expect(
        dead_code,
        reason = "field exists solely to extend the watcher's lifetime; \
                  dropping this handle drops the adapter-specific watcher \
                  guard inside, which stops the watch"
    )]
    pub(crate) inner: Box<dyn Any + Send>,
}

impl WatcherKeepAlive {
    /// Wrap any adapter-specific guard so its `Drop` runs when this
    /// keep-alive is dropped.
    pub fn new<T: Send + 'static>(guard: T) -> Self {
        Self {
            inner: Box::new(guard),
        }
    }
}

impl fmt::Debug for WatcherKeepAlive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatcherKeepAlive")
            .field("inner", &"<dyn Watcher>")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("watcher backend error: {0}")]
    Backend(String),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// Filesystem-watcher port. `start` consumes the watcher (one-shot
/// activation) and returns a keep-alive plus the event receiver.
pub trait Watcher: Send {
    /// Start the watch. The keep-alive must be retained for the duration of
    /// the watch; dropping it stops event delivery.
    ///
    /// # Errors
    /// Returns an error if the underlying watcher backend cannot be created
    /// or fails to register one of the requested paths.
    fn start(
        self: Box<Self>,
    ) -> Result<(WatcherKeepAlive, mpsc::Receiver<CorpusEvent>), WatcherError>;
}

// ─── MetadataStore ───────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MetadataStoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serialise: {0}")]
    Serialise(String),
    #[error("parse: {0}")]
    Parse(String),
}

/// Persistence for the [`Metadata`] manifest. The default adapter writes TOML
/// to disk; tests can supply an in-memory implementation.
pub trait MetadataStore: Send + Sync {
    /// Load the persisted manifest, returning [`Metadata::default`] when no
    /// file exists yet.
    ///
    /// # Errors
    /// Returns an error if the on-disk manifest cannot be read or parsed.
    fn load(&self) -> Result<Metadata, MetadataStoreError>;

    /// Persist the manifest atomically.
    ///
    /// # Errors
    /// Returns an error if the parent directory cannot be created, the
    /// manifest cannot be serialised, or the file cannot be written.
    fn save(&self, metadata: &Metadata) -> Result<(), MetadataStoreError>;

    /// Overwrite the manifest with [`Metadata::default`].
    ///
    /// Equivalent to `save(&Metadata::default())`; exposed as a dedicated
    /// method so the cleanup use case has a single, intention-revealing
    /// call site that adapters can specialise (e.g. to delete the file
    /// instead of writing an empty one) without changing callers.
    ///
    /// # Errors
    /// Returns an error if the manifest cannot be written.
    fn reset(&self) -> Result<(), MetadataStoreError>;
}
