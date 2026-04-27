---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0001", "ADR-0013"]
---

# 0034 — Migrate to a Cargo workspace: `schema-core` + `schema` + `recall`

> **Y-statement** — In the context of ADR-0001 setting up a single
> Cargo crate `schema` for the MCP retrieval tool, and ADR-0033
> introducing a second bounded context (`recall` — session
> transcript retrieval) that shares meaningful infrastructure with
> the existing `schema` (the `Embedder` port + fastembed loader,
> the markdown chunker, the line-window chunker, generic HNSW
> wiring), facing the choice between (a) **status quo single
> crate** with both binaries (`schema` and `recall`) co-living in
> the same `src/`, sharing modules implicitly via the same crate
> root (mixes two bounded contexts in one compilation unit; ADR-
> 0013 hexagonal boundaries blur), (b) **two crates side by side**
> in this repo without any workspace structure (no shared
> dependency resolution; `recall` re-declares fastembed, rmcp,
> tokio, etc.; drift between Cargo.lock entries is inevitable),
> (c) **Cargo workspace with shared kernel** — root `Cargo.toml`
> becomes a workspace declaring three members
> (`crates/schema-core/` library, `crates/schema/` binary,
> `crates/recall/` binary), one Cargo.lock, one toolchain pin,
> shared `[lints.*]` policies inherited from the workspace,
> incremental migration in three small commits, or (d) **two
> separate repositories** (`mcp-schema` keeps schema; new repo
> `mcp-recall` ships recall; no shared code), we decided for
> **(c)** Cargo workspace with `schema-core` shared kernel,
> against (a) (mixes vocabularies in one compilation unit; the
> `tool_router` macro from `rmcp` would have to register both
> binaries' tools simultaneously or the binaries would have to
> live as `[[bin]]` entries with separate `main`s but the same
> module tree — both options are uglier than splitting), (b)
> (re-declared deps drift; lint policies diverge; the workspace
> is exactly the Cargo idiom for this case), and (d) (two repos
> means two release cycles, two CI configs, two CLAUDE.md files;
> for two binaries from the same hand we accept the extra
> directory level over the duplicated repo overhead), to achieve
> **a single repository hosting two bounded contexts (schema and
> recall) that share infrastructure via `schema-core` without
> sharing vocabulary, with one Cargo.lock and one toolchain
> pin**, accepting **(1) a one-time migration that moves
> `src/` → `crates/schema/src/` then extracts shared modules into
> `crates/schema-core/`, executed in three small commits to keep
> the validation gate green at every step (incremental shape per
> decision 18), (2) `arch/` stays at the workspace root —
> ADRs and runbook cover both binaries; cross-references stay
> readable, (3) the single `lint.yml` workflow is updated to run
> `cargo clippy --workspace --all-targets --all-features --
> -D warnings` so ADR-0012's strict gate covers both binaries,
> (4) the install procedure (ADR-0014) stays at
> `/usr/local/bin/schema`; a parallel symlink for
> `/usr/local/bin/recall` is added at workspace install time,
> (5) the existing `cargo install --path .` command is replaced
> by `cargo install --path crates/schema` for the schema
> binary and `cargo install --path crates/recall` for the recall
> binary; the runbook step that currently says `cargo build
> --release` becomes `cargo build --release --workspace`.**

## Context and Problem Statement

The current repo layout is a single Cargo crate:

```
mcp-schema/
├── Cargo.toml           # crate name = "schema"
├── Cargo.lock
├── rust-toolchain.toml  # 1.95.0 pinned
├── clippy.toml
├── src/
│   ├── main.rs           bin entry
│   ├── lib.rs            re-exports
│   ├── adapters/         driving + driven adapters
│   ├── app/              use cases
│   ├── domain.rs         entities/VOs
│   ├── ports.rs          ports/traits
│   └── cli/              CLI subcommands
├── tests/                integration tests
├── arch/decisions/       ADRs
├── arch/operations/      runbook + lint playbook
├── examples/             sample schema.toml
└── .github/workflows/lint.yml
```

