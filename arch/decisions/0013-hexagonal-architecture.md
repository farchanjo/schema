---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0013 — Hexagonal architecture (ports & adapters)

> **Y-statement** — In the context of a Rust crate that has
> grown to ~1.5 K LOC across five technology-shaped modules
> (`config/`, `corpus/`, `embeddings/`, `mcp/`, `retrieval/`)
> and is about to swap its persistence implementation
> (LanceDB → SQLite + `sqlite-vec` per ADR-0011), facing the
> choice between (a) **keep the current technology-shaped
> layout** (modules named after the dependency they wrap;
> orchestration code (`DeltaSync`) directly couples to
> concrete types like `VectorStore` and `Embedder`), (b)
> **classic hexagonal (ports & adapters)** with full
> directory depth (`domain/{}/`, `ports/{}/`, `app/{}/`,
> `adapters/<group>/<impl>/`), or (c) **hexagonal-lite**
> (single-file `domain.rs` and `ports.rs`, flat
> `adapters/<impl>.rs`, services under `app/`), we decided
> for **(c) hexagonal-lite**, against (a) (orchestration
> coupled to concrete types blocks adapter swaps and unit
> tests with fakes; the impending sqlite-vec swap would
> touch every caller of `VectorStore`) and (b) (over-deep
> directory tree for a 1.5 K-LOC crate; ports and domain
> have at most a handful of items each — splitting them
> across files adds navigation cost without payoff), to
> achieve a clean separation between **what the application
> needs** (ports = traits) and **how each external
> dependency is wired** (adapters = trait impls), enabling
> the ADR-0011 persistence swap to touch exactly one file
> under `adapters/` and unblocking unit tests against fakes
> instead of real LanceDB / fastembed / filesystem,
> accepting that the restructure is a one-time mechanical
> shuffle of existing code (no behaviour change, only
> module layout + extracted trait definitions) and that
> Rust's `dyn Trait` overhead at the few port-call sites is
> negligible at our throughput.

## Context and Problem Statement

Today, `src/` is shaped by **technology** rather than
**responsibility**:

```text
src/
├── main.rs
├── lib.rs
├── config/        — TOML loader + ProjectIdentity
├── corpus/        — walker + chunker + watcher
├── embeddings/    — fastembed wrapper
├── mcp/           — rmcp tools
└── retrieval/     — LanceDB store + delta-sync orchestrator
```

Two observable consequences:

1. **Orchestration is coupled to concrete types.**
   `DeltaSync::run` takes `&VectorStore` and `&mut Embedder`
   (the LanceDB and fastembed concrete types directly).
   Swapping LanceDB for `sqlite-vec` (ADR-0011) requires
   changing every caller signature, even though the swap is
   a pure persistence concern. Same shape blocks unit tests
   that want a fake store.
2. **No clear boundary between "domain" and "I/O".**
   `Chunk`, `ChunkRecord`, `CorpusKind`, `FileMeta` are pure
   data types — they describe *what* the application
   manipulates, not *how* anything is read or written. Today
   they live mixed with their producer/consumer modules, so
   it is non-obvious where a new pure type should land.

These costs are small at 1.5 K LOC but compound. ADR-0011's
implementation is the forcing function: doing the swap
under a hex layout means writing one new file under
`adapters/` and updating one wire-up line in `main.rs`;
doing it under the current layout means changing every
caller of `VectorStore`.

## Decision Drivers

- **Adapter-swap cost = one file.** ADR-0011 must touch
  exactly one file under `adapters/persistence/` plus the
  bootstrap wire-up. Anything more means we got the
  boundary wrong.
- **Pure types live in the domain.** `Chunk`,
  `ChunkRecord`, `CorpusKind`, `FileMeta` belong in
  `domain/` (or `domain.rs` for hex-lite). They have no I/O
  and no `async` and no error type that mentions a
  dependency.
- **Port traits are tiny and explicit.** Each port lists
  exactly the methods the application calls — not the full
  surface of any underlying library. If the adapter has 20
  methods and the application uses 4, the port has 4.
