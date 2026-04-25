//! Filesystem adapters — `WalkdirWalker` (sync corpus discovery) and
//! `NotifyWatcher` (async filesystem events).
//!
//! Bundled by responsibility per ADR-0013: both adapters drive the local
//! filesystem; keeping them in one module limits the blast radius if the
//! storage substrate changes (e.g. swapping notify for fanotify on Linux).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher as NotifyWatcherTrait};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::toml_config::{Corpus, SchemaConfig};
use crate::domain::{CorpusEvent, CorpusKind, DiscoveredFile};
use crate::ports::{Walker, WalkerError, Watcher, WatcherError, WatcherKeepAlive};

// ─── Walker adapter ──────────────────────────────────────────────────────

/// `walkdir`-backed implementation of [`Walker`].
#[derive(Debug)]
pub struct WalkdirWalker {
    config: Arc<SchemaConfig>,
    project_root: PathBuf,
}

impl WalkdirWalker {
    #[must_use]
    pub const fn new(config: Arc<SchemaConfig>, project_root: PathBuf) -> Self {
        Self {
            config,
            project_root,
        }
    }

    fn discover_one(
        &self,
        corpus: &Corpus,
        files: &mut Vec<DiscoveredFile>,
    ) -> Result<(), WalkerError> {
        let absolute = self.project_root.join(&corpus.path);
        if !absolute.exists() {
            return Err(WalkerError::PathNotFound(absolute));
        }

        // Single file → one entry.
        if absolute.is_file() {
            if let Some(file) = self.consider_file(&absolute, corpus.kind)? {
                files.push(file);
            }
            return Ok(());
        }

        let walker = WalkDir::new(&absolute)
            .follow_links(self.config.security.follow_symlinks)
            .into_iter()
            .filter_entry(|e| !self.is_globally_excluded(e.path()));

        for entry in walker {
            let entry = entry.map_err(|e| WalkerError::Io {
                path: absolute.clone(),
                source: e.into(),
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            if Self::matches_per_corpus_exclude(entry.path(), corpus) {
                continue;
            }
            if let Some(file) = self.consider_file(entry.path(), corpus.kind)? {
                files.push(file);
            }
        }
        Ok(())
    }

    fn consider_file(
        &self,
        absolute: &Path,
        kind: CorpusKind,
    ) -> Result<Option<DiscoveredFile>, WalkerError> {
        let metadata = fs::metadata(absolute).map_err(|e| WalkerError::Io {
            path: absolute.to_path_buf(),
            source: e,
        })?;

        let size_bytes = metadata.len();
        if size_bytes > self.config.retrieval.file_size_max {
            tracing::warn!(
                path = %absolute.display(),
                size_bytes,
                limit = self.config.retrieval.file_size_max,
                "file exceeds file_size_max; skipping",
            );
            return Ok(None);
        }

        let relative_path = absolute
            .strip_prefix(&self.project_root)
            .map_or_else(|_| absolute.to_path_buf(), Path::to_path_buf);

        Ok(Some(DiscoveredFile {
            absolute_path: absolute.to_path_buf(),
            relative_path,
            kind,
            size_bytes,
        }))
    }

    fn is_globally_excluded(&self, path: &Path) -> bool {
        let last_component = path.file_name().and_then(|s| s.to_str());
        if let Some(name) = last_component {
            for excluded in &self.config.security.exclude_default {
                if name == excluded {
                    return true;
                }
            }
        }
        false
    }

    fn matches_per_corpus_exclude(path: &Path, corpus: &Corpus) -> bool {
        // Simple substring match for FASE 1.0; full glob support arrives later.
        for pattern in &corpus.exclude {
            if let Some(name) = path.file_name().and_then(|s| s.to_str())
                && name.contains(pattern.trim_start_matches('*').trim_end_matches('*'))
            {
                return true;
            }
        }
        false
    }
}

impl Walker for WalkdirWalker {
    fn discover(&self) -> Result<Vec<DiscoveredFile>, WalkerError> {
        let mut files = Vec::new();
        for corpus in &self.config.corpus {
            self.discover_one(corpus, &mut files)?;
        }
        Ok(files)
    }
}

// ─── Watcher adapter ─────────────────────────────────────────────────────

/// `notify`-backed implementation of [`Watcher`]. Constructed eagerly with
/// the paths it should observe; `start()` consumes the struct, registers
/// every path, and returns the keep-alive + event receiver.
pub struct NotifyWatcher {
    paths: Vec<PathBuf>,
}

impl fmt::Debug for NotifyWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NotifyWatcher")
            .field("paths", &self.paths)
            .finish()
    }
}