ADR-0033 introduces a second binary (`recall`) that wants to
re-use:

- **Embedder + fastembed loader** (today: `src/adapters/
  fastembed_embedder.rs`)
- **Markdown chunker** (today: `src/adapters/markdown_chunker.rs`)
- **HNSW wiring infrastructure** — the boilerplate around
  `instant-distance` / `hnsw_rs` is small but non-trivial, and
  if both binaries write it twice they will drift.
- **Some part of `domain.rs`** — `CorpusKind`, `Chunk`,
  `Embedding` are corpus-flavoured and **don't** belong to
  recall, but `Embedding` (a Vec<f32> wrapper) is generic and
  worth sharing.
- **`ports.rs`** — `Embedder` trait is generic; `Persistence`
  is corpus-only.

Mixing both binaries inside a single `src/` would put two
bounded contexts in one compilation unit, blurring ADR-0013's
hex boundaries. Two parallel crates without a workspace would
duplicate dependency declarations and risk Cargo.lock drift.

A Cargo workspace is the explicit Cargo idiom for "multiple
crates that share dependencies and config". It is the lowest-
overhead path that keeps both bounded contexts isolated while
sharing the kernel.

## Decision drivers

- **Hexagonal isolation across bounded contexts.** ADR-0013's
  domain-purity rule applies inside each crate. The workspace
  guarantees `crates/recall/` cannot accidentally import from
  `crates/schema/` (Cargo only allows direct use of explicitly
  declared dependencies).
- **One Cargo.lock for the whole project.** No drift between
  recall's `tokio` version and schema's. Updates land once.
- **Strict lint policy applied uniformly.** ADR-0012's
  `[lints.clippy]` and `[lints.rust]` blocks live in the
  workspace root and are inherited by every member crate via
  `lints.workspace = true`.
- **One toolchain pin.** `rust-toolchain.toml` stays at
  workspace root; both binaries build under Rust 1.95.0
  (Edition 2024).
- **Incremental migration safety.** Three small commits
  (decision 18 of the recall design):
  1. Convert root to workspace, move `src/` → `crates/schema/
     src/`. Validation gate green; nothing else changes.
  2. Extract shared modules into `crates/schema-core/`. Update
     schema's imports to `use schema_core::…`. Validation
     gate green.
  3. Add `crates/recall/` skeleton with a no-op `mcp-server`
     subcommand returning `pong`. Validation gate green.
  Each commit is reversible without affecting the others.

## Considered options

### Option (a) — Status quo single crate
- Mix bounded contexts in one `src/`.
- Hex boundaries blur.
- Two `[[bin]]` entries inside one Cargo.toml work, but the
  `tool_router` macro from rmcp registers tools per-impl-block;
  cross-bounded-context tools end up in the same router unless
  carefully gated.
- **Rejected.**

### Option (b) — Two crates side by side, no workspace
- `schema/` and `recall/` are siblings with their own
  `Cargo.toml` and `Cargo.lock`.
- Lint policies diverge unless duplicated.
- Dependency resolution diverges (e.g., schema upgrades tokio
  and recall lags).
- **Rejected.**

### Option (c) — Cargo workspace + `schema-core` shared kernel (chosen)
- Root `Cargo.toml` becomes `[workspace]` with three members.
- Three commits, each individually testable.
- Shared `[lints.*]` and `[workspace.dependencies]` blocks
  centralise policy and version pins.
- **Selected.**

### Option (d) — Two separate repositories
- Maximum isolation, maximum overhead.
- Two CIs, two CLAUDE.md files, two release cycles, two
  install procedures.
- For two binaries that share ~30% of their code, the overhead
  is not justified.
- **Rejected.**

## Decision

We adopt **option (c)**. The repo is migrated to a Cargo
workspace in three commits.

### Final layout

