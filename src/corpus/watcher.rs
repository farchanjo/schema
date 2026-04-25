//! Filesystem watcher.
//!
//! On macOS, uses `kqueue` (per ADR-0010); on Linux, uses the default `notify`
//! backend (`inotify`). Emits [`CorpusEvent`]s on a Tokio mpsc channel; the
//! retrieval layer (commit 6) consumes these and re-embeds affected files
//! incrementally.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub enum CorpusEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
}

/// Wraps a `notify` watcher and forwards events to a Tokio mpsc receiver.
pub struct CorpusWatcher {
    /// Owns the underlying notify watcher; dropping this stops the watch.
    inner: Box<dyn Watcher + Send>,
    pub events: Receiver<CorpusEvent>,
}

/// Owned handle to the notify watcher. Holding this alive keeps the watch
/// running. Move it into a long-lived task / struct; drop it to stop the
/// watcher.
pub struct WatcherKeepAlive {
    #[expect(
        dead_code,
        reason = "field exists solely to extend the watcher's lifetime; \
                  dropping this handle stops the underlying notify watcher"
    )]
    inner: Box<dyn Watcher + Send>,
}

impl std::fmt::Debug for WatcherKeepAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatcherKeepAlive")
            .field("inner", &"<dyn Watcher>")
            .finish()
    }
}

impl std::fmt::Debug for CorpusWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CorpusWatcher")
            .field("inner", &"<dyn Watcher>")
            .field("events", &"<mpsc::Receiver<CorpusEvent>>")
            .finish()
    }
}

impl CorpusWatcher {
    /// Split into a `WatcherKeepAlive` (holds the notify watcher alive) and
    /// the event receiver. The caller must keep both alive — typically by
    /// moving them into the same long-lived task.
    pub fn into_parts(self) -> (WatcherKeepAlive, Receiver<CorpusEvent>) {
        (WatcherKeepAlive { inner: self.inner }, self.events)
    }
}

impl CorpusWatcher {
    /// Construct a watcher that observes every `path` recursively. Uses the
    /// platform-recommended backend (`kqueue` on macOS per ADR-0010, `inotify`
    /// on Linux).
    pub fn new(paths: &[&Path]) -> Result<Self> {
        let (tx, rx) = channel::<CorpusEvent>(256);

        let mut watcher: Box<dyn Watcher + Send> = make_watcher(tx)?;

        for p in paths {
            watcher
                .watch(p, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", p.display()))?;
        }

        Ok(Self {
            inner: watcher,
            events: rx,
        })
    }
}

#[cfg(target_os = "macos")]
fn make_watcher(tx: Sender<CorpusEvent>) -> Result<Box<dyn Watcher + Send>> {
    use notify::Config;
    use notify::KqueueWatcher;
    let config = Config::default().with_poll_interval(Duration::from_secs(2));
    let watcher = KqueueWatcher::new(move |res| handle_event(res, &tx), config)?;
    Ok(Box::new(watcher))
}

#[cfg(not(target_os = "macos"))]
fn make_watcher(tx: Sender<CorpusEvent>) -> Result<Box<dyn Watcher + Send>> {
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
                    _ => continue,
                };
                if let Err(e) = tx.try_send(kind_event) {
                    debug!(error = %e, "watcher channel full; dropping event");
                }
            }
        }
        Err(e) => warn!(error = %e, "notify watcher error"),
    }
}
