---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0010 — Filesystem watcher uses `kqueue` on macOS

> **Y-statement** — In the context of needing to **react to in-session
> filesystem changes** (operator edits a doc while Claude Code +
> schema are alive; the index should reflect the change without a
> daemon restart), facing the choice between (a) **`FSEvents`**
> (macOS's recursive directory-event API; ~100-500 ms latency;
> directory-level granularity; opens 1 file descriptor for an entire
> tree), (b) **`kqueue`** (per-FD push-based; ~10 ms latency; file-
> level granularity; opens 1 FD per file + 1 per directory), or (c)
> **polling** (stat() loop; CPU-wasteful; only justified for
> network/pseudo filesystems), we decided for **`kqueue`** on macOS
> via `notify` 8.2's `macos_kqueue` feature, with `inotify` as the
> automatic Linux backend, against `FSEvents` (lower latency and
> file-level granularity matter for our delta-sync) or polling
> (zero use case here), to achieve sub-15 ms event propagation that
> tells the delta-sync layer exactly which file changed (not just
> which directory), accepting that `kqueue` opens one FD per
> watched file + one per directory (~150 FDs for a Lowcow-sized
> corpus, well below the 256/1024 default `ulimit -n` on macOS) and
> that the `notify` API documented FSEvents as the platform default
> — switching to `kqueue` is a deliberate feature-flag override.

## Context and Problem Statement

Within a single Claude Code session, the operator may edit a doc
file. The delta-sync manifest (ADR-0007) handles edits *between*
sessions; *within* a session we need a watcher that triggers a
re-embed of the changed file as soon as the editor saves.

`notify = "8.2"` is the canonical Rust crate. On macOS it offers
two backends:

- **FSEvents** — Apple's high-level API. Default in `notify`.
- **kqueue** — BSD-derived per-FD event API.

On Linux it uses **inotify** automatically (no feature flag).

Three concerns shape the choice:

1. **Latency.** How fast does the daemon see the change?
2. **Granularity.** Does the event tell us *which file* changed,
   or only that *something inside this directory* changed?
3. **Resource cost.** FDs open per watched file/directory.

## Decision Drivers

- **Delta-sync precision.** The narrower the event signal, the
  smaller the re-embed work. File-level events (kqueue) tell us
  exactly which file to re-hash; directory-level events
  (FSEvents) require us to walk the directory + diff against
  the manifest.
- **User-perceived latency.** Operator hits save → expects
  schema's index to reflect the new content within a few seconds
  at most. 100-500 ms (FSEvents) vs 10 ms (kqueue) is a small
  absolute difference, but the latter feels "instant" in
  conversational AI workflows.
- **Bounded FD usage.** Lowcow-platform has ~150 indexable files.
  macOS default `ulimit -n` is 256 (sometimes 1024); 150 FDs
  fits comfortably.

## Considered Options

### Option A — FSEvents (default in `notify`) (rejected)

Pro: 1 FD for an entire recursive watch; battery-friendlier.
Con: directory-level granularity (event tells us "something in
`docs/decisions/` changed" — we then walk the directory to find
out which file). Higher latency (100-500 ms; kernel batches /
coalesces events).

### Option B — kqueue (chosen)

Pro: 10 ms latency; file-level events; works on BSD too. Con: 1
FD per file + 1 per directory. ~150 FDs for our corpus. Slightly
higher CPU/battery from more syscalls.

### Option C — polling (rejected)

Pro: works on any filesystem (NFS, /proc, /sys). Con: CPU-
wasteful; not useful for our workload. `notify`'s `PollWatcher`
is opt-in and would be a regression.

## Decision Outcome

Chosen option: **B — kqueue** on macOS.

### Cargo.toml feature config

```toml
[target.'cfg(target_os = "macos")'.dependencies.notify]
version = "8.2"
default-features = false
features = ["macos_kqueue"]

[target.'cfg(not(target_os = "macos"))'.dependencies]
notify = "8.2"
```

`default-features = false` opts out of FSEvents. On non-macOS
targets (Linux primarily) we accept `notify`'s defaults (which
auto-select inotify on Linux, since `inotify` is built-in to
`notify` and not exposed as a feature flag in 8.x).

### Watcher wrapping

`src/corpus/watcher.rs` constructs `notify::KqueueWatcher` on
macOS and uses `notify::recommended_watcher` on other platforms.
Events are forwarded via a Tokio mpsc channel to the retrieval
layer.

### Future reconsideration

If a consumer project ever indexes more than ~1000 files, the
default `ulimit -n` may bite. Two paths:

1. Document `ulimit -n 4096` in the runbook and ask consumers
   to adjust.
2. Switch to FSEvents for that consumer (single feature flag
   in their fork or via `--watcher fsevents` CLI override —
   ADR amendment).

The current tool serves spec-only repos with hundreds of files.
1000+ is FASE 2 territory and warrants a fresh ADR.

## Consequences

- **Good:** sub-15 ms event-to-decision latency; the index
  reflects an editor save almost instantly.
- **Good:** file-level events let the delta-sync re-embed
  precisely what changed; no directory-walk diffing.
- **Good:** kqueue is also the BSD watcher, so cross-BSD
  portability comes free.
- **Bad:** ~150 FDs open during a session. Within macOS's default
  `ulimit -n`; flagged in the runbook to bump if a consumer's
  corpus grows.
- **Bad:** a future macOS minor that quirks kqueue (rare; kqueue
  is a stable kernel API since FreeBSD/Darwin merge) requires us
  to flip the feature flag back to `macos_fsevent`.
- **Neutral:** battery cost is marginally higher than FSEvents;
  on a Mac doing development work the laptop is plugged in
  anyway.

## Fitness function

- `Cargo.toml`'s `[target.'cfg(target_os = "macos")']` block
  pins `macos_kqueue`; CI on macOS exercises that path. A drift
  to `macos_fsevent` would be visible in any PR diff.
- The watcher loop in `src/corpus/watcher.rs` is unit-testable
  by feeding it synthetic events; integration test (FASE 1.1)
  writes a fixture file, observes the event arriving on the
  channel, and asserts latency below 50 ms.

## More information

- `src/corpus/watcher.rs` — wrapping.
- `Cargo.toml` — feature config.
- ADR-0007 — delta-sync layer (the consumer of watcher events).
- `notify` docs: <https://docs.rs/notify/8.2.0/notify/>