```
mcp-schema/                         workspace root
├── Cargo.toml                      [workspace] declaration
├── Cargo.lock                      single lock for all members
├── rust-toolchain.toml             1.95.0 (unchanged)
├── clippy.toml                     thresholds (unchanged)
├── crates/
│   ├── schema-core/                shared kernel (lib)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs              embedder loader, chunkers,
│   │                               generic HNSW wiring,
│   │                               shared Embedding VO
│   ├── schema/                     existing binary (renamed)
│   │   ├── Cargo.toml
│   │   └── src/                    everything currently in src/
│   │                               minus what moved to
│   │                               schema-core
│   └── recall/                     new binary (ADR-0033)
│       ├── Cargo.toml
│       └── src/                    new content
├── arch/                           ADRs + runbook + playbook
│   ├── decisions/
│   └── operations/
├── examples/                       sample schema.toml
├── tests/                          (integration tests will move
│                                    into per-crate `tests/`
│                                    directories during step 1)
└── .github/workflows/lint.yml      now runs `--workspace`
```

### Workspace `Cargo.toml`

```toml
[workspace]
members = ["crates/*"]
resolver = "3"  # Edition 2024 default

[workspace.package]
version    = "0.1.0"
edition    = "2024"
rust-version = "1.95.0"
license    = "MIT OR Apache-2.0"
authors    = ["Fabricio Archanjo"]

[workspace.dependencies]
# pinned once, inherited by member crates as `tokio.workspace = true`
anyhow            = "1"
thiserror         = "2"
serde             = { version = "1.0", features = ["derive"] }
serde_json        = "1.0"
tokio             = { version = "1.52", features = ["full", "test-util"] }
tracing           = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
rmcp              = { version = "1.5", features = ["server", "macros", "transport-streamable-http-server", "transport-io"] }
fastembed         = "..."
# … etc.

[workspace.lints.clippy]
# ADR-0012 Layer A + B, identical to today's [lints.clippy] block
# moved here so member crates inherit via `lints.workspace = true`.

[workspace.lints.rust]
# ADR-0012 Layer C, identical to today's [lints.rust] block

[profile.release]
opt-level = 3
lto       = "fat"
strip     = "symbols"
panic     = "abort"
```

Each member crate's `Cargo.toml` is small:

```toml
# crates/schema/Cargo.toml
[package]
name        = "schema"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[lints]
workspace = true

[dependencies]
schema-core      = { path = "../schema-core" }
anyhow.workspace = true
tokio.workspace  = true
rmcp.workspace   = true
# … crate-specific deps

[[bin]]
name = "schema"
path = "src/main.rs"
```

### Migration commits

#### Commit 1 — `chore(workspace): convert single crate to workspace, move src/ → crates/schema/src/`