- **No reverse dependencies.** `domain/` does not import
  from `app/` or `adapters/`. `app/` does not import from
  `adapters/`. `adapters/` may import from `domain/` and
  `ports/`. Driver adapters (`main.rs`, MCP server) wire
  concrete adapters into `app/` services that take
  `Arc<dyn Port>`.
- **Pragmatic depth.** A 1.5 K-LOC crate does not benefit
  from `domain/{chunk.rs,corpus.rs}` and
  `ports/{persistence.rs,embedder.rs,...}` directories.
  Single-file `domain.rs` and `ports.rs` are easier to
  navigate; split later only if either grows past ~300
  lines.

## Considered Options

### Option A — Status quo (rejected)

Technology-shaped modules; orchestration coupled to
concrete types. Pros: zero refactor work; existing
contributors know the layout. Cons: ADR-0011 swap touches
every caller; unit tests need real adapters or a custom
mock per call site; new pure types have no obvious home.

### Option B — Classic hexagonal (rejected)

```text
src/
├── domain/
│   ├── mod.rs
│   ├── chunk.rs
│   └── corpus.rs
├── ports/
│   ├── mod.rs
│   ├── persistence.rs
│   ├── embedder.rs
│   ├── walker.rs
│   ├── watcher.rs
│   └── chunker.rs
├── app/
│   ├── mod.rs
│   ├── delta_sync.rs
│   ├── query.rs
│   └── watcher_consumer.rs
└── adapters/
    ├── persistence/
    │   ├── mod.rs
    │   └── lancedb.rs
    ├── embedder/
    │   ├── mod.rs
    │   └── fastembed.rs
    └── ...
```

Pros: textbook clean. Cons: 6 directories with `mod.rs` plus
1-2 leaf files each, for a crate where each port is a single
trait with 4-7 methods. Navigation cost without payoff.

### Option C — Hexagonal-lite (chosen)

```text
src/
├── main.rs                — driver adapter (CLI)
├── lib.rs                 — module wiring
├── domain.rs              — pure types: Chunk, CorpusKind, ChunkRecord, FileMeta
├── ports.rs               — traits: Persistence, Embedder, Walker, Watcher, Chunker
├── app/
│   ├── mod.rs
│   ├── delta_sync.rs      — DeltaSync<P, E, W, C> service (or with dyn ports)
│   ├── query.rs           — query/find_decisions/glossary_lookup/cross_reference logic
│   └── watcher_consumer.rs
└── adapters/
    ├── mod.rs
    ├── lancedb_store.rs   — Persistence impl (LanceDB; replaced by sqlite_vec_store.rs in ADR-0011)
    ├── fastembed_embedder.rs   — Embedder impl
    ├── filesystem.rs      — Walker (walkdir) + Watcher (notify) — bundled by responsibility
    ├── markdown_chunker.rs — Chunker impl (pulldown-cmark)
    ├── toml_config.rs     — schema.toml loader
    ├── project_identity.rs — ProjectIdentity (cache path resolution)
    └── mcp_server.rs      — rmcp driving adapter
```

Pros: clear domain/ports/adapters separation; adapter swap = one file; navigation is shallow (top-level adapter file per technology). Cons: domain.rs and ports.rs grow over time and may need splitting later (acceptable; document the splitting threshold at ~300 lines).

## Decision Outcome

Chosen option: **C — Hexagonal-lite**.

### Module rules (enforce by review)

- `domain.rs` imports only `serde`/`thiserror`/std primitives.
  No `tokio`, no `lancedb`/`rusqlite`/`fastembed`/`notify`/
  `walkdir`/`pulldown_cmark`/`toml`. No `async`. No
  `Result<_, anyhow::Error>` (use a domain-level error type
  if needed).
- `ports.rs` imports `domain` + `async_trait` (or uses
  manual async fn-in-trait, depending on Rust 1.95
  ergonomics) + std. No external dependencies. Traits are
  shaped by what the application calls, not by the
  underlying library.
