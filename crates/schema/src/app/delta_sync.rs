//! Delta-sync orchestrator.
//!
//! On startup (and after notify events), [`DeltaSync::run`] reconciles the
//! on-disk corpus against the [`Metadata`] manifest:
//!
//! 1. Walk all corpus entries declared in `schema.toml` (via [`crate::ports::Walker`]).
//! 2. For each file: compute mtime + content hash; compare to manifest.
//! 3. Build three sets — `created`, `modified`, `removed`.
//! 4. Apply: delete from persistence by `source_path`; chunk + embed; append.
//! 5. Update manifest and persist via [`crate::ports::MetadataStore`].
//!
//! Per ADR-0013 this service is generic over its ports through `Arc<dyn ...>`,
//! so swapping `LanceDB` for sqlite-vec (ADR-0011) only needs a different
//! adapter wired in at `main.rs` — no changes to this file.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::adapters::metadata_store::{file_content_hash, file_mtime};
use crate::adapters::toml_config::SchemaConfig;
use crate::domain::{ChangeOutcome, Chunk, CorpusKind, DiscoveredFile, FileMeta, Metadata};
use crate::ports::{Chunker, MetadataStore, Persistence, Walker};
use schema_core::embedder::Embedder;

/// Summary of what a sync pass did.
#[derive(Debug, Default, Clone)]
pub struct SyncReport {
    pub files_total: usize,
    pub files_unchanged: usize,
    pub files_added: usize,
    pub files_modified: usize,
    pub files_removed: usize,
    pub chunks_indexed: usize,
}

/// Service that runs a single delta-sync pass against the wired-up ports.
///
/// All ports are held behind `Arc<dyn>` (or `Arc<Mutex<dyn>>` for the
/// `&mut`-needing embedder) so the service is cheap to clone and safe to
/// share across tasks (the watcher consumer needs that).
#[derive(Clone)]
pub struct DeltaSync {
    persistence: Arc<dyn Persistence>,
    embedder: Arc<Mutex<dyn Embedder>>,
    walker: Arc<dyn Walker>,
    chunker: Arc<dyn Chunker>,
    metadata: Arc<dyn MetadataStore>,
    /// Per-project ADR-0029 flag: whether `embed_passages` should
    /// prepend the BAAI bge-m3 passage prefix. Captured at wire time
    /// from `[embedding] query_passage_prefix`. Threaded into every
    /// embed call so the daemon's shared embedder honours the
    /// project's recipe.
    query_passage_prefix: bool,
}

impl fmt::Debug for DeltaSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeltaSync")
            .field("persistence", &"<dyn Persistence>")
            .field("embedder", &"<Mutex<dyn Embedder>>")
            .field("walker", &"<dyn Walker>")
            .field("chunker", &"<dyn Chunker>")
            .field("metadata", &"<dyn MetadataStore>")
            .field("query_passage_prefix", &self.query_passage_prefix)
            .finish()
    }
}

/// State produced by `discover_disk_state`: the on-disk relative paths and
/// the list of files that need to be (re-)embedded.
struct DiskDiff {
    on_disk: BTreeSet<String>,
    to_reindex: Vec<ReindexJob>,
}

struct ReindexJob {
    relative: PathBuf,
    absolute: PathBuf,
    kind: CorpusKind,
    meta: FileMeta,
}

impl DeltaSync {
    #[must_use]
    pub fn new(
        persistence: Arc<dyn Persistence>,
        embedder: Arc<Mutex<dyn Embedder>>,
        walker: Arc<dyn Walker>,
        chunker: Arc<dyn Chunker>,
        metadata: Arc<dyn MetadataStore>,
        query_passage_prefix: bool,
    ) -> Self {
        Self {
            persistence,
            embedder,
            walker,
            chunker,
            metadata,
            query_passage_prefix,
        }
    }