- Create `crates/schema/`.
- `git mv src/ crates/schema/src/`.
- Move `tests/` into `crates/schema/tests/`.
- Move `examples/` to workspace root (stays one level up because
  it documents both binaries' consumer-side configs).
- Replace root `Cargo.toml` with workspace declaration; member
  is `crates/schema/`. Member's `Cargo.toml` carries the
  package metadata + dependencies that used to live at root.
- Move `rust-toolchain.toml` and `clippy.toml` — they stay at
  root, where Cargo discovers them workspace-wide.
- Update `.github/workflows/lint.yml`:
  - `cargo clippy --all-targets --all-features --workspace -- -D warnings`
  - `cargo test --workspace --all-features`
- Run validation gate: fmt, clippy, test. **Must be green.**

#### Commit 2 — `refactor(workspace): extract schema-core shared kernel`

- Create `crates/schema-core/` library crate.
- Move into it:
  - `fastembed_embedder.rs` (renamed from
    `crates/schema/src/adapters/`)
  - `markdown_chunker.rs`
  - The line-window chunker (currently inside
    `markdown_chunker.rs` as helper, may need its own file)
  - Generic embedder traits / VOs that don't depend on
    `CorpusKind` (decide on extraction at commit time —
    `Embedding`, `EmbedderError`, the `Embedder` port
    itself).
- `schema-core/Cargo.toml` declares only the dependencies
  the kernel needs (fastembed, serde, anyhow, thiserror).
- `crates/schema/Cargo.toml` adds
  `schema-core = { path = "../schema-core" }`.
- Update schema's imports: `use schema::adapters::
  fastembed_embedder` → `use schema_core::FastembedEmbedder`.
- Run validation gate. **Must be green.**

#### Commit 3 — `feat(workspace): add recall binary skeleton`

- Create `crates/recall/` with minimal `main.rs` that runs
  `rmcp` stdio server and registers a single `ping` tool
  returning `"pong"`.
- `crates/recall/Cargo.toml` adds
  `schema-core = { path = "../schema-core" }`.
- Add `crates/recall/tests/smoke.rs` that spawns the binary
  via `Stdio::piped`, sends `initialize` + `tools/list`,
  asserts `ping` is registered.
- Update `.github/workflows/lint.yml` (already covers
  workspace); no change needed.
- Add `recall` install step to the runbook (will land with
  ADR-0033 implementation but stub the section here).
- Run validation gate. **Must be green.**

### CI / install impact

- **CI lint workflow** changes from per-crate to `--workspace`
  in commit 1. Existing test suite continues to run.
- **Install procedure** (ADR-0014) gets a follow-up step in
  the runbook for the recall binary:
  ```bash
  cargo build --release --workspace
  sudo install -m 0755 target/release/schema /usr/local/bin/schema
  sudo install -m 0755 target/release/recall /usr/local/bin/recall
  sudo codesign --sign "..." --options runtime --force /usr/local/bin/schema
  sudo codesign --sign "..." --options runtime --force /usr/local/bin/recall
  ```
- **Linux portability check** (memory: confirmed at commit
  `0e9e48b` for the single-crate layout) needs re-running on
  the workspace layout in commit 1 to confirm
  `cargo build --workspace` is portable.

## Consequences

- **Good.** Two bounded contexts coexist with hex-boundary
  guarantees. `recall` cannot accidentally `use schema::…`
  because it isn't a declared dependency.
- **Good.** One Cargo.lock, one toolchain pin, one set of
  lint policies (ADR-0012). Updates land once.
- **Good.** Three small commits keep the validation gate
  green at every step. Reverting any single commit leaves
  the repo in a valid state.
- **Good.** Workspace is the explicit Cargo idiom; future
  contributors recognise the shape immediately.
- **Bad / accepted.** Every existing path in
  documentation that says `src/foo/bar.rs` becomes
  `crates/schema/src/foo/bar.rs` after commit 1, then some
  paths become `crates/schema-core/src/...` after commit 2.
  ADR text references and runbook paths need a sweep. The
  diff is mechanical (`s|src/|crates/schema/src/|g` for
  most cases) and committed alongside commit 1.
- **Bad / accepted.** Repository-wide tools (`cargo deny`,
  `cargo audit` if added later) need a workspace-aware
  invocation. Documented in the runbook follow-up.

## Fitness function

- **Build gate post-commit-1:** `cargo build --workspace` exits
  0; `cargo test --workspace --all-features` passes the same
  test count as the pre-migration single crate.
- **Build gate post-commit-2:** `cargo tree -p schema-core
  -e all` shows zero dependency on `crates/schema/`. Schema
  depends on schema-core, never the reverse.
- **Build gate post-commit-3:** `cargo run -p recall --
  mcp-server` produces `pong` for a `ping` tool call sent
  through a piped stdio MCP `initialize` + `tools/call`
  exchange.
- **Lint gate (workspace):** `cargo clippy --workspace
  --all-targets --all-features -- -D warnings` exits 0 at
  every commit. ADR-0012 Layer A `forbid` lints inherited
  via `[workspace.lints.clippy]`; member crates declare
  `[lints] workspace = true`.
- **Doc lint:** the `arch/operations/runbook.md` install
  section references `crates/schema/` and `crates/recall/`
  paths after migration — `grep -rn 'src/' arch/operations/`
  has zero false-positive paths post-migration.

## Cross-references and follow-ups

- **ADR-0001 — Rust + Cargo for the schema binary.** This
  ADR amends ADR-0001's "single crate" assumption. ADR-0001
  gets an `Evidence and amendments` entry on accept.
- **ADR-0012 — strict lint baseline.** Lint policies move to
  workspace root and are inherited via `lints.workspace =
  true`. ADR-0012 Evidence amended.
- **ADR-0013 — Hexagonal.** Workspace makes the hex
  boundaries enforceable at compile time across bounded
  contexts. ADR-0013 Evidence amended.
- **ADR-0014 — install location + codesign.** Install
  procedure now installs **two** binaries; runbook updated.
- **ADR-0033 — Recall bounded context.** This ADR is the
  pre-condition; recall ships in commit 3.
- **Follow-up:** rerun the Linux portability check on the
  workspace layout (memory `project_linux_portability_verified`).
- **Follow-up:** evaluate `cargo deny` + `cargo audit`
  workspace-aware setup (separate ADR if it ships).
- **Follow-up:** the `examples/` directory is currently
  (almost) empty; revisit whether it stays at the workspace
  root or moves into `crates/schema/examples/` after the
  recall binary lands its own samples.

## Evidence and amendments

- **2026-04-27 — commit 2 landed.** New library crate
  `crates/schema-core/` carries the shared kernel:
  `embedder::{Embedder, EmbedError, EMBEDDER_QUERY_PREFIX,
  EMBEDDER_PASSAGE_PREFIX}` + `fastembed_embedder::{FastembedEmbedder,
  BGE_M3_DIMENSIONS, FastembedEmbedderError}`. The fastembed adapter
  is bounded-context-agnostic — `new_bge_m3()` now takes a
  `cache_dir: PathBuf` parameter so each consumer (schema today,
  recall later) chooses where to cache the ONNX weights. The crate
  refuses `pub use` flattening at the root per ADR-0012's `pub_use`
  deny; consumers reach via `schema_core::embedder::Embedder` and
  `schema_core::fastembed_embedder::FastembedEmbedder`. The
  `Embedder` block in `crates/schema/src/ports.rs` was deleted (a
  doc-comment stub points at schema-core); every `use
  crate::ports::Embedder` use site moved to
  `use schema_core::embedder::Embedder`. `crates/schema/Cargo.toml`
  drops the direct `fastembed = "5.13"` dep and adds
  `schema-core = { path = "../schema-core" }`. Validation gate
  green: 139 tests (one removed: the `cache_dir_under_schema_root`
  test that relied on a now-deleted `bge_m3_cache_dir` helper —
  the constructor takes the cache_dir explicitly so the helper
  is no longer needed). `cargo tree -p schema-core -e all` would
  show zero dependency on `crates/schema/` (asserted by
  construction — schema-core declares only `async-trait`,
  `fastembed`, `thiserror`, `tokio`, `tracing`).

- **2026-04-27 — commit 1 landed.** Root `Cargo.toml`
  converted to `[workspace]` + `[workspace.package]` + per-
  layer `[workspace.lints.{clippy,rust}]` blocks (the strict
  ADR-0012 baseline), and `[profile.release]`. New member
  `crates/schema/` carries package metadata via
  `*.workspace = true` shorthand, full dependency block,
  target-specific notify deps, and `[lints] workspace =
  true`. `git mv src/ crates/schema/src/` and `git mv tests/
  crates/schema/tests/` preserved blame across the move.
  Validation gate: `cargo build --workspace` ✓; `cargo fmt
  --all -- --check` ✓; `cargo clippy --all-targets
  --all-features --workspace -- -D warnings` ✓; `cargo test
  --workspace --all-features` ✓ — 140 tests pass (no
  regression vs pre-migration count). `.github/workflows/
  lint.yml` was already workspace-aware (used `--workspace`
  before migration); no change required.