- `app/` modules import `domain` + `ports` + std + `tokio`
  primitives (channels, mutexes, futures). No direct
  dependency-crate imports. Take ports as `Arc<dyn Trait>`
  (or generic `T: Trait` if monomorphisation matters at a
  hot path).
- `adapters/` modules implement port traits and import
  whatever they need. May import from `domain`. May NOT
  import from `app/`.
- `main.rs` is the driver: it constructs concrete adapters
  (`LancedbStore`, `FastembedEmbedder`, etc.), wraps them
  in `Arc<dyn Trait>`, and hands them to `app/` services.

### Port shapes (final)

- **`Persistence`** (was `VectorStore`):
  `ensure_ready`, `append_chunks`, `delete_by_source`,
  `query_nearest`, `find_by_artifact_id`, `find_mentioning`,
  `list_source_paths`. All `async`. Error type
  `PersistenceError`.
- **`Embedder`**: `embed(&mut self, &[&str]) -> Vec<Vec<f32>>`.
  Synchronous (fastembed wraps onnxruntime); the application
  layer may call from a `spawn_blocking` if needed.
  Error type `EmbedError`.
- **`Walker`**: `discover() -> Vec<DiscoveredFile>`.
  Synchronous.
- **`Watcher`**: `subscribe() -> mpsc::Receiver<CorpusEvent>`
  + `WatcherKeepAlive` for lifetime control.
- **`Chunker`**: `chunk(rel, abs, kind) -> Vec<Chunk>`.

### Migration plan

1. Create `domain.rs` and move pure types from current
   modules. Update `pub use` in `lib.rs`.
2. Create `ports.rs` with the trait definitions (extracted
   from current concrete signatures).
3. Create `adapters/` and move existing implementation
   files. Each adapter implements its port.
4. Create `app/` and move `DeltaSync`, query orchestration,
   and watcher consumer. Make them generic over ports
   (or take `Arc<dyn Port>`).
5. Update `main.rs` to wire concrete adapters into `app/`
   services.
6. Run the canonical lint gate + tests after each step.

The migration **must not change behaviour**. Test count
stays at 20. Lint gate stays at exit 0.

## Consequences

- **Good:** ADR-0011 swap becomes a single-file drop-in.
  `adapters/lancedb_store.rs` is replaced by
  `adapters/sqlite_vec_store.rs`; no caller knows the
  difference.
- **Good:** Unit tests against fake ports (in-memory stub
  `Persistence`, deterministic `Embedder`) become trivial,
  enabling fast tests of `DeltaSync` and query logic
  without spinning up real LanceDB or downloading
  fastembed weights.
- **Good:** New pure types have an obvious home
  (`domain.rs`).
- **Good:** Reading the codebase top-down — `main.rs`
  shows what's wired; `app/` shows what the application
  does; `adapters/` shows the concrete tech stack.
- **Bad:** One-time refactor cost: file moves, trait
  extraction, generic/`dyn` parameterisation. Estimated
  one focused session.
- **Bad:** `Arc<dyn Trait>` adds one virtual dispatch per
  port call. Negligible at our throughput (kHz-scale at
  most); hot loops (the embedding loop, the brute-force
  vector scan) are inside adapters and not affected.
- **Neutral:** `domain.rs` and `ports.rs` may need
  splitting if the crate grows. Threshold: ~300 lines
  each. When it happens, split is mechanical (one file →
  a directory).

## Fitness function

- **Adapter-swap test:** ADR-0011 implementation must
  touch exactly **one** file under `adapters/` (replacing
  `lancedb_store.rs` with `sqlite_vec_store.rs`) plus the
  one wire-up line in `main.rs`. If it touches `app/` or
  `domain/` or `ports/`, the boundary leaked and the ADR
  has been violated.
- **Domain purity test (informal, by review):**
  `grep -rE 'lancedb|rusqlite|fastembed|notify|walkdir|tokio'
  src/domain.rs src/ports.rs` returns nothing. (If we add
  CI for this later, it is a one-line grep step.)
