---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
supersedes: ["ADR-0006"]
---

# 0011 — SQLite + `sqlite-vec` embedded store (supersedes ADR-0006)

> **Y-statement** — In the context of persisting chunks + their bge-m3
> embeddings (ADR-0005, 1024-dim Float32) plus structured metadata so
> the MCP tools can run nearest-neighbour search and metadata
> filtering, facing the choice between (a) **keep LanceDB 0.27**
> (Rust-native, columnar/Arrow, but heavy in deps and compile time
> for the actual corpus size), (b) **`Vec<ChunkRecord>` + bincode**
> (lightest possible; brute-force in RAM; loses concurrent readers
> and hybrid search), (c) **SQLite + `sqlite-vec` + FTS5 in WAL
> mode** (single embedded file; vector + lexical + scalar in one
> engine; MVCC built-in), or (d) **pure-Rust HNSW** crate
> (`instant-distance` / `hora`) (ANN we do not need; persistence
> would be bespoke), we decided for **(c) SQLite + `sqlite-vec` +
> FTS5 + WAL**, one `store.db` per project at
> `~/.cache/schema/projects/<id>/store.db`, against (a) (overkill
> at < 100 K chunks per project; Arrow `RecordBatch` boilerplate
> earns no return), (b) (drops hybrid search and concurrent
> readers from the roadmap), and (d) (ANN unwarranted at this
> scale; loses scalar columns and FTS in one engine), to achieve
> a single-file embedded store that natively covers all three
> roadmap drivers — hybrid lexical + vector search via FTS5,
> Parquet export via DuckDB `ATTACH`, MVCC via WAL — at a fraction
> of the dependency and compile-time cost, accepting that
> `sqlite-vec` is still at 0.x (pinned in `Cargo.toml`), that
> brute-force vector scan is O(N · D) per query (validated under
> 50 ms at 50 K × 1024 — well within budget), and that Parquet
> export becomes one external tool (DuckDB) away instead of
> Arrow-native.

## Context and Problem Statement

ADR-0006 picked LanceDB on three roadmap drivers — hybrid search,
Parquet/Arrow lineage, MVCC for two readers. Within hours of that
decision, a re-evaluation surfaced two facts that shift the
analysis:

1. **The real scale is small.** Lowcow-platform's corpus today
   sits at ~3 K chunks; the documented FASE 1.0 upper bound is
   ~50 K. BGE-M3 produces 1024-dim Float32 vectors. Brute-force
   linear scan at the upper bound is 50 000 × 1024 = ~51 M FMA
   ops per query, which finishes in under 50 ms with SIMD on a
   commodity laptop. **ANN indexes solve a problem we do not
   have.**
2. **All three roadmap drivers are also covered by SQLite.**
   FTS5 + `sqlite-vec` gives hybrid lexical + vector search out
   of the box; DuckDB reads SQLite directly so Parquet export is
   `ATTACH 'store.db'; COPY chunks TO 'x.parquet';`; SQLite WAL
   mode supports N readers + 1 writer concurrently. None of the
   three drivers requires native Arrow lineage.

The cost of LanceDB is real: `lancedb` 0.27 + `arrow-array` +
`arrow-schema` add meaningful compile time, the `RecordBatch`
construction in `build_batch` is verbose enough that ADR-0006
itself flagged it, and the 0.x API surface has churned multiple
times in the past year.

## Decision Drivers

- **Same roadmap, lighter stack.** Hybrid search, Parquet export,
  and concurrent readers without Arrow / DataFusion.
- **Single-file store.** `store.db` per project: reset is `rm`,
  backup is `cp`, inspection is `sqlite3` shell. Fits ADR-0008's
  per-project cache shape.
- **Boring tech.** SQLite is the most-deployed database on Earth;
  `sqlite-vec` (alex garcia) is the maintained successor of
  `sqlite-vss`; `rusqlite` is mature.
- **Compile-time and binary-size discipline.** Removing the
  Arrow stack trims seconds from `cargo build` and tens of MB
  from the release binary, in line with ADR-0001's
  single-binary-deploy invariant.

