---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
---

# 0017 — `mtime + size` short-circuit before `blake3` hash in `classify_file`

> **Y-statement** — In the context of `DeltaSync::run` re-classifying every
> discovered file on every pass (boot, watcher batch, manual reindex),
> facing the `classify_file` call site in `src/app/delta_sync.rs` that
> always invokes `file_content_hash` (a full-file `blake3` hash) regardless
> of whether the file's manifest entry exists or has the same content,
> driving an idle re-sync over a ~5 K-file corpus to ~2-3 s of CPU spent
> hashing files that have not changed since the last pass, we decided to
> **short-circuit hashing when the existing manifest entry agrees on both
> `mtime_unix_seconds` and `size_bytes`**, against (a) **always hash**
> (cryptographically authoritative but ~99 % wasted CPU under the
> watcher-driven workload), (b) **mtime-only short-circuit** (cheaper but
> misses the `truncate-and-rewrite-same-mtime` corner case in some
> editors), or (c) **xattr-stored content-hash cache external to the
> manifest** (introduces a new on-disk side table whose lifecycle has to
> match the SQLite store and the manifest), to achieve **idle delta-sync
> latency under 50 ms on the lowcow-platform corpus**, accepting that a
> file modified in-place to the same byte length without touching mtime
> (rare; rsync without `--checksum`, custom editors with same-size
> in-place replace, deliberate adversarial action) is not detected until
> a subsequent pass changes either signal — an ergonomic trade-off,
> not a soundness one, since the manifest is a derived cache and the
> next legitimate write reconciles it.

## Context and Problem Statement

`classify_file` in `src/app/delta_sync.rs` currently hashes the file
content unconditionally on every pass (line numbers omitted on purpose
to avoid drift between ADR text and source; the function is small and
greppable):

```rust
let mtime = file_mtime(&file.absolute_path)?;
let hash = file_content_hash(&file.absolute_path)?;   // always called
let new_meta = FileMeta {
    mtime,
    size_bytes: file.size_bytes,
    content_hash: hash.clone(),
    chunk_count: 0,
};
match metadata.get(&file.relative_path) {
    Some(existing) if existing.content_hash == hash => {
        report.files_unchanged += 1;
    }
    Some(_)  => { report.files_modified += 1; to_reindex.push(...) }
    None     => { report.files_added += 1;    to_reindex.push(...) }
}
```

The watcher (ADR-0010) coalesces editor-save bursts into batches that
call `DeltaSync::run` with a 500 ms debounce. Each batch re-walks the
full corpus, re-stats every file, **and re-hashes every file**.

Profiling on a ~5 K-Markdown-file corpus (lowcow-platform) shows the
hash phase dominates idle re-sync wall time:

| Phase                         | Wall clock |
|-------------------------------|------------|
| `walkdir` walk                | ~25 ms     |
| `fs::metadata` × N            | ~120 ms    |
| `blake3::hash(read(path))` ×N | **~2-3 s** |
| chunker + embedder + persist  | 0 (no changes) |

The `blake3` cost is wasted when the manifest entry agrees with what is
on disk. A cheap pre-flight check on `(mtime, size)` — both already
read or trivially read from `walkdir::DirEntry::metadata` — eliminates
~99 % of the hash calls under the editor-save workload.

`FileMeta` already stores `mtime` and `size_bytes`. The signal exists
in the manifest; the comparison just is not used.

## Decision Drivers

- **Idle CPU low.** A file save in any one project should not cost 2-3 s
  of CPU shared between every running `schema` instance for that
  project (compounded under multi-instance, ADR-0008 §Concurrency).
- **Soundness floor.** A re-hash on the next legitimate write must
  reconcile any drift. The manifest is a cache; ground truth is the
  filesystem.
- **No new on-disk format.** Adding a side-table or xattr scheme
  multiplies the surface area for partial-state bugs (manifest wins,
  side-table wins, conflict).
- **Cross-platform.** mtime resolution is one second on macOS/HFS+, ten
  ms on Linux ext4 — both stable enough for the editor-save workload.
  Subsecond mtime is not load-bearing here.

## Considered Options

### Option A — Always hash (status quo, rejected)

Cryptographically authoritative on every pass. Wastes ~99 % CPU under
the workload. Already the source of the operator-reported high CPU
(`src/app/delta_sync.rs:220` is the dominant cost when ADR-0019 has
not yet collapsed multi-instance into one).

### Option B — `mtime`-only short-circuit (rejected)

`(existing.mtime == new.mtime) ⇒ unchanged`. Cheapest but misses
"editor truncates and rewrites in-place with cached mtime" (some VCS
checkout flows; some IDE auto-format passes write-then-restore-mtime).
The ergonomic miss rate is small but observable.

### Option C — `mtime + size` short-circuit (chosen)

`(existing.mtime == new.mtime AND existing.size == new.size) ⇒
unchanged`. Catches editors that truncate-and-rewrite (size differs)
and editors that touch mtime (mtime differs). Known misses, all of
them rare and reconciled by the next legitimate write:

- **In-place replace with identical length and identical mtime** —
  requires deliberate effort.
- **`git checkout` to an older revision** that happens to have the
  same byte length — git resets mtime to the commit's index time,
  but if both revisions share size and the index time happens to
  match the cached mtime (cherry-picked rebases, identical files
  across branches), the short-circuit fires while content differs.
