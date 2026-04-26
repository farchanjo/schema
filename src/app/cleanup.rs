//! Cleanup orchestrator (ADR-0015).
//!
//! Groups the persistence reset + manifest reset behind two clear use cases
//! that the MCP tools (`reset_index`, `forget_source`) and the CLI
//! subcommands (`schema reset`, `schema forget`) share. Per ADR-0013 this
//! service depends only on [`crate::ports`]; concrete adapters are wired in
//! at `main.rs`.

use std::fmt;
use std::sync::Arc;

use tracing::info;

use crate::ports::{MetadataStore, Persistence};

/// Read-side service exposing the two cleanup verbs.
///
/// Cheap to clone — holds `Arc`s.
#[derive(Clone)]
pub struct Cleanup {
    persistence: Arc<dyn Persistence>,
    metadata: Arc<dyn MetadataStore>,
}

impl fmt::Debug for Cleanup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cleanup")
            .field("persistence", &"<dyn Persistence>")
            .field("metadata", &"<dyn MetadataStore>")
            .finish()
    }
}

impl Cleanup {
    #[must_use]
    pub fn new(persistence: Arc<dyn Persistence>, metadata: Arc<dyn MetadataStore>) -> Self {
        Self {
            persistence,
            metadata,
        }
    }

    /// Wipe every chunk from persistence AND reset the manifest to its
    /// default (empty) state.
    ///
    /// Idempotent: a second call is a no-op (the store is already empty,
    /// the manifest already default).
    ///
    /// # Errors
    /// Returns an error if the persistence wipe or manifest reset fails.
    pub async fn reset_index(&self) -> anyhow::Result<()> {
        info!("reset_index: wiping persistence + manifest");
        self.persistence.reset_all().await?;
        self.metadata.reset()?;
        Ok(())
    }

    /// Drop every chunk for `path` from persistence AND remove the matching
    /// entry from the manifest.
    ///
    /// `path` is interpreted as a project-root-relative source path,
    /// matching the convention used everywhere else in the crate (see
    /// [`crate::domain::Chunk::source_path`]). The on-disk source file is
    /// **not** touched. Idempotent: removing a path that does not exist
    /// returns `Ok(())`.
    ///
    /// # Errors
    /// Returns an error if the persistence delete or manifest update fails.
    pub async fn forget_source(&self, path: &str) -> anyhow::Result<()> {
        info!(path = %path, "forget_source: dropping path from index + manifest");
        self.persistence.delete_by_source(&[path]).await?;
        let mut meta = self.metadata.load()?;
        meta.files.remove(path);
        self.metadata.save(&meta)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::*;
    use crate::domain::{Chunk, ChunkRecord, CorpusKind, FileMeta, Metadata};
    use crate::ports::{MetadataStoreError, PersistenceError};
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    /// In-memory `Persistence` fake. Stores `(Chunk, Vec<f32>)` rows so the
    /// cleanup verbs can be observed end-to-end without a real `SQLite`
    /// file behind them.
    #[derive(Default)]
    struct FakePersistence {
        rows: StdMutex<Vec<(Chunk, Vec<f32>)>>,
    }

    impl fmt::Debug for FakePersistence {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FakePersistence").finish()
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
            _k: usize,
            _kind_filter: Option<&str>,
        ) -> Result<Vec<ChunkRecord>, PersistenceError> {
            Ok(Vec::new())
        }

        async fn find_by_artifact_id(
            &self,
            _artifact_id: &str,
            _limit: usize,
        ) -> Result<Vec<ChunkRecord>, PersistenceError> {
            Ok(Vec::new())
        }

        async fn find_mentioning(
            &self,
            _needle: &str,
            _limit: usize,
        ) -> Result<Vec<ChunkRecord>, PersistenceError> {
            Ok(Vec::new())
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

    fn sample_chunk(path: &str) -> Chunk {
        Chunk {
            source_path: path.to_string(),
            line_start: 1,
            line_end: 1,
            artifact_id: None,
            title: Some("t".to_string()),
            content: "c".to_string(),
            kind: CorpusKind::Markdown,
        }
    }

    fn sample_meta() -> FileMeta {
        FileMeta {
            mtime: 0,
            size_bytes: 0,
            content_hash: "deadbeef".to_string(),
            chunk_count: 1,
        }
    }

    /// Seed both fakes with two paths and return them ready for cleanup
    /// assertions. Must be called from within an async context.
    async fn seeded_pair() -> (Arc<FakePersistence>, Arc<FakeMetadataStore>) {
        let persistence = Arc::new(FakePersistence::default());
        let metadata = Arc::new(FakeMetadataStore::default());

        // Persistence: two chunks under a.md and b.md.
        let chunks = vec![sample_chunk("a.md"), sample_chunk("b.md")];
        let vectors: Vec<Vec<f32>> = vec![vec![1.0_f32], vec![1.0_f32]];
        persistence.append_chunks(&chunks, &vectors).await.unwrap();

        // Manifest: two file entries.
        let mut meta = Metadata::default();
        meta.upsert(&PathBuf::from("a.md"), sample_meta());
        meta.upsert(&PathBuf::from("b.md"), sample_meta());
        metadata.save(&meta).unwrap();

        (persistence, metadata)
    }

    #[tokio::test]
    async fn reset_index_clears_both_persistence_and_metadata() {
        let (persistence, metadata) = seeded_pair().await;
        let cleanup = Cleanup::new(
            Arc::<FakePersistence>::clone(&persistence),
            Arc::<FakeMetadataStore>::clone(&metadata),
        );

        cleanup.reset_index().await.unwrap();

        let paths = persistence.list_source_paths().await.unwrap();
        assert!(paths.is_empty(), "persistence must be wiped, got {paths:?}");

        let manifest = metadata.load().unwrap();
        assert!(
            manifest.files.is_empty(),
            "manifest must be Metadata::default(), got {} entries",
            manifest.files.len()
        );
        assert_eq!(manifest.version, 1, "default version must be 1");
    }

    #[tokio::test]
    async fn forget_source_drops_only_the_named_path() {
        let (persistence, metadata) = seeded_pair().await;
        let cleanup = Cleanup::new(
            Arc::<FakePersistence>::clone(&persistence),
            Arc::<FakeMetadataStore>::clone(&metadata),
        );

        cleanup.forget_source("a.md").await.unwrap();

        let paths = persistence.list_source_paths().await.unwrap();
        assert_eq!(
            paths,
            vec!["b.md".to_string()],
            "only b.md should remain in persistence"
        );

        let manifest = metadata.load().unwrap();
        assert_eq!(manifest.files.len(), 1, "manifest must have one entry");
        assert!(
            manifest.files.contains_key("b.md"),
            "b.md must still be in the manifest"
        );
        assert!(
            !manifest.files.contains_key("a.md"),
            "a.md must be gone from the manifest"
        );
    }
}
