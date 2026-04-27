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

    /// Domain specification (DDD): classify a freshly-observed file
    /// against this manifest, **without I/O**. The caller probes the
    /// filesystem for `mtime` / `size_bytes` (cheap), and only computes
    /// `hash_if_computed` (expensive `blake3` over file content) when
    /// the metadata short-circuit fails.
    ///
    /// ADR-0017 short-circuit lives here, not in the application
    /// service: it is a domain decision about how the manifest
    /// classifies file changes. `delta_sync::classify_file` becomes a
    /// thin orchestrator around this method.
    #[must_use]
    pub fn classify(
        &self,
        relative_path: &Path,
        mtime: i64,
        size_bytes: u64,
        hash_if_computed: Option<&str>,
    ) -> ChangeOutcome {
        match self.get(relative_path) {
            None => ChangeOutcome::New,
            Some(existing) if existing.mtime == mtime && existing.size_bytes == size_bytes => {
                ChangeOutcome::UnchangedByMetadata
            }
            Some(existing) if hash_if_computed.is_some_and(|h| h == existing.content_hash) => {
                ChangeOutcome::UnchangedByHash
            }
            Some(_) => ChangeOutcome::Modified,
        }
    }
}

/// Domain enum (Value Object): outcome of [`Metadata::classify`].
///
/// Pure tag; no I/O involved. The application service maps each variant
/// to a counter bump on `SyncReport` plus a reindex-job push when needed.
/// Adding a variant is a domain-shape change and requires updating every
/// match site (rustc enforces that via `#[non_exhaustive]` deliberately
/// **not** applied here — the enum is closed by design).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOutcome {
    /// File is new — not present in the manifest.
    New,
    /// `mtime + size` agree with the manifest entry; ADR-0017
    /// short-circuit fires, no hash needed.
    UnchangedByMetadata,
    /// `mtime` or `size` differ but a recomputed hash matches the
    /// manifest's `content_hash`. The file was touched (e.g., `touch`,
    /// editor save-without-edit) but its content is identical.
    UnchangedByHash,
    /// Hash differs from the manifest entry — file content changed and
    /// re-embedding is required.
    Modified,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::{ChangeOutcome, FileMeta, Metadata};
    use std::path::PathBuf;

    fn meta(mtime: i64, size: u64, hash: &str) -> FileMeta {
        FileMeta {
            mtime,
            size_bytes: size,
            content_hash: hash.to_string(),
            chunk_count: 1,
        }
    }

    fn manifest_with(path: &str, fm: FileMeta) -> Metadata {
        let mut m = Metadata::default();
        m.upsert(&PathBuf::from(path), fm);
        m
    }

    /// ADR-0013 §"DDD Specification" + ADR-0017 short-circuit:
    /// when the path is unknown, classification is `New`. No hash
    /// is consulted (caller is allowed to pass `None`).
    #[test]
    fn classify_new_path_returns_new() {
        let manifest = Metadata::default();
        assert_eq!(
            manifest.classify(&PathBuf::from("doc.md"), 100, 200, None),
            ChangeOutcome::New
        );
    }

    /// ADR-0017 fitness — same `(mtime, size)` short-circuits to
    /// `UnchangedByMetadata` and the caller never has to compute a
    /// hash.
    #[test]
    fn classify_matching_mtime_size_short_circuits() {
        let manifest = manifest_with("doc.md", meta(100, 200, "deadbeef"));
        assert_eq!(
            manifest.classify(&PathBuf::from("doc.md"), 100, 200, None),
            ChangeOutcome::UnchangedByMetadata
        );
    }

    /// `mtime` differs but recomputed hash matches → `UnchangedByHash`
    /// (touched but content-identical).
    #[test]
    fn classify_mtime_drifted_but_hash_matches_unchanged_by_hash() {
        let manifest = manifest_with("doc.md", meta(100, 200, "deadbeef"));
        assert_eq!(
            manifest.classify(&PathBuf::from("doc.md"), 999, 200, Some("deadbeef")),
            ChangeOutcome::UnchangedByHash
        );
    }

    /// Size changed → hash must differ (mathematically possible to
    /// match but rare); caller passes computed hash; classifies as
    /// `Modified`.
    #[test]
    fn classify_size_changed_with_new_hash_returns_modified() {
        let manifest = manifest_with("doc.md", meta(100, 200, "deadbeef"));
        assert_eq!(
            manifest.classify(&PathBuf::from("doc.md"), 100, 250, Some("aaaa1111")),
            ChangeOutcome::Modified
        );
    }

    /// Hash absent + mtime/size disagree → still `Modified`. Caller is
    /// expected to recompute hash next; this branch covers the case
    /// where the caller is probing without yet having the hash and
    /// wants to learn whether to spend the cost.
    #[test]
    fn classify_drifted_without_hash_returns_modified() {
        let manifest = manifest_with("doc.md", meta(100, 200, "deadbeef"));
        assert_eq!(
            manifest.classify(&PathBuf::from("doc.md"), 999, 250, None),
            ChangeOutcome::Modified
        );
    }
}
