---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0001 — Rust + Cargo for the `schema` binary

> **Y-statement** — In the context of needing a **long-running daemon** that
> Claude Code spawns as a stdio subprocess on every session open, performs
> RAG (embedding + vector search), watches the filesystem, and must start
> in well under one second to avoid degrading the user's first interaction,
> facing the choice between **Python** (fast bootstrap, mature ML ecosystem
> but heavy startup, ~250 MB RSS, ~2 s cold start) and **Rust** (longer
> bootstrap, younger ML ecosystem, ~50 MB RSS, sub-100 ms cold start),
> we decided for **Rust + Cargo (Edition 2024, pinned to 1.95.0)** and
> against Python + uv to achieve a single-binary daemon with predictable
> low-memory low-latency behaviour suitable for the per-session subprocess
> lifecycle, accepting that bootstrap takes ~2-3× the equivalent Python
> work and that the Rust ML ecosystem is younger (mitigated by `fastembed`
> ADR-0005 + `lancedb` ADR-0006 which are production-grade today).

## Context and Problem Statement

`schema` runs as a stdio subprocess of Claude Code. Each session opens →
Claude Code spawns the daemon → daemon stays alive until the session
closes. The hot path is:

1. **Cold start** — daemon spawn + state init must be sub-second so the
   user does not feel a stall on the first MCP tool call.
2. **Hot path** — every MCP request (query, find_decisions, etc.) must
   complete within tens of ms (excluding ML compute).
3. **Memory** — the daemon shares the user's machine with editor, browser,
   Claude Code itself. Footprint matters.

Two realistic candidates: Python (mature ML, slow startup) and Rust
(young ML, fast startup).

## Decision Drivers

- **Daemon startup latency.** Cold Python: 1-2 s for a non-trivial app.
  Rust: 30-100 ms typical. Daemon spawning is the user's first impression
  of the tool.
- **Memory footprint.** A Python interpreter alone is ~80 MB; with
  fastembed + LanceDB transitive deps loaded the RSS easily exceeds
  300 MB. Rust binary ~50 MB plus model weights (~2 GB on disk for
  bge-m3, but mmaped and shared across processes via OS page cache).
- **Compile-time safety.** `unsafe_code = "forbid"`; ownership rules
  catch concurrency bugs the daemon would otherwise hit at 3 AM.
- **Single binary deploy.** `cargo install --path .` puts one file on
  PATH. Python equivalents (pipx, conda) are heavier and platform-quirky.
- **AI tooling trend.** Astral wrote `uv` and `ruff` in Rust precisely
  because long-running Python tools struggle with the same issues we
  are solving here. The bet is Rust ML ecosystem catches Python within
  3 years; today it is good enough for embeddings + vector search.

## Considered Options

### Option A — Python + uv (rejected)

`fastembed-py`, `chromadb`, `mcp` Python SDK, `watchdog`. uv handles
the package manager pain.

Implication: faster bootstrap (~1-2 days vs 5-7), broader ML library
choice. But cold start adds ~1.5 s per session-open, and the daemon
RSS sits ~250 MB. For a tool the user spawns 10× a day across multiple
projects, the cumulative latency and memory pressure is meaningful.

### Option B — Rust + Cargo (chosen)

`rmcp` (ADR-0002), `fastembed-rs` (ADR-0005), `lancedb` (ADR-0006),
`notify` (ADR-0010), `tokio` for async I/O.

Implication: 2-3× longer initial implementation; younger ML ecosystem
(some HF models lack Rust ports yet — bge-m3 is supported). But cold
start ~50 ms, RSS ~50 MB, single static binary. The trade matches the
daemon shape.

### Option C — Go (not pursued)

Would compete on startup + memory. But the Go ML ecosystem is even
thinner than Rust's; `lancedb` has no Go bindings at the time of this
ADR.

## Decision Outcome

Chosen option: **Rust + Cargo**.

### Pinned versions

- Rust toolchain: **1.95.0** (Edition 2024). Pinned in
  `rust-toolchain.toml` and `.mise.toml`.
- All crate versions pinned to minor, never to floating ranges.

### Why Edition 2024

- Stable since Rust 1.85 (Feb 2025).
- Default match ergonomics improvements (`if let && ...` chains).
- Standardised `&raw const`/`&raw mut` for raw pointer ergonomics
  (not used here today; future-proof).

## Consequences

- **Good:** sub-100 ms cold start; ~50 MB RSS; single static binary;
  compile-time safety on the daemon hot path.
- **Good:** future-proof — Rust ML ecosystem (candle, ort, fastembed,
  lancedb) is growing fast.
- **Bad:** 2-3× longer initial implementation than Python; ~5-7 days
  for FASE 1.0 vs ~1-2 days in Python.
- **Bad:** ML model availability is narrower than Python. Today bge-m3
  is supported (ADR-0005); a future model swap may require model
  ONNX export work.
- **Neutral:** ecosystem of derive macros + traits adds compile-time
  cost. Mitigated by Cargo's incremental compilation and CI cache via
  `Swatinem/rust-cache`.

## Fitness function

- `cargo clippy --all-features --all-targets --workspace -- -D warnings`
  in CI gates every PR. Per `Cargo.toml [lints.clippy]` the strict
  baseline (Layer A + Layer C) blocks regressions.
- `rust-toolchain.toml` pins `1.95.0`; mise reads the same version.
  Drift between local and CI is impossible.
- A future amendment to switch language requires this ADR to be
  superseded.

## More information

- `Cargo.toml` — pinned dep versions.
- `rust-toolchain.toml` — pinned compiler version.
- `.github/workflows/lint.yml` — strict gate.
- ADR-0002 — rmcp + stdio, the MCP-specific binding.
- ADR-0005, ADR-0006 — ML ecosystem choices on top of Rust.
