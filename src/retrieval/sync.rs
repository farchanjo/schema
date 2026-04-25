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
//!
//! [`run_watcher_consumer`] consumes [`CorpusEvent`]s from the filesystem
//! watcher (ADR-0010) and triggers a debounced delta-sync, so in-session
//! edits are reflected in the index within ~500 ms-2 s of an editor save.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

use crate::config::SchemaConfig;
use crate::corpus::{Chunker, CorpusEvent, Walker};
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

#[derive(Debug)]
pub struct DeltaSync<'a> {
    pub config: &'a SchemaConfig,
    pub project_root: &'a Path,
    pub metadata_path: &'a Path,
}

impl DeltaSync<'_> {
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

// ─── Watcher consumer ────────────────────────────────────────────────────

/// Inputs the watcher consumer needs to run a delta-sync after a debounced
/// burst of [`CorpusEvent`]s.
#[derive(Debug, Clone)]
pub struct WatcherConsumerInputs {
    pub config: SchemaConfig,
    pub project_root: PathBuf,
    pub metadata_path: PathBuf,
    pub store: Arc<VectorStore>,
    pub embedder: Arc<Mutex<Embedder>>,
    pub debounce_window: Duration,
}

/// Drain incoming [`CorpusEvent`]s; flush a delta-sync after `debounce_window`
/// of quiet. Exits when the event channel closes.
///
/// The debounce loop is split into [`debounce_batch`] (testable in
/// isolation under `tokio::time::pause()`); this wrapper handles flushing.
pub async fn run_watcher_consumer(
    inputs: WatcherConsumerInputs,
    mut events: mpsc::Receiver<CorpusEvent>,
) {
    while let Some(batch) = debounce_batch(&mut events, inputs.debounce_window).await {
        info!(events = batch.len(), "watcher batch flushing delta-sync");
        flush(&inputs).await;
    }
    info!("watcher channel closed; consumer exiting");
}

/// Pure debounce loop. Returns the accumulated batch once `window` of quiet
/// elapses, or `None` when the channel closes before any event arrives.
///
/// Burst handling: every event resets the deadline; we flush only after
/// `window` of total quiet. A pathologically constant event stream keeps
/// extending the deadline; we accept that and rely on real-world editor
/// saves being bursty rather than continuous.
pub async fn debounce_batch(
    events: &mut mpsc::Receiver<CorpusEvent>,
    window: Duration,
) -> Option<Vec<CorpusEvent>> {
    let first = events.recv().await?;
    let mut batch = vec![first];
    let mut deadline = Instant::now() + window;

    loop {
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Some(batch);
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(ev)) => {
                batch.push(ev);
                deadline = Instant::now() + window;
            }
            Ok(None) => return Some(batch),
            Err(_elapsed) => return Some(batch),
        }
    }
}

async fn flush(inputs: &WatcherConsumerInputs) {
    let sync = DeltaSync {
        config: &inputs.config,
        project_root: &inputs.project_root,
        metadata_path: &inputs.metadata_path,
    };
    let mut emb_guard = inputs.embedder.lock().await;
    let store: &VectorStore = &inputs.store;
    let result = sync.run(store, &mut emb_guard).await;
    drop(emb_guard);
    match result {
        Ok(report) => info!(?report, "watcher-triggered delta-sync complete"),
        Err(e) => error!(error = %e, "watcher-triggered delta-sync failed"),
    }
}

/// Helper for callers (`main::run_serve`) that need the absolute paths to
/// hand to [`crate::corpus::CorpusWatcher::new`].
#[must_use]
pub fn corpus_paths_from_config(config: &SchemaConfig, project_root: &Path) -> Vec<PathBuf> {
    config
        .corpus
        .iter()
        .map(|c| project_root.join(&c.path))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::*;
    use std::path::PathBuf;
    use tokio::sync::mpsc::channel;

    fn made_event() -> CorpusEvent {
        CorpusEvent::Modified(PathBuf::from("/dev/null"))
    }

    /// `debounce_batch` returns `None` when the channel closes before any
    /// event arrives, signalling the consumer should exit.
    #[tokio::test(start_paused = true)]
    async fn returns_none_when_channel_closed_before_first_event() {
        let (tx, mut rx) = channel::<CorpusEvent>(8);
        drop(tx);

        let result = debounce_batch(&mut rx, Duration::from_millis(500)).await;
        assert!(result.is_none(), "closed channel must short-circuit");
    }

    /// A single event is held for the full debounce window before being
    /// returned.
    #[tokio::test(start_paused = true)]
    async fn flushes_single_event_after_window() {
        let (tx, mut rx) = channel::<CorpusEvent>(8);
        tx.send(made_event()).await.unwrap();
        // Don't drop tx — keep the channel open so timeout fires (not close).

        let window = Duration::from_millis(500);
        let handle = tokio::spawn(async move { debounce_batch(&mut rx, window).await });

        // Advance virtual time past the debounce window.
        tokio::time::advance(window + Duration::from_millis(10)).await;

        let result = handle.await.unwrap();
        let batch = result.unwrap();
        assert_eq!(batch.len(), 1, "single event flushes as a 1-event batch");
    }

    /// A burst of events resets the deadline on each new event; the flush
    /// only fires after `window` of quiet AFTER the last event.
    #[tokio::test(start_paused = true)]
    async fn coalesces_burst_into_one_batch() {
        let (tx, mut rx) = channel::<CorpusEvent>(8);
        let window = Duration::from_millis(500);

        let handle = tokio::spawn(async move { debounce_batch(&mut rx, window).await });

        // Fire three events 100 ms apart; all should land in the same batch.
        tx.send(made_event()).await.unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        tx.send(made_event()).await.unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        tx.send(made_event()).await.unwrap();

        // Now wait the full window of quiet.
        tokio::time::advance(window + Duration::from_millis(10)).await;

        let result = handle.await.unwrap();
        let batch = result.unwrap();
        assert_eq!(batch.len(), 3, "all 3 events coalesce into one batch");
    }

    /// Channel closing mid-batch flushes whatever has accumulated.
    #[tokio::test(start_paused = true)]
    async fn flushes_partial_batch_when_channel_closes() {
        let (tx, mut rx) = channel::<CorpusEvent>(8);
        let window = Duration::from_millis(500);

        let handle = tokio::spawn(async move { debounce_batch(&mut rx, window).await });

        tx.send(made_event()).await.unwrap();
        tx.send(made_event()).await.unwrap();
        // Advance a bit (still inside window), then close the channel.
        tokio::time::advance(Duration::from_millis(100)).await;
        drop(tx);

        let result = handle.await.unwrap();
        let batch = result.unwrap();
        assert_eq!(batch.len(), 2, "partial batch flushes on channel close");
    }
}