## Considered Options

### Option A — Keep LanceDB (rejected; superseded)

Pros: already in place; Arrow lineage; first-class ANN. Cons:
ANN unused at our scale; Arrow boilerplate; 0.x API churn risk;
heavy compile time. The drivers it was picked for are equally
served by Option C at much lower cost.

### Option B — `Vec<ChunkRecord>` + bincode (rejected)

Pros: zero new deps beyond `serde` + `bincode`; smallest
possible code surface (~150 LOC store). Cons: drops hybrid
search (no FTS engine), drops concurrent readers (full-file
locking), drops scalar query pushdown (every filter becomes a
linear scan in Rust). The roadmap drivers eliminate this option.

### Option C — SQLite + `sqlite-vec` + FTS5 + WAL (chosen)

```rust
let conn = rusqlite::Connection::open(&store_path)?;
conn.pragma_update(None, "journal_mode", "WAL")?;
sqlite_vec::load(&conn)?; // registers vec0 virtual table
// CREATE VIRTUAL TABLE chunks_vec USING vec0(embedding float[1024]);
// CREATE VIRTUAL TABLE chunks_fts USING fts5(content, title, content='chunks', content_rowid='id');
```

Pros: covers all three roadmap drivers in one engine; mature
host (SQLite); single file; reset/backup are filesystem ops.
Cons: `sqlite-vec` 0.x (pin and review on bump); brute-force
vector scan O(N · D) — within budget at our scale.

### Option D — Pure-Rust HNSW (`instant-distance` / `hora`) (rejected)

Pros: very small; very fast ANN. Cons: ANN we do not need;
persistence DIY; no scalar columns or FTS in the engine — would
have to layer SQLite on top anyway, in which case Option C wins.

## Decision Outcome

Chosen option: **C — SQLite + `sqlite-vec` + FTS5 + WAL**, one
`store.db` per project.

### Schema (single file, three logical tables)

