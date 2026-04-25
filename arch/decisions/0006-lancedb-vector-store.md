---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0006 — LanceDB embedded vector store

> **Y-statement** — In the context of persisting chunks + their bge-m3
> embeddings (ADR-0005, 1024-dim Float32) plus structured metadata
> (source_path, line_start/line_end, artifact_id, kind, content) so
> the MCP tools can run nearest-neighbour search and metadata
> filtering, facing the choice between (a) **LanceDB** (Rust-native,
> embedded, columnar/Apache Arrow under the hood, MVCC, no daemon),
> (b) **Qdrant** (production-grade but requires a sidecar daemon /
> container), (c) **sqlite-vss** (lightest, but features-poor and
> unmaintained as of audit), or (d) **FAISS** (Meta, top-tier ANN
> performance, but no built-in persistence — would require pairing
> with a separate KV store), we decided for **LanceDB 0.27 with a
> single `chunks` table per project**, stored at `~/.cache/schema/
> projects/<id>/lance/`, against Qdrant (overkill for an embedded
> daemon), sqlite-vss (limits FASE 2 features like hybrid search),
> or FAISS (persistence + scalar columns become bespoke), to
> achieve a Rust-native, embedded, MVCC-safe vector store with
> built-in scalar columns for metadata + filter pushdown via
> SQL-style `only_if` predicates, accepting that LanceDB is still
> at 0.x (so the API surface may shift between minor versions
> within FASE 1.0) and that the Apache Arrow type system imposes
> some boilerplate when constructing `RecordBatch`es for inserts.

## Context and Problem Statement

The store must:

1. Hold ~thousands of chunks per project (Lowcow-platform: ~150 ADR
   files, ~50 docs, ~30 schemas → ~3 K chunks today; lowcow-site
   could grow to ~50 K). Hundreds of K is the upper bound.
2. Run **nearest-neighbour** search against the 1024-dim vector
   column (top-K typically 8-32).
3. Run **scalar filters** for `kind = 'AdrMadr'`, `artifact_id =
   'ADR-0055'`, `content LIKE '%ADR-0055%'` — these are core to
   `find_decisions`, `glossary_lookup`, and `cross_reference`.
4. Persist across restarts; survive crashes (MVCC or comparable).
5. Run as an embedded library — no separate daemon, no port.

## Decision Drivers

- **Rust-native.** Avoids FFI binding work; matches ADR-0001's
  single-binary-deploy invariant.
- **Embedded.** No daemon means no port allocation, no auth, no
  separate process to manage.
- **Hybrid search.** Vector ANN + SQL-ish filters in the same query
  is exactly what tools 7-9 need.
- **MVCC.** Multiple `schema` instances on the same project (e.g.,
  Claude Code in two terminals) can read consistently while one
  re-indexes.
- **Apache Arrow lineage.** Future export to Parquet for backup,
  to DataFusion for SQL exploration, etc., is one downstream
  consumer away.

## Considered Options

### Option A — LanceDB (chosen)

```rust
let conn = lancedb::connect(&uri).execute().await?;
conn.create_table("chunks", reader).execute().await?;
let mut stream = table.vector_search(&v)?.limit(k).execute().await?;
```

Trade-offs: 0.x API; some boilerplate around Arrow `RecordBatch`
construction. Active development (saw 0.10 → 0.27 in ~12 months);
production users (Hugging Face, Anyscale).

### Option B — Qdrant (rejected)

Requires a daemon process or container. For an embedded MCP daemon
this is the wrong shape: now we have two processes to manage per
project. Qdrant is excellent for centralised teams; we are
edge-local.

### Option C — sqlite-vss (rejected)

Simplest, smallest. But unmaintained as of audit; features-poor for
hybrid search; closed off from the Arrow / Parquet lineage. Trades
features we will need in FASE 2.

### Option D — FAISS (rejected)

Top-tier ANN. No built-in persistence; metadata storage is bespoke;
no SQL-style filters. Pairing it with a KV store + custom predicate
layer reinvents what LanceDB provides out of the box.

## Decision Outcome

Chosen option: **A — LanceDB 0.27 embedded**.

### Schema

Single `chunks` table per project, schema fixed for FASE 1.0:

| column        | type                              | notes                    |
| ------------- | --------------------------------- | ------------------------ |
| `id`          | utf8                              | `<source>#L<a>-L<b>#<n>` |
| `source_path` | utf8                              | relative to project root |
| `line_start`  | int32                             | 1-indexed inclusive      |
| `line_end`    | int32                             | 1-indexed inclusive      |
| `artifact_id` | utf8 (nullable)                   | e.g. `ADR-0055`          |
| `title`       | utf8 (nullable)                   | section heading          |
| `kind`        | utf8                              | `CorpusKind` Debug fmt   |
| `content`     | utf8                              | chunk body               |
| `vector`      | fixed_size_list<float32, 1024>    | bge-m3 embedding         |

Modelled in `src/retrieval/store.rs::chunks_schema()`. Adding a
column is a breaking change — bumps an internal schema version
(FASE 1.1 wires the version field).

### Operations exposed

- `open` + `ensure_table` — idempotent setup.
- `append_chunks(chunks, vectors)` — batch insert.
- `delete_by_source(paths)` — prune on file change/removal.
- `query_nearest(vector, k, kind_filter)` — top-K vector search,
  optional scalar filter.
- `find_by_artifact_id(id, limit)` — exact scalar match.
- `find_mentioning(needle, limit)` — `LIKE '%needle%'` excluding
  artifact's own chunks.
- `list_source_paths()` — distinct sources, debug.

### Concurrency

LanceDB MVCC handles multiple readers + a writer. Combined with the
fd-lock advisory lock on the project's cache dir (FASE 1.1), two
`schema` instances on the same project never corrupt each other.

## Consequences

- **Good:** Rust-native, embedded, MVCC-safe, columnar storage.
- **Good:** scalar filters (`only_if`) handle every shape the
  current MCP tools need.
- **Good:** Apache Arrow lineage opens future Parquet export +
  DataFusion-style SQL exploration with zero adapter work.
- **Bad:** API at 0.x; minor releases may break callers. Pinned to
  `0.27` in `Cargo.toml`; bumps require a code review.
- **Bad:** Arrow `RecordBatch` construction is verbose. The
  `build_batch` helper in `src/retrieval/store.rs` localises the
  boilerplate.
- **Neutral:** disk footprint on a Lowcow-platform-sized corpus is
  ~50 MB (3 K chunks × 4 KB on average + 1024×4-byte vectors).
  Fits in the user's `~/.cache/` budget.

## Fitness function

- `chunks_schema()` is constructed once and used both for table
  creation and insert; mismatches produce immediate Arrow type
  errors at build time.
- Integration test (FASE 1.1) — append a chunk, query nearest,
  assert it returns. Validates the full pipeline with a fixed
  vector.
- The strict lint gate (`-D warnings`) catches uninitialised /
  shadowed access patterns at compile time.

## More information

- `src/retrieval/store.rs` — full schema + ops.
- `src/retrieval/sync.rs` — delta-sync orchestrator (consumes
  the store).
- ADR-0005 — embedding model (produces the 1024-dim vectors the
  store indexes).
- ADR-0007 — delta-sync protocol on top of this store.
- ADR-0008 — cache isolation (lance dir per project).
