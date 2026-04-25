//! Domain types — pure, dependency-light data the rest of the crate flows around.
//!
//! Per ADR-0013, this module imports only `serde`, `thiserror`, `chrono`, and
//! `std`. NO I/O. NO async. NO tokio / lancedb / fastembed / notify / walkdir
//! / pulldown-cmark / toml dependencies. Methods on these types are pure
//! transformations (`upsert`, `get`, `remove`); persistence belongs to
//! [`crate::ports::MetadataStore`] and its adapters.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ─── Corpus kinds (was config/schema_toml.rs::CorpusKind) ─────────────

/// FASE 1.0 supports five kinds. `code` (rs/py/ts via tree-sitter) is FASE 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CorpusKind {
    AdrMadr,
    Markdown,
    Glossary,
    Cue,
    Openapi,
}

// ─── Chunk produced by a chunker (was corpus/chunker.rs::Chunk) ───────

/// A chunk extracted from a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Path relative to the project root.
    pub source_path: String,
    /// 1-indexed line range (inclusive).
    pub line_start: usize,
    pub line_end: usize,
    /// Optional artifact id (e.g. `"ADR-0055"`) when extractable.
    pub artifact_id: Option<String>,
    /// Optional human-readable title (e.g. the section heading).
    pub title: Option<String>,
    /// The raw chunk text.
    pub content: String,
    /// The kind of the originating corpus entry.
    pub kind: CorpusKind,
}

// ─── ChunkRecord — what the persistence port returns (was retrieval/store.rs) ─

/// A row materialised from the persistence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub id: String,
    pub source_path: String,
    pub line_start: i32,
    pub line_end: i32,
    pub artifact_id: Option<String>,
    pub title: Option<String>,
    pub kind: String,
    pub content: String,
    /// Distance returned by the nearest-neighbour query (lower = closer).
    pub score: Option<f32>,
}

// ─── DiscoveredFile (was corpus/walker.rs::DiscoveredFile) ────────────

/// A file discovered by the walker, paired with the kind from its corpus entry.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub absolute_path: PathBuf,
    /// Path relative to the project root.
    pub relative_path: PathBuf,
    pub kind: CorpusKind,
    pub size_bytes: u64,
}

// ─── CorpusEvent (was corpus/watcher.rs::CorpusEvent) ─────────────────

/// Filesystem event observed by the watcher port.
#[derive(Debug, Clone)]
pub enum CorpusEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
}

// ─── Metadata manifest (was retrieval/metadata.rs::Metadata + FileMeta) ─

/// Single-file record. Stored under the file's relative-to-project path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: i64,
    pub size_bytes: u64,
    pub content_hash: String,
    /// Number of chunks produced last time we indexed this file. Drives
    /// pruning when count drops on a re-index.
    pub chunk_count: usize,
}

/// In-memory shape of the metadata manifest.
///
/// Persistence (load + save) is delegated to
/// [`crate::ports::MetadataStore`] so this type stays pure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// Schema version of the metadata file itself.
    #[serde(default = "default_meta_version")]
    pub version: u32,

    /// Map: relative-path-as-string → file metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

const fn default_meta_version() -> u32 {
    1
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            version: default_meta_version(),
            files: BTreeMap::new(),
        }
    }
}

impl Metadata {
    pub fn upsert(&mut self, relative_path: &Path, meta: FileMeta) {
        self.files
            .insert(relative_path.to_string_lossy().to_string(), meta);
    }

    pub fn remove(&mut self, relative_path: &Path) -> Option<FileMeta> {
        self.files
            .remove(&relative_path.to_string_lossy().to_string())
    }

    #[must_use]
    pub fn get(&self, relative_path: &Path) -> Option<&FileMeta> {
        self.files.get(&relative_path.to_string_lossy().to_string())
    }
}