| Object        | Kind                              | Purpose                                                             |
| ------------- | --------------------------------- | ------------------------------------------------------------------- |
| `chunks`      | regular table                     | Scalar columns: `id` PK, `source_path`, `line_start`, `line_end`, `artifact_id` (nullable), `title` (nullable), `kind`, `content`. |
| `chunks_vec`  | virtual table (`sqlite-vec` vec0) | `embedding float[1024]`; rowid joins `chunks.rowid`.                |
| `chunks_fts`  | virtual table (FTS5)              | External-content index on `chunks.content` + `chunks.title`; rowid joins `chunks.rowid`. |
| triggers      | AFTER INSERT / UPDATE / DELETE on `chunks` | Keep `chunks_vec` and `chunks_fts` in sync atomically.       |

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;
```

`synchronous = NORMAL` is the daemon-safe sweet spot for an
embedded write-heavy-on-startup, read-heavy-during-session
workload (no fsync per transaction; WAL fsync at checkpoint).

### Public API of `VectorStore` is preserved

`open`, `ensure_table`, `append_chunks`, `delete_by_source`,
`query_nearest`, `find_by_artifact_id`, `find_mentioning`,
`list_source_paths`. Callers (`sync.rs`, `mcp/server.rs`) are
not touched by this refactor — that is the success criterion.

`find_mentioning` migrates from LanceDB `only_if("content LIKE
'%needle%'")` to plain SQLite `WHERE content LIKE '%needle%'` —
same semantics. The future hybrid-search MCP tool will be added
on top of FTS5 in a separate ADR; it is **not** introduced here.

### Concurrency

WAL mode delivers MVCC: multiple read transactions and one write
transaction can proceed concurrently. The advisory `fd-lock` on
the project cache directory (ADR-0008, FASE 1.1) layers on top
for catastrophic-write isolation between two `schema` processes
mid-reindex.

### Cache layout (replaces `lance/`)

```
~/.cache/schema/projects/<id>/
├── store.db          ← was lance/ (now a single file)
├── store.db-wal      ← WAL journal
├── store.db-shm      ← WAL shared memory
└── metadata.toml     ← unchanged (ADR-0007)
```

Reset becomes `rm store.db*`. Backup becomes `cp store.db*`.
Both feed the cleanup tool family planned in the follow-up PR.

### Migration from existing LanceDB caches

Open question — see **Follow-ups**. FASE 1.0 has no deployed
consumers outside the developer's own machines, so manual
cleanup before upgrading is acceptable for v0.2.

## Consequences

- **Good:** Removes `lancedb`, `arrow-array`, `arrow-schema` from
  `Cargo.toml`; adds `rusqlite` + `sqlite-vec`. Net reduction in
  deps and compile time.
- **Good:** Hybrid search is a future `SELECT` away (`MATCH` ⊕
  `vec_search` joined by rowid).
- **Good:** Parquet export is `duckdb -c "ATTACH 'store.db';
  COPY chunks TO 'chunks.parquet';"` — no native Arrow needed
  in `schema` itself.
- **Good:** WAL mode covers the two-readers concurrency that
  the LanceDB MVCC pitch addressed.
- **Good:** Cleanup tools (planned as a follow-up PR) become
  trivial: `DELETE FROM chunks` for `reset_all`, `DELETE FROM
  chunks WHERE source_path = ?` for `forget_source`.
- **Bad:** `sqlite-vec` is at 0.x; pinned in `Cargo.toml`;
  bumps require code review (same posture as the LanceDB pin
  it replaces).
- **Bad:** Brute-force vector scan is O(N · D). Measured under
  50 ms at 50 K × 1024 dims; acceptable. If any consumer
  exceeds ~500 K chunks, `sqlite-vec` exposes an HNSW index that
  can be enabled in a follow-up ADR.
- **Neutral:** Disk footprint roughly equivalent — vectors
  dominate; SQLite metadata adds < 5 MB.

## Fitness function

- **Integration test (latency):** insert 1 000 fixture chunks,
  run `query_nearest(vec, 8)`; assert wall-clock latency under
  50 ms and that the top-1 hit matches the seeded golden.
- **Integration test (hybrid):** insert a chunk with title
  "RBAC" and content "role-based access control"; assert that
  both `MATCH 'rbac'` (FTS5) and `vec_search(<embedding of
  'rbac'>)` return that chunk's rowid. Validates the FTS5 + vec0
  triggers are firing.
- **Integration test (concurrency):** open two read-only handles
  + one writer; assert reads succeed during writes. Validates
  WAL mode is set.
- **CI build-time check:** the `cargo build --release` time
  delta after the swap is logged in the PR description; a
  regression that re-introduces LanceDB or Arrow would be
  visible in `Cargo.toml` diff and in the build log.

## More information

- `src/retrieval/store.rs` — implementation scope (rewrite).
- ADR-0006 — predecessor; superseded by this ADR.
- ADR-0007 — delta-sync layer (consumes the store; unchanged).
- ADR-0008 — cache isolation (path layout updated to `store.db`).
- `sqlite-vec`: <https://github.com/asg017/sqlite-vec>
- `rusqlite`: <https://docs.rs/rusqlite/>

## Follow-ups

- **LanceDB cache migration policy.** Open question: (1) manual,
  release-notes-only ("delete `~/.cache/schema/projects/*/lance/`
  before upgrading"); (2) automatic detect-and-purge in
  `run_serve` on first start. Default for v0.2: manual. Promote
  to (2) if any external consumer adopts before then.
- **Hybrid-search MCP tool.** A new tool combining FTS5 + vector
  scores warrants its own ADR in FASE 2.
- **HNSW activation threshold.** If a consumer hits ~500 K
  chunks, evaluate enabling `sqlite-vec` HNSW. New ADR.
- **Cleanup tools (PR2).** `schema reset` + `schema forget
  --path <p>` CLI subcommands sharing one library function with
  the corresponding MCP tools (deferred to the next PR per the
  refactor-first sequencing).

## Evidence and amendments

- _2026-04-25 — Initial recording. ADR-0011 proposed by
  Fabricio in the same session that accepted ADR-0006, after
  re-evaluating LanceDB against the actual upper bound of corpus
  size and confirming that the three roadmap drivers (hybrid
  search, Parquet export, MVCC) are equally served by SQLite +
  `sqlite-vec` + FTS5 at a fraction of the dependency cost._
- _2026-04-25 — Accepted and implemented. The persistence
  adapter swap was carried out under ADR-0013 (hexagonal
  architecture), which made the migration a one-file change:
  `src/adapters/lancedb_store.rs` deleted, `src/adapters/
  sqlite_vec_store.rs` created, plus `Cargo.toml` deps swap
  (`lancedb 0.27` + `arrow-array 57` + `arrow-schema 57`
  removed; `rusqlite 0.39` with `bundled` feature + `sqlite-vec
  0.1.9` added), one wire-up line in `src/main.rs`, and a
  field rename in `src/adapters/project_identity.rs`
  (`lance_dir` → `store_path`, `lance/` → `store.db`). No
  changes to `app/`, `domain/`, or `ports/` — the
  ADR-0013 fitness function held. **Cache migration:** manual
  per the original ADR's Follow-ups; first `schema serve` on
  the new binary logs `tracing::warn!` if `~/.cache/schema/
  projects/<id>/lance/` is detected and asks the operator to
  delete it; no auto-prune. **Validation:** `cargo fmt --check`,
  `cargo clippy --all-features --all-targets --workspace -- -D
  warnings`, and `cargo test --all-features` all exit 0; test
  count grew from 20 → 27 (7 new unit tests covering the new
  adapter: idempotent open, append-and-list, delete cascade
  through both vec0 and FTS5 triggers, find-by-artifact-id,
  find-mentioning self-ref exclusion, query_nearest distance
  ordering, vector-byte layout invariant)._
- _2026-04-25 — Fitness-function tests landed. The three
  contractual tests in `## Fitness function` above are now
  implemented as unit tests in
  `src/adapters/sqlite_vec_store.rs::tests`:
  `query_nearest_meets_latency_at_scale` (1 000 chunks, top-8,
  measured wall-clock 1.25 ms release / 2.37 ms debug on
  Apple M-series — well under the 50 ms ceiling),
  `fts5_and_vec0_fire_on_same_chunk` (asserts the `chunks_fts`
  trigger populates from a `chunks` insert, by opening a
  separate read-only `rusqlite::Connection` and running
  `MATCH 'rbac'`), and
  `wal_allows_concurrent_reads_during_writes` (two read-only
  connections + one writer task on `tokio::test(flavor =
  "multi_thread")`; reads succeed throughout, no
  `SQLITE_BUSY`, count monotonic non-decreasing). Test count
  went from 27 → 31._
- _2026-04-25 — **Surprising finding: `sqlite-vec` 0.1.9 has
  no safe loader.** The published crate exposes only the raw
  `extern "C" fn sqlite3_vec_init()` symbol; the
  `sqlite_vec::load(&conn)` API cited in the ADR's Decision
  Outcome (Option C code sketch) does not exist in 0.1.9.
  `rusqlite::auto_extension::register_auto_extension` and
  `Connection::load_extension` are both `pub unsafe fn`. The
  resolution required a single narrow `#[expect(unsafe_code,
  reason = "...")]` block in
  `src/adapters/sqlite_vec_store.rs::register_vec_extension_once`
  registering `sqlite3_vec_init` via `sqlite3_auto_extension`
  + `mem::transmute`; the `[lints.rust] unsafe_code` was
  downgraded from `forbid` to `deny` (one-line `Cargo.toml`
  amendment, recorded as an ADR-0012 Evidence amendment). No
  other `unsafe` exists in the crate. When `sqlite-vec` ships
  a safe `load` function (tracked in this ADR's Follow-ups),
  the `#[expect]` block self-expires (clippy will flag the
  unused attribute) and the `unsafe_code` lint can be promoted
  back to `forbid`._