    /// Run a full delta-sync pass: walk the corpus, diff against the metadata
    /// manifest, prune deletions, re-embed changes, persist the manifest.
    ///
    /// # Errors
    /// Returns an error if metadata load/save, walking, hashing, embedding, or
    /// any persistence operation fails.
    pub async fn run(&self) -> anyhow::Result<SyncReport> {
        let mut metadata = self.metadata.load()?;

        let discovered = self.walker.discover()?;
        let mut report = SyncReport {
            files_total: discovered.len(),
            ..Default::default()
        };

        let diff = Self::discover_disk_state(&discovered, &metadata, &mut report)?;
        let removed = compute_removed(&metadata, &diff.on_disk);
        report.files_removed = removed.len();

        self.apply_deletions(&mut metadata, &removed, &diff.to_reindex)
            .await?;
        self.embed_and_append(diff.to_reindex, &mut metadata, &mut report)
            .await?;

        self.metadata.save(&metadata)?;
        log_sync_report(&report);
        Ok(report)
    }

    fn discover_disk_state(
        discovered: &[DiscoveredFile],
        metadata: &Metadata,
        report: &mut SyncReport,
    ) -> anyhow::Result<DiskDiff> {
        let mut on_disk: BTreeSet<String> = BTreeSet::new();
        let mut to_reindex: Vec<ReindexJob> = Vec::new();
        for file in discovered {
            on_disk.insert(file.relative_path.to_string_lossy().to_string());
            classify_file(file, metadata, report, &mut to_reindex)?;
        }
        Ok(DiskDiff {
            on_disk,
            to_reindex,
        })
    }

    /// Delete stale rows BEFORE re-embedding so reindexed files don't
    /// accumulate duplicates from prior runs.
    async fn apply_deletions(
        &self,
        metadata: &mut Metadata,
        removed: &[String],
        to_reindex: &[ReindexJob],
    ) -> anyhow::Result<()> {
        let to_delete_paths: Vec<String> = removed
            .iter()
            .cloned()
            .chain(
                to_reindex
                    .iter()
                    .map(|job| job.relative.to_string_lossy().to_string()),
            )
            .collect();
        if !to_delete_paths.is_empty() {
            let refs: Vec<&str> = to_delete_paths.iter().map(String::as_str).collect();
            self.persistence.delete_by_source(&refs).await?;
        }
        for r in removed {
            metadata.files.remove(r);
        }
        Ok(())
    }

    async fn embed_and_append(
        &self,
        jobs: Vec<ReindexJob>,
        metadata: &mut Metadata,
        report: &mut SyncReport,
    ) -> anyhow::Result<()> {
        for job in jobs {
            let ReindexJob {
                relative,
                absolute,
                kind,
                mut meta,
            } = job;
            let chunks: Vec<Chunk> = self.chunker.chunk(&relative, &absolute, kind)?;
            if chunks.is_empty() {
                warn!(path = %relative.display(), "chunker produced 0 chunks; skipping");
                continue;
            }
            let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
            let vectors = {
                let mut emb = self.embedder.lock().await;
                emb.embed_passages(texts, self.query_passage_prefix).await?
            };
            self.persistence.append_chunks(&chunks, &vectors).await?;
            meta.chunk_count = chunks.len();
            report.chunks_indexed += chunks.len();
            metadata.upsert(&relative, meta);
        }
        Ok(())
    }
}

fn compute_removed(metadata: &Metadata, on_disk: &BTreeSet<String>) -> Vec<String> {
    metadata
        .files
        .keys()
        .filter(|k| !on_disk.contains(*k))
        .cloned()
        .collect()
}

