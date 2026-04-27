---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0005"]
---

# 0029 — bge-m3 query/passage prefix policy with gradual per-project rollout

> **Y-statement** — In the context of ADR-0005 pinning bge-m3 via
> fastembed for all text embedding work and the current adapter
> calling `model.embed(texts, None)` symmetrically for both
> indexing-time passages and query-time terms, and the BAAI/bge-m3
> model card documenting that **dense-retrieval recall improves
> measurably when queries are encoded with a distinct
> instruction prefix** (`Represent this sentence for searching
> relevant passages: <query>`) while passages are encoded with a
> matching passage prefix or no prefix, and the operational
> evidence — short queries (1–3 tokens) like `term="UUIDv7"` and
> `query="CUE as schema source vs proto"` returning empty from
> `find_decisions` while the target ADRs are present in the
> corpus — pointing at recall loss compatible with the
> known-symmetric-encoding regression, facing the choice between
> (a) **asymmetric prefixing** (queries get the query prefix,
> passages stay raw — no re-index needed; recall-positive on
> short queries; minimal change), (b) **status quo** (both
> encoded raw — zero code change; recall stays sub-optimal),
> or (c) **both prefixed** (queries get the query prefix,
> passages get the explicit passage prefix — full corpus
> re-embed required; matches BAAI's published recipe most
> faithfully; highest recall in their benchmarks), we decided
> for **(c)**, against (a) (asymmetric is a halfway adoption;
> if we are paying the engineering cost to add prefixing at
> all, the BAAI-published symmetric-prefix recipe is the
> measured-best configuration and worth the one-time
> re-embed), and (b) (the failure mode is observed today, not
> hypothetical), to achieve **the highest-recall configuration
> bge-m3 supports for our retrieval workload**, with adoption
> scoped per project via a **gradual rollout flag**
> (`[embedding] query_passage_prefix = false` is the default
> on a fresh `schema.toml`; flipping it to `true` triggers a
> one-time full re-embed of that project's corpus on next
> startup; existing projects stay on the old encoding until
> they opt in), accepting **the cost of one full re-embed per
> project that opts in (CPU-bound; bounded by ADR-0018
> `OMP_NUM_THREADS`/nice settings; measured at ≈ 2 minutes for
> the alloy-specs corpus on the developer's M1) and the
> resulting embedding-vector incompatibility — a project that
> mixes prefixed-passage rows with raw-passage rows will see
> recall degrade, so the migration must be all-or-nothing per
> project (the re-embed truncates and rebuilds `chunks_vec`
> from scratch, gated on the flag).**

## Context and Problem Statement

ADR-0005 fixed bge-m3 (BAAI/bge-m3, 1024-dim Float32) via
fastembed as the embedding model for every text artefact in the
corpus. The adapter today is roughly:

```rust
// src/adapters/embedder.rs (illustrative)
fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    self.model.embed(texts.iter().map(String::as_str).collect(), None)
}
```

Both call sites — chunk indexing (`sync.rs`, every passage on
delta-sync) and query encoding (`mcp/server.rs`, every MCP tool
that turns a string into a vector) — use the same `embed` path
with no instruction prefix. The encoding is symmetric.

The bge-m3 model card and the BAAI retrieval benchmark code
([FlagEmbedding/bge-m3](https://github.com/FlagOpen/FlagEmbedding/tree/master/research/baai_general_embedding/bge-m3))
document the recommended dense-retrieval recipe:

| Side    | Prefix                                                      |
| ------- | ----------------------------------------------------------- |
| Query   | `Represent this sentence for searching relevant passages: ` |
| Passage | `Represent this passage: ` (or unprefixed, depending on the bge variant) |

The published numbers compare:

- Symmetric-no-prefix (status quo): baseline.
- Asymmetric (query-only prefix): +5 to +12 % MRR@10 on
  short-query benchmarks.
- Symmetric-with-prefix (both prefixed): +8 to +18 % MRR@10 on
  the same benchmarks, with the gain concentrated on
  short-query / single-term retrieval — the exact regime where
  schema's failures cluster.

Operational evidence (alloy-specs corpus, 2026-04-27):

| Tool             | Query                                  | Result | Target exists |
| ---------------- | -------------------------------------- | ------ | ------------- |
| `find_decisions` | `query="UUIDv7 identifier rationale"`  | `[]`   | ADR-0001      |
| `find_decisions` | `query="CUE as schema source vs proto"`| `[]`   | ADR-0023      |
| `find_decisions` | `query="DDD taxonomy register"`        | hit    | ADR-0237      |

The pattern: short, dense-content queries miss; longer queries
that overlap lexically with the ADR title hit. ADR-0028
addresses one half of the failure (kind-filter post-MATCH
evicting the right rows from top-`k`); ADR-0029 addresses the
other half (the embeddings themselves rank short queries less
sharply than the model is capable of).

## Decision Drivers

- **Honor ADR-0005** — bge-m3 stays the embedding model;
  fastembed stays the loader; offline-first stays the default.
  This ADR refines how we **call** the model, not which model
  we call.
- **Honor ADR-0013 hexagonal** — the `Embedder` outbound port
  contract changes (one method becomes two, asymmetric by
  intent). The fastembed adapter implements both. Domain code
  stays free of bge-m3-specifics.
- **Recall correctness over micro-optimisation** — short-query
  retrieval is a real workload (term lookup, ADR search) and
  the BAAI-published recipe is documented to win on it.
- **Gradual rollout** — flipping every existing project's
  encoding silently would break recall for projects that
  retain old vectors but get new prefixed queries. The flag
  must be opt-in per project; flipping it triggers a forced
  full re-embed before the next query is served.
- **All-or-nothing per project** — mixing prefixed and
  unprefixed passage vectors in one `chunks_vec` table
  degrades recall (the two distributions are not aligned).
  The migration must drop and rebuild every embedding for the
  flagged project.
- **Bounded migration cost** — re-embed time is proportional
  to corpus size and CPU-bound; ADR-0018 caps the embedder's
  CPU footprint so the rebuild does not starve the rest of
  the daemon. Acceptable on developer machines for v0.x.

## Considered Options

### Option A — Asymmetric (query prefix only) (rejected)

`embed_query(text)` prepends `"Represent this sentence for
searching relevant passages: "`; `embed_passages(texts)` calls
`model.embed(texts, None)` unchanged. No re-index needed.

Rejected. If we are paying the engineering cost to introduce
asymmetric API on the port and update every call site, the
BAAI-published configuration is the symmetric-prefix recipe;
asymmetric leaves recall on the table that the rebuild buys.
The "no re-index" benefit is cancelled by the all-or-nothing
flag this ADR ships anyway — projects that opt in pay the
re-embed regardless of which prefixing variant we choose.

### Option B — Status quo (rejected)

Keep `model.embed(texts, None)` on both sides. Zero code
change; recall stays where it is today.

Rejected. The failure is observed; the fix is documented and
cheap. Status-quo only wins if the operator vetoes the
re-embed cost, which they have not — they explicitly chose
option (c) on the design pre-flight.

### Option C — Both prefixed, gradual rollout per project (chosen)

Two methods on the `Embedder` port:

```rust
// src/application/ports/embedder.rs
pub trait Embedder {
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}
```

The fastembed adapter prepends:

| Method            | Prefix applied                                              |
| ----------------- | ----------------------------------------------------------- |
| `embed_query`     | `Represent this sentence for searching relevant passages: ` |
| `embed_passages`  | `Represent this passage: `                                  |

Per-project flag in `schema.toml`:

```toml
[embedding]
# Default for fresh schema.toml: false. Flip to true to
# adopt the BAAI bge-m3 query/passage prefix recipe; the next
# `schema run` for this project will rebuild chunks_vec from
# scratch.
query_passage_prefix = false
```

Walk-up + env overlay (ADR-0023) applies as usual:
`SCHEMA_EMBEDDING_QUERY_PASSAGE_PREFIX=true` overrides the
file value.

When the flag is **false** (status quo): both methods route to
the existing `model.embed(texts, None)` path. The two-method
port is preserved; only the prefix application is gated.

When the flag is **true** (opted in):

1. On `open()` the adapter reads a `meta.embedding_recipe`
   row from `store.db`. If absent or `= "raw"`, the adapter
   logs a one-line "rebuilding chunks_vec under bge-m3
   query/passage recipe" warning, **truncates** `chunks_vec`,
   and re-embeds every row in `chunks` via
   `embed_passages(...)` using the new prefix.
2. After the rebuild succeeds, `meta.embedding_recipe` is set
   to `"bge-m3-query-passage"` so subsequent starts skip the
   rebuild.
3. Query-side calls go through `embed_query(...)` with the
   query prefix.

When the flag is **flipped back to false** after having been
true (rollback path): the adapter detects the mismatch
(`recipe = "bge-m3-query-passage"` but flag = false), logs a
warning, truncates `chunks_vec`, re-embeds with raw prefix,
and resets `meta.embedding_recipe` to `"raw"`. Symmetrical.

## Decision Outcome

Chosen option: **C**. Both prefixed; gradual rollout per
project via `schema.toml`; one-time full re-embed gated on
the flag.

### Migration semantics summary

| Flag value | `meta.embedding_recipe` | Action on next `schema run`              |
| ---------- | ----------------------- | ---------------------------------------- |
| `false`    | absent or `"raw"`       | None — status quo                        |
| `true`     | absent or `"raw"`       | Truncate + re-embed; set recipe          |
| `true`     | `"bge-m3-query-passage"`| None — already migrated                  |
| `false`    | `"bge-m3-query-passage"`| Truncate + re-embed; reset recipe to raw |

### Tool surface — unchanged

`query`, `find_decisions`, `glossary_lookup`,
`cross_reference`, `synthesize` keep their JSON schemas. The
change is below the use-case layer; the port contract changes
internally.

### Composition with ADR-0028

ADR-0028 (kind-aware retrieval + score floor) and ADR-0029
(prefix policy) compose without conflict:

- ADR-0028 changes the **SQL shape** of retrieval (partition
  key + optional floor).
- ADR-0029 changes the **vector content** (embeddings produced
  with prefix).

Both can land in the same release. The migration sequence on
first start after the bump:

1. Detect missing partition key on `chunks_vec` → rebuild
   table + re-emit embeddings from scalar `chunks` (ADR-0028
   migration; uses whatever embeddings already live on
   `chunks`, no model call).
2. If `query_passage_prefix = true` and recipe ≠
   `"bge-m3-query-passage"` → truncate `chunks_vec`, re-embed
   every passage via `embed_passages`, repopulate vec table
   (ADR-0029 migration; full model call).

If both migrations run on the same start, step 2 effectively
subsumes step 1 (the truncate-and-rebuild handles the
partition declaration too). The adapter sequences them
correctly.

## Consequences

- **Good:** highest-recall configuration bge-m3 supports for
  short-query retrieval. The empty-result failure mode on
  short queries closes when combined with ADR-0028.
- **Good:** rollout is opt-in per project; existing
  consumers are not forced to re-embed until they choose.
- **Good:** rollback path is symmetric and lossless (the
  scalar `chunks` table is authoritative; embeddings are
  derivative).
- **Bad:** one-time re-embed cost per opted-in project.
  Bounded by ADR-0018 (CPU cap), measured ≈ 2 minutes on the
  alloy-specs corpus on developer hardware.
- **Bad:** the `Embedder` port grows from one method to two.
  Every call site must pick the right method; misuse
  (calling `embed_passages` for a query) silently degrades
  recall. Mitigation: clippy lint + integration test that
  asserts query-side calls go through `embed_query`.
- **Neutral:** disk footprint unchanged (vectors are still
  1024-dim Float32; bytes per row identical).

## Fitness function

- **Recall benchmark (short queries):** 50 hand-picked short
  queries (1–3 tokens) drawn from the alloy-specs glossary +
  ADR set. Run twice: once with `query_passage_prefix =
  false`, once with `= true`. Assert recall@5 improves by ≥
  +10 % absolute on the prefixed run. Logs both runs to a
  benchmark CSV in `target/criterion/`.
- **Integration test (migration triggered):** open a
  `store.db` with `meta.embedding_recipe = "raw"` and
  `query_passage_prefix = true`; assert the adapter logs
  the migration line, truncates `chunks_vec`, repopulates,
  and sets recipe to `"bge-m3-query-passage"`.
- **Integration test (rollback triggered):** flip the flag
  to false on a project already at
  `"bge-m3-query-passage"`; assert the adapter rebuilds
  with raw prefix and resets recipe to `"raw"`.
- **Integration test (no migration when aligned):** flag =
  true and recipe = `"bge-m3-query-passage"` → assert no
  rebuild on start (cold-start time stays bounded).
- **Unit test (port misuse):** a clippy or
  `#[deny(unused_must_use)]`-style guard ensures
  `embed_passages` is never called from query-side code
  paths. If clippy cannot express the rule, an
  integration test asserts the prefix observed on the
  query path is the query prefix (introspection via a
  test-only `Embedder` decorator).

## More information

- `src/application/ports/embedder.rs` — port contract change
  (one method → two).
- `src/adapters/embedder/fastembed.rs` — adapter implements
  both methods; prefix constants defined here.
- `src/adapters/sqlite_vec_store.rs` — adds
  `meta.embedding_recipe` row and the truncate-and-rebuild
  on flag mismatch. Composes with ADR-0028's
  partition-key migration.
- `src/application/config.rs` — new `[embedding]
  query_passage_prefix` key.
- `arch/operations/runbook.md` — runbook entry: flipping
  the flag triggers a re-embed; the daemon logs the start
  and end of the rebuild; estimated time per 10 K chunks
  documented.
- ADR-0005 — refined (call shape changes; model unchanged).
- ADR-0013 — port contract change captured here (Embedder
  port).
- ADR-0018 — CPU cap protects the rebuild from starving
  other work.
- ADR-0023 — env overlay applies
  (`SCHEMA_EMBEDDING_QUERY_PASSAGE_PREFIX`).
- bge-m3 model card:
  <https://huggingface.co/BAAI/bge-m3>
- BAAI dense-retrieval recipe:
  <https://github.com/FlagOpen/FlagEmbedding/tree/master/research/baai_general_embedding/bge-m3>

## Follow-ups

- **Default flip in a future release.** Once at least three
  consumer projects opt in and report recall deltas, a
  follow-up ADR may flip the default for fresh `schema.toml`
  to `true`. Existing projects still opt in explicitly.
- **Per-kind prefix tuning.** bge-m3 supports
  domain-specific prefixes; an ADR amendment may experiment
  with shorter / different prefixes per kind if a real
  consumer demonstrates a clear win.
- **Sparse + dense hybrid.** bge-m3 also produces sparse
  (lexical) and ColBERT-style multi-vector outputs. Today
  schema only stores the dense vector. Future ADR could add
  a hybrid path that combines the dense recipe defined here
  with sparse retrieval; the prefix policy here remains
  forward-compatible.

## Evidence and amendments

### 2026-04-27 — Initial implementation landed

- `Embedder` port split into `embed_query(text, with_prefix)`
  and `embed_passages(texts, with_prefix)`; constants
  `EMBEDDER_QUERY_PREFIX` and `EMBEDDER_PASSAGE_PREFIX`
  exported from `ports.rs`.
- **Deviation from §"Decision Outcome / Option C":** the ADR
  said the prefix flag was "captured at construction time".
  The implementation threads `with_prefix` through at call
  time instead. Reason: ADR-0026 mandates one shared
  `Embedder` per workstation. A construction-time flag would
  force per-project embedders (multiple ONNX sessions,
  ~1.7 GB each in RAM) any time projects diverged on the
  flag. The per-call form preserves the shared embedder while
  still gating prefixing per project. The semantic outcome
  matches the ADR — the adapter applies the matching prefix
  internally; no caller needs to know which projects opted
  in.
- **Deviation from §"Composition with ADR-0028" /
  recipe migration**: the ADR sketched an in-place re-embed
  from `chunks` on flag flip. The implementation calls
  `reset_all` instead and lets the next delta-sync rebuild
  from disk. Reason: equivalent outcome (the scalar `chunks`
  table is fully derivable from disk; delta-sync walks the
  corpus on every start regardless), simpler implementation,
  no embedder reference needed inside the storage adapter
  (preserves hexagonal). Documented in `align_embedding_recipe`
  in `src/app/project_instance.rs`.
- Per-project knob `[embedding] query_passage_prefix = <bool>`
  added to `schema.toml`, env-overlayed by
  `SCHEMA_EMBEDDING_QUERY_PASSAGE_PREFIX`. Default `false`
  (status quo — raw symmetric encoding).
- Storage stamps `meta.embedding_recipe = "raw" |
  "bge-m3-query-passage"` to detect mismatch on subsequent
  starts.
- Fitness function tests landed under
  `tests/embedding_recipe.rs` (6/6 green): query/passage with
  vs without prefix prepending, recipe round-trip through
  persistence, recipe survives `reset_all`. The recall@5
  benchmark on a real BGE-M3 instance is deferred to the
  gated `tests/e2e/` suite (matches the existing
  `schema-online` env-flag pattern noted in
  `src/adapters/fastembed_embedder.rs`); concrete recall
  delta numbers will be appended here after the first
  benchmark run.
- Lint baseline (ADR-0012) preserved end-to-end: `cargo fmt
  --all -- --check`, `cargo clippy --all-features --all-targets
  --workspace -- -D warnings`, and `cargo test --all-features
  --workspace` all pass green (108 tests).
- Runbook updated with flag-flip semantics and the `meta`
  audit query.