impl NotifyWatcher {
    #[must_use]
    pub const fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

impl Watcher for NotifyWatcher {
    fn start(self: Box<Self>) -> Result<(WatcherKeepAlive, Receiver<CorpusEvent>), WatcherError> {
        let (tx, rx) = channel::<CorpusEvent>(256);

        let mut watcher: Box<dyn NotifyWatcherTrait + Send> = make_watcher(tx)
            .map_err(|e| WatcherError::Backend(format!("creating watcher: {e}")))?;

        for p in &self.paths {
            watcher
                .watch(p, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", p.display()))?;
        }

        Ok((WatcherKeepAlive::new(watcher), rx))
    }
}

#[cfg(target_os = "macos")]
fn make_watcher(tx: Sender<CorpusEvent>) -> Result<Box<dyn NotifyWatcherTrait + Send>> {
    use notify::Config;
    use notify::KqueueWatcher;
    let config = Config::default().with_poll_interval(Duration::from_secs(2));
    let watcher = KqueueWatcher::new(move |res| handle_event(res, &tx), config)?;
    Ok(Box::new(watcher))
}

#[cfg(not(target_os = "macos"))]
fn make_watcher(tx: Sender<CorpusEvent>) -> Result<Box<dyn NotifyWatcherTrait + Send>> {
    let _ = Duration::from_secs(0); // suppress unused on non-macOS
    let watcher = notify::recommended_watcher(move |res| handle_event(res, &tx))?;
    Ok(Box::new(watcher))
}

fn handle_event(res: notify::Result<Event>, tx: &Sender<CorpusEvent>) {
    match res {
        Ok(event) => {
            for path in event.paths {
                let kind_event = match event.kind {
                    EventKind::Create(_) => CorpusEvent::Created(path),
                    EventKind::Modify(_) => CorpusEvent::Modified(path),
                    EventKind::Remove(_) => CorpusEvent::Removed(path),
                    EventKind::Any | EventKind::Access(_) | EventKind::Other => continue,
                };
                if let Err(e) = tx.try_send(kind_event) {
                    debug!(error = %e, "watcher channel full; dropping event");
                }
            }
        }
        Err(e) => warn!(error = %e, "notify watcher error"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]
    use super::*;
    use crate::adapters::toml_config::{
        Corpus, EmbeddingConfig, ProjectMeta, RetrievalConfig, SecurityConfig,
    };
    use std::fs;
    use tempfile::TempDir;

    fn write(p: &Path, contents: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn cfg_with(corpus: Vec<Corpus>) -> SchemaConfig {
        SchemaConfig {
            project: ProjectMeta {
                name: "demo".into(),
                version: "1".into(),
            },
            corpus,
            embedding: EmbeddingConfig::default(),
            retrieval: RetrievalConfig::default(),
            security: SecurityConfig::default(),
        }
    }

    #[test]
    fn discovers_single_corpus_entry() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("docs/decisions/0001-test.md"), "# ADR-0001");
        write(&root.join("docs/decisions/0002-test.md"), "# ADR-0002");
        write(&root.join("docs/decisions/template.md"), "# template");

        let cfg = cfg_with(vec![Corpus {
            path: PathBuf::from("docs/decisions"),
            kind: CorpusKind::AdrMadr,
            exclude: vec!["template".into()],
        }]);

        let walker = WalkdirWalker::new(Arc::new(cfg), root.to_path_buf());
        let files = walker.discover().unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.kind == CorpusKind::AdrMadr));
        assert!(
            !files
                .iter()
                .any(|f| f.relative_path.to_string_lossy().contains("template"))
        );
    }

    #[test]
    fn skips_files_above_size_limit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("docs/big.md"), &"x".repeat(2_000));

        let mut cfg = cfg_with(vec![Corpus {
            path: PathBuf::from("docs"),
            kind: CorpusKind::Markdown,
            exclude: vec![],
        }]);
        cfg.retrieval.file_size_max = 1_000;

        let files = WalkdirWalker::new(Arc::new(cfg), root.to_path_buf())
            .discover()
            .unwrap();
        assert!(files.is_empty(), "big file should be skipped");
    }

    #[test]
    fn excludes_default_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("docs/a.md"), "ok");
        write(&root.join("docs/node_modules/bad.md"), "skip");

        let cfg = cfg_with(vec![Corpus {
            path: PathBuf::from("docs"),
            kind: CorpusKind::Markdown,
            exclude: vec![],
        }]);

        let files = WalkdirWalker::new(Arc::new(cfg), root.to_path_buf())
            .discover()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].relative_path.ends_with("a.md"));
    }
}
