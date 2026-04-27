//! Watcher consumer — debounces filesystem events and triggers delta-syncs.
//!
//! Consumes [`CorpusEvent`]s coming out of the [`crate::ports::Watcher`]
//! adapter and delegates the actual reconciliation to [`DeltaSync`]. The
//! debounce loop is split into [`debounce_batch`] (testable in isolation
//! under `tokio::time::pause()`); the wrapper handles flushing.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{error, info};

use crate::app::delta_sync::DeltaSync;
use crate::domain::CorpusEvent;

/// Drain incoming [`CorpusEvent`]s; flush a delta-sync after `debounce_window`
/// of quiet. Exits when the event channel closes.
pub async fn run_watcher_consumer(
    sync: DeltaSync,
    debounce_window: Duration,
    mut events: mpsc::Receiver<CorpusEvent>,
) {
    while let Some(batch) = debounce_batch(&mut events, debounce_window).await {
        info!(events = batch.len(), "watcher batch flushing delta-sync");
        match sync.run().await {
            Ok(report) => info!(?report, "watcher-triggered delta-sync complete"),
            Err(e) => error!(error = %e, "watcher-triggered delta-sync failed"),
        }
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
        match timeout(remaining, events.recv()).await {
            Ok(Some(ev)) => {
                batch.push(ev);
                deadline = Instant::now() + window;
            }
            Ok(None) | Err(_) => return Some(batch),
        }
    }
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
    use tokio::time::advance;

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
        advance(window + Duration::from_millis(10)).await;

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
        advance(Duration::from_millis(100)).await;
        tx.send(made_event()).await.unwrap();
        advance(Duration::from_millis(100)).await;
        tx.send(made_event()).await.unwrap();

        // Now wait the full window of quiet.
        advance(window + Duration::from_millis(10)).await;

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
        advance(Duration::from_millis(100)).await;
        drop(tx);

        let result = handle.await.unwrap();
        let batch = result.unwrap();
        assert_eq!(batch.len(), 2, "partial batch flushes on channel close");
    }
}