- **`app/` decoupling test (informal, by review):**
  `grep -rE 'lancedb|rusqlite|fastembed|notify|walkdir|
  pulldown_cmark|toml::' src/app/` returns nothing.
- **Unit-testability:** Adding a unit test for `DeltaSync`
  with an in-memory fake `Persistence` requires implementing
  the `Persistence` trait (4-7 methods) and nothing else.
  No async runtime trickery; no real disk; no real model.

## More information

- ADR-0011 — persistence adapter swap; this ADR enables
  the clean shape.
- ADR-0012 — strict lint baseline; the new structure is
  born under it.
- `src/lib.rs` — top-level module wiring (post-restructure).
- `src/main.rs` — driver wiring (post-restructure).

## Follow-ups

- **Domain error type.** Decide whether `domain.rs`
  defines its own `DomainError` enum or stays "data only"
  with no error type. If services in `app/` need to map
  port errors to a domain-level error for MCP responses,
  add it then. Defer until the need is concrete.
- **`async-trait` vs native async fn-in-trait.** Rust
  1.95 supports native async fn-in-trait for `dyn`
  contexts (with `Send` bounds). Try native first; fall
  back to `async-trait` only if `dyn` boxing becomes
  awkward.
- **Generic vs `dyn`.** Decide per service whether `app/`
  takes generic ports (`<P: Persistence>`) for
  monomorphisation or `dyn` for object safety. Default to
  `dyn` for simplicity; promote to generic only if a
  specific call site is hot.
- **`domain.rs` / `ports.rs` split threshold.** When
  either crosses ~300 lines, split into a directory.
  Document the actual sizes after the migration to gauge
  how soon this will fire.

## Evidence and amendments

- _2026-04-25 — Initial recording. ADR-0013 proposed
  alongside ADR-0011 (persistence swap) and ADR-0012
  (strict lint baseline). The migration occurs after
  ADR-0012 is live (so the new structure is born under
  the strict gate) and before ADR-0011's implementation
  (so the swap touches one adapter file)._
- _2026-04-25 — Restructure landed and ADR-0011 swap
  carried out one file later (`adapters/lancedb_store.rs`
  → `adapters/sqlite_vec_store.rs`). Adapter-swap fitness
  function held: only `Cargo.toml`, `adapters/`,
  `main.rs`, and `adapters/project_identity.rs` were
  touched by the swap; `app/`, `domain/`, `ports/` were
  not modified. Domain-purity and app-decoupling greps
  return clean (no adapter-specific imports in
  `domain.rs` or under `app/`)._
- _2026-04-25 — Unit-testability fitness function proved
  by `delta_sync_orchestrates_through_fake_ports` in
  `src/app/delta_sync.rs::tests`. The test wires
  `DeltaSync` against `FakePersistence`, `FakeEmbedder`,
  `FakeWalker`, `FakeChunker`, and `FakeMetadataStore`
  (each implementing the corresponding port in ~10-30 lines)
  and runs a full delta-sync pass. Test count went from 27
  → 31 with this addition._
- _2026-04-25 — **Known leak flagged for follow-up.**
  `app/delta_sync.rs` imports `file_content_hash` and
  `file_mtime` from `adapters/metadata_store.rs` to compute
  per-file hashes/mtimes during the diff phase. This is a
  small downward-pointing import (`app/` → `adapters/`)
  that violates the strict reading of this ADR's "module
  boundary rules" (`app/` modules import only `domain`,
  `ports`, `std`, `tokio`). The chosen test path (Test 4
  above) creates real temporary files via `TempDir` so the
  helpers work end-to-end; a fully disk-free fake-port
  test would require introducing a `FileMetaProbe` port
  for hash/mtime probing. **Tracked as TODO** at
  `src/app/delta_sync.rs` (in-source `// TODO(ADR-0013-
  followup): move file_content_hash/file_mtime behind a
  `FileMetaProbe` port to fully decouple from adapters/.`).
  Not blocking — the leak is one helper module, scope is
  limited, and the fitness function still holds for the
  ADR-0011 swap which was the primary goal._
