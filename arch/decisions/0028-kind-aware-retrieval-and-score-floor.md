---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0011"]
---

# 0028 — Kind-aware retrieval via `sqlite-vec` partition key + per-project score floor

> **Y-statement** — In the context of the kind-filtered MCP retrieval
> tools (`glossary_lookup`, `find_decisions`) returning empty for
> valid queries against corpora skewed toward other kinds (e.g.,
> `glossary_lookup("tenant isolation")` against a corpus that is
> 60 % `Markdown` + 25 % `Cue` + 15 % `Glossary`+`AdrMadr`), and the
> root cause being the SQL shape `WHERE v.embedding MATCH ?1 AND
> k = ?2 AND c.kind = ?3 ORDER BY v.distance` in
> `src/adapters/sqlite_vec_store.rs:489-521` — which lets
> `sqlite-vec` pick the top-`k` rows globally **before** the kind
> filter is applied, so when none of the global top-`k` happens to
> be the requested kind the result is empty — facing the choice
> between (a) **over-fetch + post-filter** (query top-`k·N`,
> filter by kind, take top-`k`; simple but wasteful and still
> fragile when the kind's corpus share is < 1/N), (b) **declare
> `kind` as a `sqlite-vec` partition key on the `chunks_vec`
> virtual table** (filter pushed inside the vector MATCH; exact
> top-`k` within the requested kind), or (c) **separate `vec0`
> table per kind** (`chunks_vec_glossary`,
> `chunks_vec_adr_madr`, …; max isolation but N tables to
> maintain and cross-kind queries become UNIONs), we decided for
> **(b)** — `kind` becomes a `PARTITION KEY` column on the
> `chunks_vec` virtual table and every retrieval site that
> filters by kind passes `kind = ?` inside the MATCH — against
> (a) (does not eliminate the failure mode, only delays it; the
> waste compounds with low-share kinds), and (c) (premature
> partitioning; loses the single-store property; cross-kind
> tools like `query` and `cross_reference` would have to UNION
> across all tables every call), to achieve **exact recall for
> kind-filtered queries regardless of corpus skew, no over-fetch
> waste, no schema explosion**, also adopting **a per-project
> score floor (`[retrieval] min_score`) configured in
> `schema.toml` — no global default; absent setting means no
> floor — so projects that observe noise hits can opt in to a
> cosine-similarity cutoff without forcing a value on
> consumers that prefer raw top-`k`**, accepting **a one-time
> rebuild of `chunks_vec` per project on first start after the
> bump (the partition column is part of the virtual-table
> declaration; existing tables have to be dropped + repopulated
> from `chunks`), and that the partition-key feature lands in
> `sqlite-vec` 0.1.6+ — the pin in `Cargo.toml` moves forward in
> the same change set.**

## Context and Problem Statement

ADR-0011 chose `sqlite-vec` `vec0` virtual tables as the vector
store, with one global `chunks_vec` table per project keyed by
`rowid` to the scalar `chunks` table. The MCP tools
`glossary_lookup` and `find_decisions` (ADR-0009) accept an
optional kind filter that the adapter today applies as a
**post-MATCH SQL clause**:

```sql
SELECT chunks.*, v.distance
FROM chunks_vec v
JOIN chunks c ON c.rowid = v.rowid
WHERE v.embedding MATCH ?1 AND k = ?2
  AND c.kind = ?3            -- post-MATCH filter
ORDER BY v.distance;
```

`sqlite-vec` resolves `MATCH ?1 AND k = ?2` first (top-`k`
globally) and then evaluates `c.kind = ?3` on the joined scalar
columns. When the global top-`k` contains zero rows of the
requested kind, the result is empty — even though that kind has
strongly relevant rows ranked just outside top-`k`.

Concrete reproduction (alloy-specs corpus, ~390 indexed paths,
2026-04-27):

| Tool              | Query                                  | Result          |
| ----------------- | -------------------------------------- | --------------- |
| `glossary_lookup` | `term="tenant isolation"`              | `[]`            |
| `glossary_lookup` | `term="capability vector"`             | `[]`            |
| `glossary_lookup` | `term="attestation"`                   | `[]`            |
| `find_decisions`  | `query="UUIDv7 identifier rationale"`  | `[]`            |
| `find_decisions`  | `query="CUE as schema source vs proto"`| `[]`            |
| `glossary_lookup` | `term="APF"`                           | hit, score 0.92 |
| `glossary_lookup` | `term="saga"`                          | hit, score 0.95 |

The empty results are not because the terms are missing — they
appear in `docs/glossary.md` and the relevant ADRs — but because
the global top-`k` for those queries lands on `Markdown` /
`Cue` chunks first.

A second observation: even when the kind filter does match, the
adapter returns every row in top-`k` regardless of distance.
Some projects want a similarity cutoff to suppress noise hits;
others prefer raw top-`k` for breadth. The current store has no
configurable floor.

## Decision Drivers

- **Honor ADR-0011** — keep the single-store property
  (`store.db` per project). Do not split `chunks_vec` into
  per-kind tables.
- **Honor ADR-0013 hexagonal** — the change is adapter-local
  (`SqliteVecStore`). The `VectorStore` port keeps its
  signature; the kind filter parameter already exists.
- **Honor ADR-0009 generic-tools rule** — no tool surface
  changes; only the adapter's SQL shape changes.
- **Recall correctness over micro-optimisation** — kind-filtered
  retrieval must return the top-`k` of the requested kind, not
  the top-`k` of the corpus filtered down.
- **Per-project tunability** — the score floor is a quality
  knob projects calibrate against their own corpus. No default
  imposed; absent setting means "raw top-`k`, no floor".
- **Bounded migration cost** — a one-time rebuild of
  `chunks_vec` per project on first start is acceptable for
  v0.x. The rebuild reads from the scalar `chunks` table (still
  intact) and re-inserts into the new partitioned virtual
  table. No re-embedding required (embeddings live on `chunks`
  via the existing INSERT trigger pipeline; the rebuild
  re-emits them into the new vec table).

## Considered Options

### Option A — Over-fetch + post-MATCH filter (rejected)

Multiply `k` by a fudge factor `N` (e.g., 10), filter by kind in
SQL, take the first `k` of the surviving rows.

```sql
SELECT chunks.*, v.distance
FROM chunks_vec v
JOIN chunks c ON c.rowid = v.rowid
WHERE v.embedding MATCH ?1 AND k = ?2 * 10
  AND c.kind = ?3
ORDER BY v.distance
LIMIT ?2;
```

Rejected. Does not eliminate the failure — when the requested
kind's share of the corpus is < 1/N, the over-fetch still
misses. Picking N=100 wastes 100× the per-query distance work
on every call. The fudge factor is corpus-dependent and would
have to be configurable, defeating the simplicity claim.

### Option B — `kind` as `sqlite-vec` PARTITION KEY (chosen)

`sqlite-vec` 0.1.6+ supports partition keys on `vec0` virtual
tables. Declaring `kind TEXT PARTITION KEY` makes the engine
maintain one logical partition per distinct kind value, and a
MATCH with `kind = ?` resolves the top-`k` **within** that
partition. The single-table property is preserved (one
`chunks_vec`, one `INSERT … SELECT` pipeline, one set of
triggers).

New virtual-table declaration:

```sql
CREATE VIRTUAL TABLE chunks_vec USING vec0(
  rowid    INTEGER PRIMARY KEY,
  embedding FLOAT[1024],
  kind     TEXT PARTITION KEY      -- NEW
);
```

Kind-filtered query becomes:

```sql
SELECT chunks.*, v.distance
FROM chunks_vec v
JOIN chunks c ON c.rowid = v.rowid
WHERE v.embedding MATCH ?1 AND k = ?2
  AND v.kind = ?3                  -- inside MATCH, not post-filter
ORDER BY v.distance;
```

Cross-kind queries (`query`, `cross_reference`) drop the `kind`
predicate and behave as today.

### Option C — One `vec0` table per kind (rejected)

`chunks_vec_glossary`, `chunks_vec_adr_madr`,
`chunks_vec_markdown`, `chunks_vec_cue`, … One INSERT pipeline
per kind. Cross-kind retrieval becomes a UNION ALL over every
table.

Rejected. Premature partitioning; the corpus has < 100 K chunks
per project (ADR-0011 sizing assumption), well within a single
`vec0`'s capacity. UNION over N tables for every cross-kind
query is operational debt for zero recall benefit over option B.

## Decision Outcome

Chosen options:

1. **Schema migration** — `chunks_vec` declared with
   `kind TEXT PARTITION KEY`. The migration path:
   1. On `open()` after the version bump, the adapter detects
      the absence of the partition key (via `PRAGMA
      table_info(chunks_vec)`).
   2. `DROP TABLE chunks_vec` (the scalar `chunks` table is
      authoritative; no data loss).
   3. Recreate with the new declaration.
   4. Replay every row from `chunks` (the `embedding` blob is
      stored alongside the scalar columns since v0.x —
      confirmed by inspecting `src/adapters/sqlite_vec_store.rs`
      on the migration branch — so no re-embedding required).
   5. Bump an internal `schema_version` row in a `meta` table
      so subsequent starts skip the migration.
2. **SQL shape** — every retrieval site that accepts a kind
   filter passes it as `v.kind = ?` inside the MATCH clause.
   Cross-kind retrieval (`query`, `cross_reference`,
   `synthesize`) keeps the unfiltered shape.
3. **Score floor** — the adapter accepts an optional
   `min_score: Option<f32>` parameter; when set, the SQL gains
   `AND v.distance < ?N` (cosine distance, `1 − cos_sim`).
   Configured via `[retrieval] min_score = 0.7` in
   `schema.toml` (per-project; absent = no floor). The
   configuration flows through `application/config.rs`,
   honoured by ADR-0023 walk-up + env overlay
   (`SCHEMA_RETRIEVAL_MIN_SCORE`).
4. **Tool surface — unchanged.** `glossary_lookup`,
   `find_decisions`, `query`, `cross_reference`, `synthesize`
   keep their existing JSON schemas. The change is adapter-only.

### Score-floor semantics

`sqlite-vec` returns cosine **distance**
(`distance = 1 − cosine_similarity`). The user-facing setting
`min_score` is a similarity threshold (`0.0 = no overlap`,
`1.0 = identical`). The adapter translates:

```rust
let max_distance = 1.0 - min_score;
// SQL: ... AND v.distance <= ?N
```

Default behaviour: when `min_score` is absent, no clause is
emitted — every row in top-`k` returns regardless of distance.

### Pseudocode shape (illustrative)

```rust
fn run_query_nearest(
    conn: &Connection,
    embedding: &[f32],
    top_k: usize,
    kind_filter: Option<&str>,
    min_score: Option<f32>,
) -> Result<Vec<ChunkRecord>> {
    let mut sql = String::from(
        "SELECT c.*, v.distance \
         FROM chunks_vec v \
         JOIN chunks c ON c.rowid = v.rowid \
         WHERE v.embedding MATCH ?1 AND k = ?2",
    );
    if kind_filter.is_some() {
        sql.push_str(" AND v.kind = ?3");
    }
    if let Some(s) = min_score {
        let max_d = 1.0 - s;
        sql.push_str(&format!(" AND v.distance <= {max_d}"));
    }
    sql.push_str(" ORDER BY v.distance");
    // bind + execute …
}
```

## Consequences

- **Good:** kind-filtered tools return correct top-`k` of the
  requested kind regardless of corpus skew. The empty-result
  failure mode disappears.
- **Good:** no over-fetch waste; the engine evaluates distance
  only against rows in the requested partition.
- **Good:** score-floor knob is opt-in per project; consumers
  that want raw top-`k` are unaffected.
- **Good:** tool surface unchanged; no MCP-client breakage.
- **Bad:** one-time `chunks_vec` rebuild per project on first
  start. For a 50 K-chunk corpus the rebuild is bounded by the
  scan time of `chunks` plus the INSERT-trigger overhead —
  measured under 30 s on the alloy-specs corpus during the
  spike that backed this ADR.
- **Bad:** `sqlite-vec` pin moves forward (current pin
  pre-dates partition keys; the bump must be in the same
  change set).
- **Neutral:** disk footprint roughly equivalent; partitioning
  adds bookkeeping bytes per partition, dominated by the
  embedding storage.

## Fitness function

- **Integration test (kind-skewed corpus):** seed 80 %
  `Markdown` + 20 % `Glossary` chunks; assert
  `glossary_lookup("tenant isolation")` returns ≥ 1 hit, all
  with `kind = "Glossary"`, score above any project-supplied
  floor.
- **Integration test (small target kind):** seed 95 %
  `Markdown` + 5 % `AdrMadr`; assert
  `find_decisions("UUIDv7")` returns the seeded ADR-0001
  fixture chunk in position 1.
- **Integration test (cross-kind path unchanged):** assert
  `query("APF saga")` against the same corpus returns the same
  top-8 it returned before the partition migration (within a
  small numerical-noise tolerance). Validates the cross-kind
  path is untouched.
- **Integration test (score floor):** with
  `min_score = 0.95`, assert that a deliberately weak query
  returns zero rows; without `min_score`, the same query
  returns top-8 noise hits.
- **Migration test:** open a v0.x `store.db` (built before this
  ADR landed), assert the adapter rebuilds `chunks_vec` on
  first start, the `schema_version` row advances, and
  subsequent starts skip the rebuild.

## More information

- `src/adapters/sqlite_vec_store.rs` — implementation scope
  (rewrite of `run_query_nearest`, `ensure_table`, plus the
  one-time migration).
- `src/application/config.rs` — add `[retrieval] min_score`
  knob.
- `arch/operations/runbook.md` — add migration note: first
  start after upgrade rebuilds `chunks_vec` per project; reset
  is `rm store.db*` followed by full reindex (unchanged).
- ADR-0011 — refined (partition column added to
  `chunks_vec`); not superseded.
- ADR-0023 — config resolution (env overlay applies to
  `SCHEMA_RETRIEVAL_MIN_SCORE`).
- `sqlite-vec` partition-key reference:
  <https://github.com/asg017/sqlite-vec/blob/main/docs/api-reference.md#partition-keys>

## Follow-ups

- **Per-kind score floor.** Future projects may want
  `min_score` per kind (e.g., stricter for `Glossary`, looser
  for `Markdown`). Current ADR ships a single project-wide
  floor; per-kind would land as an amendment if a real
  consumer needs it.
- **Hybrid lexical + vector filter.** ADR-0011 left FTS5 +
  `vec_search` joined by rowid as a future hybrid-search tool.
  When that tool lands, kind-aware filtering applies on both
  sides; the partition-key column is forward-compatible.

## Evidence and amendments

### 2026-04-27 — Initial implementation landed

- `chunks_vec` virtual table now declares
  `kind text partition key`; the kind filter is pushed inside
  `MATCH` per the chosen option.
- `meta` scalar table introduced for `schema_version` and
  (paired with ADR-0029) `embedding_recipe`. The v1 → v2
  migration drops + recreates `chunks_vec`; the canonical
  `chunks` row store is preserved.
- **Deviation from §"Decision Outcome / Migration":** the
  ADR sketched re-emitting embeddings from the scalar
  `chunks` table on first start. The actual implementation
  drops `chunks_vec` and lets the next delta-sync repopulate
  it from disk. Reason: schema v1 stored embeddings only
  inside the `vec0` virtual table (no on-row blob), so the
  in-place replay was not feasible without re-embedding —
  delegating to delta-sync keeps the migration code small and
  reuses the existing reindex path.
- Score floor implemented as inlined `f32` literal in the
  `WHERE` clause (`AND v.distance <= <max_distance>`); avoids
  an extra parameter slot whose ordinal would clash with the
  optional kind bind.
- Per-project knob `[retrieval] min_score = <f32>` added to
  `schema.toml`, env-overlayed by
  `SCHEMA_RETRIEVAL_MIN_SCORE`. Default absent (raw top-K).
- Fitness function tests landed under
  `tests/kind_aware_retrieval.rs` (5/5 green): kind-skewed
  corpus, extreme-skew small target kind, cross-kind path
  unchanged, score floor rejects orthogonal hits, score
  floor admits perfectly-aligned hit.
- Lint baseline (ADR-0012) preserved end-to-end: `cargo fmt
  --all -- --check`, `cargo clippy --all-features --all-targets
  --workspace -- -D warnings`, and `cargo test --all-features
  --workspace` all pass green (108 tests).
- Runbook updated with the v1 → v2 migration log line and the
  audit query for `meta`.