/// Application-service orchestrator around the domain decision
/// [`Metadata::classify`].
///
/// ADR-0017 short-circuit semantics live entirely on the domain side —
/// see [`Metadata::classify`]. This function only does I/O and
/// translation.
fn classify_file(
    file: &DiscoveredFile,
    metadata: &Metadata,
    report: &mut SyncReport,
    to_reindex: &mut Vec<ReindexJob>,
) -> anyhow::Result<()> {
    let mtime = file_mtime(&file.absolute_path)?;
    let size_bytes = file.size_bytes;

    if matches!(
        metadata.classify(&file.relative_path, mtime, size_bytes, None),
        ChangeOutcome::UnchangedByMetadata,
    ) {
        report.files_unchanged += 1;
        return Ok(());
    }

    let hash = file_content_hash(&file.absolute_path)?;
    let new_meta = FileMeta {
        mtime,
        size_bytes,
        content_hash: hash.clone(),
        chunk_count: 0,
    };
    apply_hashed_outcome(file, metadata, &hash, new_meta, report, to_reindex);
    Ok(())
}

/// Translate the second-pass (hash-aware) [`ChangeOutcome`] into a
/// counter bump and, when applicable, a reindex job. Pure mapping —
/// extracted from `classify_file` to honour the project's 30-line
/// function ceiling and to keep the domain dispatch readable.
fn apply_hashed_outcome(
    file: &DiscoveredFile,
    metadata: &Metadata,
    hash: &str,
    new_meta: FileMeta,
    report: &mut SyncReport,
    to_reindex: &mut Vec<ReindexJob>,
) {
    match metadata.classify(
        &file.relative_path,
        new_meta.mtime,
        new_meta.size_bytes,
        Some(hash),
    ) {
        ChangeOutcome::New => {
            report.files_added += 1;
            to_reindex.push(reindex_job(file, new_meta));
        }
        ChangeOutcome::Modified => {
            report.files_modified += 1;
            to_reindex.push(reindex_job(file, new_meta));
        }
        ChangeOutcome::UnchangedByHash => {
            // mtime/size drifted but content is byte-identical — touch
            // event, no reindex.
            report.files_unchanged += 1;
        }
        ChangeOutcome::UnchangedByMetadata => {
            // Filtered by `classify_file`'s first-pass call. Reaching
            // this branch would be a domain-logic regression.
            unreachable!(
                "UnchangedByMetadata must be filtered by the first \
                 metadata.classify(..., None) call"
            )
        }
    }
}

fn reindex_job(file: &DiscoveredFile, meta: FileMeta) -> ReindexJob {
    ReindexJob {
        relative: file.relative_path.clone(),
        absolute: file.absolute_path.clone(),
        kind: file.kind,
        meta,
    }
}

fn log_sync_report(report: &SyncReport) {
    info!(
        total = report.files_total,
        unchanged = report.files_unchanged,
        added = report.files_added,
        modified = report.files_modified,
        removed = report.files_removed,
        chunks = report.chunks_indexed,
        "delta-sync complete"
    );
}

/// Helper for `main::run_serve` that needs the absolute paths to hand to a
/// [`crate::ports::Watcher`] adapter.
#[must_use]
pub fn corpus_paths_from_config(config: &SchemaConfig, project_root: &Path) -> Vec<PathBuf> {
    config
        .corpus
        .iter()
        .map(|c| project_root.join(&c.path))
        .collect()
}

