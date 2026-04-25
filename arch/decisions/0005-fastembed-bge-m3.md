---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0005 — bge-m3 via fastembed for text embeddings

> **Y-statement** — In the context of needing **text embeddings** for
> RAG (chunks → vectors that LanceDB indexes for nearest-neighbour
> search) on a workload that mixes **English** (most code, most
> ADRs) with **Brazilian Portuguese** (Lowcow business glossary,
> per the consumer-project mix), facing the choice between (a)
> **`BGE-M3`** (multilingual SOTA on MTEB, 1024 dims, ~2 GB ONNX
> weights), (b) **`multilingual-e5-large-instruct`** (lighter, ~560
> MB, slightly weaker on MTEB), (c) an **English-only model** like
> `bge-large-en-v1.5` (smallest, but loses pt-BR), or (d) a **cloud
> API** (OpenAI / Voyage) for embeddings, we decided for **bge-m3 via
> the `fastembed` crate** (which wraps ONNX Runtime + ships bge-m3 as
> a built-in model identifier with download-on-first-use), against
> e5-large (we want best-of-best per the consumer brief), English-only
> (would force a per-locale embedder fork), or cloud (no offline
> guarantee, recurring cost, latency adds 50-200 ms per call), to
> achieve native multilingual quality at SOTA-MTEB level with the
> entire pipeline running on the operator's M-series Mac, accepting
> ~2 GB on-disk weight file (downloaded once to `~/.cache/schema/
> models/`, shared across consumer projects), ~2 GB RAM resident
> when the embedder is loaded, and the `&mut self` ergonomic that
> fastembed enforces (handled by `Mutex<Embedder>` in the MCP
> server state).

## Context and Problem Statement

RAG quality is bounded by embedding quality. Two questions to
answer:

1. **Which model?**
2. **Which Rust runtime?** (in-tree implementation vs ONNX wrapper
   vs cloud API).

The consumer mix today is `lowcow-platform` (mostly English ADRs;
some pt-BR glossary terms) and future Lowcow-derived repos. Future
non-Lowcow consumers might be entirely English. Future Lowcow code
will be mixed.

## Decision Drivers

- **Multilingual capability.** Single model that handles EN + pt-BR
  well avoids per-locale forks.
- **MTEB quality.** Public benchmark. Rule of thumb: if the
  embedder is ≥ 5 ranks behind SOTA for our locale mix, retrieval
  precision tanks.
- **On-device.** Operator wants offline guarantee and zero
  recurring cost.
- **Ergonomics in Rust.** Loading + running ONNX from Rust is real
  work; `fastembed` does it well.

## Considered Options

### Option A — bge-m3 via fastembed (chosen)

`fastembed = "5.13"` ships bge-m3 as `EmbeddingModel::BGEM3`. Internally
it uses `ort` (ONNX Runtime) + `tokenizers` (HuggingFace's Rust
tokenizer). Model downloaded on first construction to a configurable
cache dir.

Specifications:

- 1024-dim dense vectors (matches `BGE_M3_DIMENSIONS` exported in
  `src/embeddings/bge_m3.rs`).
- ~2 GB ONNX file on disk (mmap'd, shared via OS page cache).
- ~2 GB RAM when loaded; ~50 ms per single-string embed on M-series.
- Multilingual: top-of-MTEB on multilingual benchmarks (Apr 2026).

Trade-offs: large download on first use; embedder must be `&mut self`
to embed (per fastembed's API), forcing a `Mutex` in the server state.

### Option B — multilingual-e5-large-instruct (rejected)

~560 MB; slightly behind bge-m3 on MTEB. We chose best-of-best per
the operator's brief; switching to e5 trades quality for ~1.5 GB of
disk we did not need to save.

### Option C — English-only model (rejected)

`bge-large-en-v1.5` (~340 MB, EN only). Would force a per-locale
fork once any pt-BR content surfaces. Premature optimisation against
a real future need.

### Option D — Cloud embedding API (rejected)

OpenAI `text-embedding-3-large` or Voyage AI; +50-200 ms latency per
embed; recurring cost; no offline path. Operator's hardware is high-
end (M-series); local is the right choice.

### Option E — `candle-transformers` (deferred)

HuggingFace's Rust ML framework. Newer than `ort`. Some models have
candle implementations; bge-m3 may or may not be wired today. We
chose `fastembed`'s production-grade `ort` path; candle is a future
swap if it offers wins.

## Decision Outcome

Chosen option: **A — bge-m3 via fastembed**.

### Initialisation

```rust
let opts = InitOptions::new(EmbeddingModel::BGEM3)
    .with_cache_dir(cache_dir)              // ~/.cache/schema/models/
    .with_show_download_progress(false);
let model = TextEmbedding::try_new(opts)?;  // downloads on first call
```

### Cache dir

Shared across consumer projects: `~/.cache/schema/models/`. The
project-specific cache (LanceDB store, metadata.toml) lives at
`~/.cache/schema/projects/<id>/` per ADR-0008; the model is *not*
per-project because the same weights serve every project.

### Concurrency

`fastembed::TextEmbedding::embed(&mut self, ...)` requires mutable
access. The MCP server state holds a `Mutex<Embedder>` so concurrent
tool invocations serialise on the embedder. For a per-session
subprocess this is fine; if request volume grows we can shard
embedders behind a pool (FASE 1.1).

## Consequences

- **Good:** SOTA multilingual quality offline.
- **Good:** one model → one cache dir → one download per machine,
  amortised across projects.
- **Good:** `fastembed`'s API is ergonomic; ~50 lines of wrapper.
- **Bad:** ~2 GB download on first use. Subsequent runs are
  instant.
- **Bad:** `Mutex<Embedder>` serialises concurrent embeds. Acceptable
  for the per-session subprocess shape; may need pooling later.
- **Neutral:** model swap requires this ADR amendment + a one-time
  re-embedding of every project (the cached vectors do not transfer
  across models). LanceDB cache isolation per project means no
  cross-contamination.

## Fitness function

- `BGE_M3_DIMENSIONS = 1024` is exported from `src/embeddings/
  bge_m3.rs` and consumed by the LanceDB schema in `src/retrieval/
  store.rs::chunks_schema()`. Mismatch = compile error.
- Live embedder construction is exercised on every `serve` startup;
  failure modes (network down on first run, malformed weights)
  surface immediately with a clear error.
- Integration test (FASE 1.1, gated on `SCHEMA_ONLINE=1`) embeds
  a known string and asserts the vector length matches
  `BGE_M3_DIMENSIONS`.

## More information

- `src/embeddings/bge_m3.rs` — wrapper.
- `Cargo.toml` — `fastembed = "5.13"`.
- ADR-0006 — LanceDB schema consumes the 1024-dim vectors.
- ADR-0008 — cache layout (`~/.cache/schema/models/` global +
  `~/.cache/schema/projects/<id>/` per-project).