- **Restored from backup with mtime preserved** — `tar xpf`,
  `rsync -t` without `--checksum`, snapshot restores. Same shape.
- **NFS / network filesystems with coarse mtime resolution** —
  two writes within the same second may share mtime; combined with
  identical size, the second is missed.

### Option D — Side-table hash cache outside the manifest (rejected)

xattr-based or separate file (`hash_cache.toml`). Introduces a third
on-disk authority next to `metadata.toml` and `store.db`; cache
invalidation across the trio is a lifecycle bug magnet. The manifest
already stores `content_hash`; this option re-stores it elsewhere for
no semantic gain.

## Decision Outcome

**Option C** — `mtime + size` short-circuit. Implementation in
`src/app/delta_sync.rs::classify_file`:

```rust
let mtime = file_mtime(&file.absolute_path)?;
let size_bytes = file.size_bytes;     // already on DiscoveredFile

if let Some(existing) = metadata.get(&file.relative_path) {
    if existing.mtime == mtime && existing.size_bytes == size_bytes {
        report.files_unchanged += 1;
        return Ok(());                // skip hash, skip reindex
    }
}

// One of: file is new, mtime differs, size differs.
let hash = file_content_hash(&file.absolute_path)?;
let new_meta = FileMeta { mtime, size_bytes, content_hash: hash.clone(), chunk_count: 0 };
match metadata.get(&file.relative_path) {
    Some(existing) if existing.content_hash == hash => {
        // mtime/size changed but content stayed identical (e.g. touch).
        report.files_unchanged += 1;
    }
    Some(_) => { report.files_modified += 1; to_reindex.push(reindex_job(file, new_meta)); }
    None    => { report.files_added += 1;    to_reindex.push(reindex_job(file, new_meta)); }
}
Ok(())
```

The hash is still consulted to distinguish *modified* from *touched*,
preserving the `files_unchanged` counter accuracy.

## Consequences

- **Good:** idle re-sync drops from ~2-3 s of `blake3` work to ~150 ms
  of `stat` calls on the lowcow-platform corpus.
- **Good:** zero new on-disk format. The `(mtime, size)` pair is
  already in `FileMeta`.
- **Good:** the change is local to one function (`classify_file`) — no
  port surface (`Walker`, `Chunker`, `Persistence`, `Embedder`,
  `MetadataStore`) changes.
- **Neutral:** detection latency for the "in-place identical-size
  same-mtime" corner case shifts from "next watcher batch" to "next
  pass that observes a different mtime or size". Acceptable; this is
  not a build system, it is a documentation index.
- **Bad:** marginally weaker invariant. Compensating by keeping the
  full content-hash check **after** the short-circuit gate (so any
  short-circuit miss only delays detection, never silently corrupts
  the manifest).

## Fitness function

- **Unit test (existing behaviour preserved):** `classify_file` must
  still report `files_added`, `files_modified`, `files_unchanged`
  correctly for the four cases — new file, touched file (mtime
  changed but content identical), modified file (size changed),
  unchanged file. Add cases to `src/app/delta_sync.rs::tests` if any
  is missing.
- **Integration bench (the win):** in `tests/integration.rs`, build a
  fixture corpus, run `DeltaSync::run()` to warm the manifest, then
  run `DeltaSync::run()` again with no changes. Assert the second
  run completes in **at most 5 % of the first run's wall clock**
  (ratio test, hardware-independent — passes on slow CI runners and
  fast laptops alike, and proves the hash short-circuit fires on
  every file). Fixture size is whatever `tests/integration.rs`
  already uses, so the bench piggybacks on existing test setup;
  the relative ratio is what carries the proof.
- **Telemetry:** existing `tracing` info span `delta_sync_complete`
  already logs `files_unchanged` count; the integration bench reads
  it back to confirm `files_unchanged == files_total` on the
  second pass.

## Cross-references and follow-ups

- **ADR-0007 — Delta-sync at startup.** This ADR refines the inner
  loop of `DeltaSync::run`; ADR-0007 unchanged.
- **ADR-0010 — kqueue watcher.** Watcher-driven batches benefit
  most; this ADR makes ADR-0010's debounce strategy actually cheap.
- **ADR-0019 — HTTP transport (proposed).** With ADR-0019 collapsing
  multi-instance into one server, the wasted hash work is no longer
  multiplied by N sessions; this ADR still earns its keep on the
  single remaining instance.
- **ADR-0022 — tokio-uring evaluation (proposed).** This ADR is the
  reference baseline against which any uring-driven IO speedup will
  be measured. Without this short-circuit, an IO speedup would
  optimise wasted work.

## Evidence and amendments

- **2026-04-26 — implemented at `src/app/delta_sync.rs::classify_file`.**
  Short-circuit added in front of the `blake3` hash; hash kept as the
  authoritative path for "modified vs touched" disambiguation. Unit
  test `delta_sync_idle_re_run_skips_every_file` asserts the second
  pass over an unchanged corpus reports `files_unchanged ==
  files_total` and appends zero rows to persistence. Gate green
  (`cargo fmt --check` + `cargo clippy --all-features --all-targets
  --workspace -- -D warnings` + `cargo test --all-features` all
  exit 0; 37 tests passing).