// NOTE: `classify_file` (above) calls `file_content_hash` and `file_mtime`
// directly from `crate::adapters::metadata_store`. That is a real ADR-0013
// boundary leak: `app/` should not depend on `adapters/`. Until the helpers
// move behind a port, the unit test below has to materialise real (empty)
// files on disk via `tempfile` to satisfy the hash + mtime calls.
//
// TODO(ADR-0013-followup): move file_content_hash/file_mtime behind a `FileMetaProbe` port to fully decouple from adapters/.

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::*;
    use crate::domain::ChunkRecord;
    use crate::ports::{ChunkerError, MetadataStoreError, PersistenceError, WalkerError};
    use async_trait::async_trait;
    use schema_core::embedder::EmbedError;
    use std::fs::File;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// In-memory `Persistence` fake. Keeps every appended `(Chunk, vector)`
    /// pair so tests can assert on the orchestration's output without a
    /// real `SQLite` file behind it.
    #[derive(Default)]
    struct FakePersistence {
        rows: StdMutex<Vec<(Chunk, Vec<f32>)>>,
    }

    impl fmt::Debug for FakePersistence {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FakePersistence").finish()
        }
    }

    /// Convert a fake row into a [`ChunkRecord`] with no scoring info
    /// (used by `find_*`).
    fn to_chunk_record(c: &Chunk) -> ChunkRecord {
        ChunkRecord {
            id: c.source_path.clone(),
            source_path: c.source_path.clone(),
            line_start: 1,
            line_end: 1,
            artifact_id: c.artifact_id.clone(),
            title: c.title.clone(),
            kind: format!("{:?}", c.kind),
            content: c.content.clone(),
            score: None,
        }
    }

    #[async_trait]
    impl Persistence for FakePersistence {
        async fn ensure_ready(&self) -> Result<(), PersistenceError> {
            Ok(())
        }

        async fn append_chunks(
            &self,
            chunks: &[Chunk],
            vectors: &[Vec<f32>],
        ) -> Result<(), PersistenceError> {
            let pairs: Vec<(Chunk, Vec<f32>)> = chunks
                .iter()
                .cloned()
                .zip(vectors.iter().cloned())
                .collect();
            let mut rows = self.rows.lock().unwrap();
            rows.extend(pairs);
            drop(rows);
            Ok(())
        }

        async fn delete_by_source(&self, paths: &[&str]) -> Result<(), PersistenceError> {
            let owned: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
            let mut rows = self.rows.lock().unwrap();
            rows.retain(|(c, _)| !owned.iter().any(|p| p == &c.source_path));
            drop(rows);
            Ok(())
        }

        async fn query_nearest(
            &self,
            _vector: &[f32],
            k: usize,
            _kind_filter: Option<&str>,
            _min_score: Option<f32>,
        ) -> Result<Vec<ChunkRecord>, PersistenceError> {
            let rows = self.rows.lock().unwrap();
            let out: Vec<ChunkRecord> = rows
                .iter()
                .take(k)
                .enumerate()
                .map(|(i, (c, _))| ChunkRecord {
                    id: format!("fake-{i}"),
                    source_path: c.source_path.clone(),
                    line_start: 1,
                    line_end: 1,
                    artifact_id: c.artifact_id.clone(),
                    title: c.title.clone(),
                    kind: format!("{:?}", c.kind),
                    content: c.content.clone(),
                    score: Some(1.0_f32),
                })
                .collect();
            drop(rows);
            Ok(out)
        }

        async fn find_by_artifact_id(
            &self,
            artifact_id: &str,
            limit: usize,
        ) -> Result<Vec<ChunkRecord>, PersistenceError> {
            let rows = self.rows.lock().unwrap();
            let out: Vec<ChunkRecord> = rows
                .iter()
                .filter(|(c, _)| c.artifact_id.as_deref() == Some(artifact_id))
                .take(limit)
                .map(|(c, _)| to_chunk_record(c))
                .collect();
            drop(rows);
            Ok(out)
        }

        async fn find_mentioning(
            &self,
            needle: &str,
            limit: usize,
        ) -> Result<Vec<ChunkRecord>, PersistenceError> {
            let rows = self.rows.lock().unwrap();
            let out: Vec<ChunkRecord> = rows
                .iter()
                .filter(|(c, _)| c.content.contains(needle))
                .take(limit)
                .map(|(c, _)| to_chunk_record(c))
                .collect();
            drop(rows);
            Ok(out)
        }

        async fn list_source_paths(&self) -> Result<Vec<String>, PersistenceError> {
            let rows = self.rows.lock().unwrap();
            let mut paths: Vec<String> = rows.iter().map(|(c, _)| c.source_path.clone()).collect();
            drop(rows);
            paths.sort();
            paths.dedup();
            Ok(paths)
        }

        async fn reset_all(&self) -> Result<(), PersistenceError> {
            let mut rows = self.rows.lock().unwrap();
            rows.clear();
            drop(rows);
            Ok(())
        }

        async fn read_embedding_recipe(&self) -> Result<Option<String>, PersistenceError> {
            Ok(None)
        }

        async fn write_embedding_recipe(&self, _recipe: &str) -> Result<(), PersistenceError> {
            Ok(())
        }
    }

    /// Deterministic `Embedder` fake — produces a unit vector per input
    /// without touching ONNX or fastembed.
    #[derive(Debug, Default)]
    struct FakeEmbedder;

    #[async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed_query(
            &mut self,
            _text: String,
            _with_prefix: bool,
        ) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![1.0_f32, 0.0, 0.0])
        }

        async fn embed_passages(
            &mut self,
            texts: Vec<String>,
            _with_prefix: bool,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32, 0.0, 0.0]).collect())
        }
    }

    /// `Walker` fake — returns a precomputed list of `DiscoveredFile`s.
    /// The test fills the list with paths to real (empty) files inside
    /// a `TempDir` so that `file_content_hash` + `file_mtime` (the
    /// boundary leak documented above) succeed.
    #[derive(Debug)]
    struct FakeWalker {
        files: Vec<DiscoveredFile>,
    }

    impl Walker for FakeWalker {
        fn discover(&self) -> Result<Vec<DiscoveredFile>, WalkerError> {
            Ok(self.files.clone())
        }
    }

    /// `Chunker` fake — returns exactly one chunk per file.
    #[derive(Debug, Default)]
    struct FakeChunker;

    impl Chunker for FakeChunker {
        fn chunk(
            &self,
            relative_path: &Path,
            _absolute_path: &Path,
            kind: CorpusKind,
        ) -> Result<Vec<Chunk>, ChunkerError> {
            Ok(vec![Chunk {
                source_path: relative_path.to_string_lossy().to_string(),
                line_start: 1,
                line_end: 1,
                artifact_id: None,
                title: Some("fake".to_string()),
                content: "fake content".to_string(),
                kind,
            }])
        }
    }

    /// In-memory `MetadataStore` fake — round-trips a single `Metadata`
    /// behind a `Mutex`.
    #[derive(Debug, Default)]
    struct FakeMetadataStore {
        inner: StdMutex<Metadata>,
    }

    impl MetadataStore for FakeMetadataStore {
        fn load(&self) -> Result<Metadata, MetadataStoreError> {
            let guard = self.inner.lock().unwrap();
            let copy = guard.clone();
            drop(guard);
            Ok(copy)
        }

        fn save(&self, metadata: &Metadata) -> Result<(), MetadataStoreError> {
            let mut guard = self.inner.lock().unwrap();
            *guard = metadata.clone();
            drop(guard);
            Ok(())
        }

        fn reset(&self) -> Result<(), MetadataStoreError> {
            self.save(&Metadata::default())
        }
    }

    /// Materialise an empty file at `path`; the file must exist for
    /// `file_content_hash` and `file_mtime` to succeed (the boundary
    /// leak documented above).
    fn touch(path: &Path) {
        File::create(path).unwrap();
    }

    /// Materialise two empty files inside `root` and produce the
    /// matching `DiscoveredFile`s for the fake walker.
    fn touched_walker_files(root: &Path) -> Vec<DiscoveredFile> {
        let abs_a = root.join("a.md");
        let abs_b = root.join("b.md");
        touch(&abs_a);
        touch(&abs_b);
        vec![
            DiscoveredFile {
                absolute_path: abs_a,
                relative_path: PathBuf::from("a.md"),
                kind: CorpusKind::Markdown,
                size_bytes: 0,
            },
            DiscoveredFile {
                absolute_path: abs_b,
                relative_path: PathBuf::from("b.md"),
                kind: CorpusKind::Markdown,
                size_bytes: 0,
            },
        ]
    }

    /// Wire fake ports into a [`DeltaSync`] backed by two empty files
    /// inside `root`. Returns the suite plus the fakes the caller will
    /// assert against after `run()`.
    fn build_fake_suite(root: &Path) -> (DeltaSync, Arc<FakePersistence>, Arc<FakeMetadataStore>) {
        let walker_files = touched_walker_files(root);
        let persistence = Arc::new(FakePersistence::default());
        let embedder = Arc::new(Mutex::new(FakeEmbedder));
        let walker = Arc::new(FakeWalker {
            files: walker_files,
        });
        let chunker = Arc::new(FakeChunker);
        let metadata = Arc::new(FakeMetadataStore::default());

        let sync = DeltaSync::new(
            Arc::<FakePersistence>::clone(&persistence),
            embedder,
            walker,
            chunker,
            Arc::<FakeMetadataStore>::clone(&metadata),
            false,
        );
        (sync, persistence, metadata)
    }

    /// ADR-0013 unit-testability proof — orchestrates `DeltaSync` with
    /// only fakes wired through the port traits. No real disk for the
    /// store, no embedder, no walker, no chunker — just `Arc<dyn ...>`.
    /// The two empty files in `TempDir` exist solely to satisfy the
    /// `file_content_hash` / `file_mtime` boundary leak documented
    /// above (ADR-0013 follow-up).
    #[tokio::test]
    async fn delta_sync_orchestrates_through_fake_ports() {
        let tmp = TempDir::new().unwrap();
        let (sync, persistence, metadata) = build_fake_suite(tmp.path());

        let report = sync.run().await.unwrap();

        assert_eq!(report.files_total, 2);
        assert_eq!(report.files_added, 2);
        assert_eq!(report.files_modified, 0);
        assert_eq!(report.files_removed, 0);
        assert_eq!(
            report.chunks_indexed, 2,
            "FakeChunker emits 1 chunk per file"
        );

        let row_count = {
            let rows = persistence.rows.lock().unwrap();
            rows.len()
        };
        assert_eq!(row_count, 2, "FakePersistence must hold one row per file");
        let manifest_files = {
            let manifest = metadata.inner.lock().unwrap();
            manifest.files.clone()
        };
        assert_eq!(manifest_files.len(), 2);
        for (rel, fm) in &manifest_files {
            assert!(["a.md", "b.md"].contains(&rel.as_str()));
            assert_eq!(fm.chunk_count, 1);
        }
    }

    /// ADR-0017 fitness function — running `delta_sync.run()` twice with no
    /// changes between runs must report **all files unchanged** on the second
    /// pass and append **zero new rows** to persistence. This proves the
    /// `mtime + size` short-circuit fires on every file (no reindex jobs
    /// pushed) and that no behavioural regression slipped in.
    #[tokio::test]
    async fn delta_sync_idle_re_run_skips_every_file() {
        let tmp = TempDir::new().unwrap();
        let (sync, persistence, _metadata) = build_fake_suite(tmp.path());

        // First pass: populates manifest, appends rows.
        let first = sync.run().await.unwrap();
        assert_eq!(first.files_added, 2);
        assert_eq!(first.files_unchanged, 0);
        let rows_after_first = {
            let rows = persistence.rows.lock().unwrap();
            rows.len()
        };
        assert_eq!(rows_after_first, 2);

        // Second pass: same files, no changes on disk → every file must
        // short-circuit through the manifest check, no reindex jobs.
        let second = sync.run().await.unwrap();
        assert_eq!(second.files_total, 2);
        assert_eq!(second.files_unchanged, 2, "ADR-0017 short-circuit");
        assert_eq!(second.files_added, 0);
        assert_eq!(second.files_modified, 0);
        assert_eq!(second.files_removed, 0);
        assert_eq!(
            second.chunks_indexed, 0,
            "second pass must not re-embed any file"
        );

        // Persistence rows must not have grown — `apply_deletions` does not
        // delete unchanged paths, so `rows_after_second == rows_after_first`.
        let rows_after_second = {
            let rows = persistence.rows.lock().unwrap();
            rows.len()
        };
        assert_eq!(
            rows_after_second, rows_after_first,
            "no new rows on idle re-run"
        );
    }
}
