---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0007 — Delta-sync at startup (no in-process index)

> **Y-statement** — In the context of `schema` running as a per-session
> subprocess that must reflect the **current** state of the consumer
> project's docs (the operator may have edited files between sessions
> with the daemon dead), facing the choice between (a) **full re-index
> on every spawn** (slow but always fresh; takes 30-60 s for ~3 K
> chunks), (b) **in-memory only** (no persistent cache; same slow
> startup but zero disk), or (c) **delta-sync at startup** (compare
> file mtime + BLAKE3 content hash against a persisted manifest;
> re-embed only what changed; prune what disappeared), we decided for
> **option C — delta-sync** with the manifest at `metadata.toml` next
> to the LanceDB store, against full re-index (60 s startup is too
> long for a per-session daemon) or in-memory-only (same cold start
> cost on every session), to achieve fresh-on-startup correctness in
> 1-3 s for a typical edit pass while keeping the index persistent
> across sessions, accepting the manifest format as a private
> compatibility surface (bumping the version requires migration code)
> and the small read-amplification on startup (`stat()` + hash every
> declared file) — both bounded by `retrieval.file_size_max`.

## Context and Problem Statement

Each Claude Code session opens → spawns `schema` → daemon must serve
queries within seconds. The daemon dies when the session closes; the
operator may then edit docs before opening the next session. On the
next spawn:

- Some files unchanged → don't re-embed.
- Some files modified → re-embed.
- Some files added → embed.
- Some files removed → prune.

This is exactly the shape of a **delta-sync** problem.

## Decision Drivers

- **Startup latency.** Embedding 3 K chunks on a Mac: ~30-60 s.
  Unacceptable per session. Delta-sync of an unchanged corpus: <1 s.
- **Correctness.** The user expects "what they last edited" to be
  searchable. Stale index is the worst failure mode for a doc-RAG.
- **Disk pressure.** LanceDB store ~50 MB per Lowcow-sized project.
  Acceptable on macOS.

## Considered Options

### Option A — full re-index on every spawn (rejected)

Pro: simplest. Con: 30-60 s startup tax; embedding a 2 GB model and
running it across all chunks is expensive even on M-series.

### Option B — in-memory only (rejected)

Pro: no persistence layer to manage. Con: same 30-60 s tax. Adds
no value over Option A.

### Option C — delta-sync (chosen)

Persisted manifest at `<cache-dir>/metadata.toml`:

```toml
version = 1

[files."docs/decisions/0001-foo.md"]
mtime = 1740000000
size_bytes = 4096
content_hash = "9f8e..."
chunk_count = 3
```

On startup:

1. Walk every `[[corpus]]` declared in `schema.toml`.
2. For each file, compute mtime + BLAKE3 content hash.
3. Compare against `metadata.toml`:
   - **unchanged** (hash match) → skip.
   - **modified** (hash differs) → re-embed; replace chunks in
     LanceDB.
   - **new** (no manifest entry) → embed.
   - **removed** (manifest entry exists, file gone) → prune.
4. Persist updated `metadata.toml`.

Implication: 1-3 s startup for unchanged corpus; 5-10 s if a few
files changed; ~30 s only when many files changed.

## Decision Outcome

Chosen option: **C — delta-sync at startup**.

### Manifest layout

```text
~/.cache/schema/projects/<id>/
├── lance/                  # LanceDB store
└── metadata.toml           # delta-sync manifest
```

`metadata.toml` versioned. Future migrations (FASE 1.1) add a
`from_v0_to_v1` step before parsing.

### Hashing

BLAKE3 of the full file contents. Why BLAKE3 over SHA-256:

- ~5× faster than SHA-256 on M-series.
- Cryptographically strong; no collision risk for our use.
- Already on the dep list (used for project_id in ADR-0008).

mtime is the cheap pre-check; hash is authoritative.

### Pruning

Files in `metadata.toml` but absent from disk get their chunks
deleted from LanceDB by `source_path` match. Then their manifest
entry is removed.

Files about to be re-indexed first have their old chunks deleted
by `source_path` match (so the index never doubles up on rolling
edits).

### Failure modes

- `metadata.toml` missing → treat as fresh index; embed everything.
- `metadata.toml` corrupt → log + treat as fresh index. The
  LanceDB store is rebuilt; 30-60 s one-off cost.
- LanceDB corrupt → out of scope for this ADR; FASE 1.1 will add
  `schema doctor` to detect + offer rebuild.

## Consequences

- **Good:** unchanged corpus = sub-3-second startup.
- **Good:** any edit between sessions is reflected on the next
  spawn — "tenho certeza do q eh" satisfied without paying the
  full re-index cost (the operator's stated goal).
- **Good:** the watcher (ADR-0010) handles in-session edits. The
  manifest only matters across sessions.
- **Bad:** `metadata.toml` is a compatibility surface. Schema
  changes need migration. Mitigated by `version` field.
- **Bad:** read-amplification on startup: `stat()` + read +
  BLAKE3 every declared file. Bounded by `retrieval.file_size_max`
  (5 MiB default). For Lowcow-platform: <500 ms total on M-series.

## Fitness function

- `metadata::roundtrip_metadata` unit test asserts the TOML
  serialisation round-trips correctly.
- `DeltaSync::run` returns a `SyncReport` with per-bucket counts;
  surfaced in `tracing::info!` so operators see exactly what a
  startup did.
- A future integration test (FASE 1.1) writes a fixture project,
  runs delta-sync, modifies a file, runs again, and asserts only
  the modified file is re-embedded.

## More information

- `src/retrieval/metadata.rs` — manifest format + load/save.
- `src/retrieval/sync.rs` — orchestrator.
- ADR-0008 — cache isolation (the manifest lives in the per-project
  cache dir).
- ADR-0010 — file watcher (handles edits *during* a session).
