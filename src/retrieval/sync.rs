//! Delta-sync orchestrator.
//!
//! On startup (and after notify events), [`DeltaSync::run`] reconciles the
//! on-disk corpus against the [`Metadata`] manifest:
//!
//! 1. Walk all corpus entries declared in `schema.toml`.
//! 2. For each file: compute mtime + content hash; compare to manifest.
//! 3. Build three sets — `created`, `modified`, `removed`.
//! 4. Apply: delete from LanceDB by source_path; chunk + embed; append.
//! 5. Update manifest and persist `metadata.toml`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tracing::{info, warn};

use crate::config::SchemaConfig;
use crate::corpus::{Chunker, Walker};
use crate::embeddings::Embedder;
use crate::retrieval::metadata::{FileMeta, Metadata, file_content_hash, file_mtime};
use crate::retrieval::store::VectorStore;

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

pub struct DeltaSync<'a> {
    pub config: &'a SchemaConfig,
    pub project_root: &'a std::path::Path,
    pub metadata_path: &'a std::path::Path,
}

impl<'a> DeltaSync<'a> {
    pub async fn run(
        &self,
        store: &VectorStore,
        embedder: &mut Embedder,
    ) -> anyhow::Result<SyncReport> {
        let mut metadata = Metadata::load_or_default(self.metadata_path)?;
        let walker = Walker::new(self.config, self.project_root);
        let chunker = Chunker::new(self.config.retrieval.chunk_size_max);

        let discovered = walker.discover()?;
        let mut report = SyncReport {
            files_total: discovered.len(),
            ..Default::default()
        };

        // Index discovered files by relative path for diffing.
        let mut on_disk: BTreeSet<String> = BTreeSet::new();
        let mut to_reindex: Vec<(PathBuf, PathBuf, crate::config::CorpusKind, FileMeta)> =
            Vec::new();

        for file in &discovered {
            on_disk.insert(file.relative_path.to_string_lossy().to_string());

            let mtime = file_mtime(&file.absolute_path)?;
            let hash = file_content_hash(&file.absolute_path)?;
            let new_meta = FileMeta {
                mtime,
                size_bytes: file.size_bytes,
                content_hash: hash.clone(),
                chunk_count: 0, // filled in after chunking
            };

            match metadata.get(&file.relative_path) {
                Some(existing) if existing.content_hash == hash => {
                    report.files_unchanged += 1;
                }
                Some(_) => {
                    report.files_modified += 1;
                    to_reindex.push((
                        file.relative_path.clone(),
                        file.absolute_path.clone(),
                        file.kind,
                        new_meta,
                    ));
                }
                None => {
                    report.files_added += 1;
                    to_reindex.push((
                        file.relative_path.clone(),
                        file.absolute_path.clone(),
                        file.kind,
                        new_meta,
                    ));
                }
            }
        }

        // Files that disappeared from disk.
        let removed: Vec<String> = metadata
            .files
            .keys()
            .filter(|k| !on_disk.contains(*k))
            .cloned()
            .collect();
        report.files_removed = removed.len();

        // Apply deletions FIRST so re-indexed files don't accumulate stale rows.
        let to_delete_paths: Vec<String> = removed
            .iter()
            .cloned()
            .chain(
                to_reindex
                    .iter()
                    .map(|(rel, ..)| rel.to_string_lossy().to_string()),
            )
            .collect();
        if !to_delete_paths.is_empty() {
            let refs: Vec<&str> = to_delete_paths.iter().map(String::as_str).collect();
            store.delete_by_source(&refs).await?;
        }
        for r in &removed {
            metadata.files.remove(r);
        }

        // Re-embed the changed/new files.
        for (relative, absolute, kind, mut new_meta) in to_reindex {
            let chunks = chunker.chunk(&relative, &absolute, kind)?;
            if chunks.is_empty() {
                warn!(
                    path = %relative.display(),
                    "chunker produced 0 chunks; skipping",
                );
                continue;
            }
            let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
            let vectors = embedder.embed(texts, None)?;
            store.append_chunks(&chunks, &vectors).await?;
            new_meta.chunk_count = chunks.len();
            report.chunks_indexed += chunks.len();
            metadata.upsert(relative, new_meta);
        }

        metadata.save(self.metadata_path)?;

        info!(
            total = report.files_total,
            unchanged = report.files_unchanged,
            added = report.files_added,
            modified = report.files_modified,
            removed = report.files_removed,
            chunks = report.chunks_indexed,
            "delta-sync complete"
        );
        Ok(report)
    }
}
