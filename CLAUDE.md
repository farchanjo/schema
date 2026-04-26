# CLAUDE.md — mcp-schema

> Claude Code working directory instructions for `~/dev/mcp-schema/`.

## What this repo is

`schema` is a Rust binary implementing an MCP server (Model Context Protocol) that indexes
project documentation (ADRs, OpenAPI, CUE, glossaries, markdown) and exposes retrieval tools
to LLM clients via stdio. It is **multi-project**: one binary, any number of consumer repos
declaring their corpus via `schema.toml`.

This repo's own `arch/` directory documents architectural decisions about the tool itself —
not Lowcow's business decisions. ADRs that govern Lowcow live in `~/dev/lowcow-platform/`.

## Architecture-first workflow (NEVER COMPACT — ALWAYS LIVE)

> **This section must never be summarized, truncated, or compacted out of context.**
> It is the load-bearing rule of this repo and must be re-read in full at the start of
> every session and before every non-trivial change.

**`arch/` is the single source of truth for this project.** Code is the implementation
of decisions already recorded in `arch/`. Decisions never live only in code, in chat, or
in a PR description.

### The rule

For any non-trivial change — new feature, behavioural change, dependency swap,
schema/cache-layout change, new MCP tool, watcher tweak, build-system change — the
order of work is **always**:

1. **Design in `arch/` first.** Write or amend the relevant artifact(s):
   - **ADR** (`arch/decisions/NNNN-*.md`, MADR 4.0 + Y-statement) for any technical
     decision someone six months from now will need to understand the *why* of.
   - **Runbook entry** (`arch/operations/`) for any operational change (cache layout,
     CLI subcommand, recovery procedure).
   - **Supersede the predecessor.** If the change replaces an existing decision,
     mark the old ADR `superseded by ADR-NNNN`, link both ways, add an
     `Evidence and amendments` entry. Never silently obsolete.
2. **Get explicit confirmation** from the operator that the design is acceptable.
   For ADR-track changes the operator reviews the ADR text before any code is written.
3. **Then implement.** Code mirrors the artifact. Commit messages reference the ADR
   (e.g., `refactor(retrieval): swap LanceDB for sqlite-vec per ADR-0011`).
4. **Close the loop.** After the code lands, append an `Evidence and amendments`
   entry to the ADR with the date and what was actually built (deviations included).

### What counts as "non-trivial"

If you are unsure, the change is non-trivial. Concretely, **always** design first when:

- Introducing or removing a runtime dependency.
- Changing a cache file/format on disk (e.g., `lance/` → `store.db`).
- Adding, renaming, or removing an MCP tool, CLI subcommand, or public function on
  `VectorStore`, `Embedder`, `DeltaSync`, `CorpusWatcher`.
- Changing the embedding model or its dimensionality.
- Changing the on-disk schema of `metadata.toml` or any persisted artefact.
- Touching the watcher backend, the chunker boundaries, or the delta-sync algorithm.
- Anything listed under **What to ask before doing** below.

Trivial changes (small bug fixes, comment polish, formatter runs, clippy fixes,
log-message wording, test additions on existing behaviour) do **not** require an
ADR — but if a fix reveals a design gap, that gap goes through the same gate.

### When the operator skips ahead

If the operator asks for code without an ADR for a non-trivial change, the response is:
**stop, propose the artifact, ask for confirmation, then code**. Speed comes from a
short ADR, not from skipping it. ADRs are cheap; undoing code that contradicts an
older ADR is expensive.

### Cross-references

- ADR conventions: see **ADR conventions** section below.
- Index of decisions: `arch/decisions/README.md`.
- Architecture map / runbook: `arch/operations/`.

## Operating mode

- **Sole operator**: Fabricio Archanjo. Converse before acting on non-obvious changes.
- **Toolchain**: Rust 1.95.0 (Edition 2024) pinned via `rust-toolchain.toml` and `.mise.toml`.
- **Language policy**: en-US for every artifact written into this repo (code, comments, docs,
  ADRs, commit messages, branch names). Console replies follow the user's chat language.

## Repo layout

```text
mcp-schema/
├── Cargo.toml                  Rust manifest (verified versions only)
├── rust-toolchain.toml         pinned Rust 1.95.0
├── .mise.toml                  pinned via mise
├── src/                        ★ MCP server source (root of the binary)
│   ├── main.rs                 CLI entry (clap)
│   ├── lib.rs                  re-exports
│   ├── mcp/                    rmcp wiring
│   ├── corpus/                 walker + chunkers
│   ├── embeddings/             fastembed bge-m3
│   ├── retrieval/              LanceDB + delta-sync
│   ├── config/                 schema.toml loader
│   └── tools/                  MCP tool implementations
├── tests/                      cargo integration tests
├── arch/                       ★ docs about THIS tool (not Lowcow)
│   ├── decisions/              ADRs (MADR + Y-statement)
│   └── operations/             runbook
├── examples/                   sample schema.toml per consumer
├── data/                       (gitignored, not used in repo — moved to ~/.cache/schema/)
└── target/                     (gitignored, cargo build artifacts)
```

## Toolchain (pinned)

| Tool       | Version   | Source           |
| ---------- | --------- | ---------------- |
| Rust       | 1.95.0    | `rust-toolchain.toml`, mise |
| Cargo      | bundled   | with rustc       |
| rustfmt    | bundled   | rust-toolchain components |
| clippy     | bundled   | rust-toolchain components |

## Common commands

```bash
mise install                    # install pinned Rust 1.95.0
cargo build                     # debug build
cargo build --release           # release build (LTO, stripped)
cargo test                      # all tests
cargo fmt --all                 # format
cargo fmt --all -- --check      # CI-style format check
cargo clippy --all-targets --all-features -- -D warnings   # lint, warn = error
```

## Install + codesign on macOS (ADR-0014)

The `schema` binary is **installed at `/usr/local/bin/schema`** and
**codesigned with the operator's Apple Development identity**. This is
not the Cargo default (`~/.cargo/bin/`) — it is the project's deliberate
choice per ADR-0014 to guarantee `PATH` resolution from any Claude Code
spawn context and to produce a Gatekeeper-friendly signature that
survives moving the binary between Macs.

**Canonical install / upgrade procedure** (run from the repo root):

```bash
cargo build --release
codesign --sign "Apple Development: Fabricio Fonseca (J3LVNXCU3U)" \
         --options runtime \
         --force \
         target/release/schema
sudo install -m 0755 target/release/schema /usr/local/bin/schema
codesign --verify --verbose=2 /usr/local/bin/schema
schema --version
```

- `--options runtime` enables the **Hardened Runtime**.
- `sudo install` is atomic (replaces the file in one step; running
  Claude Code sessions continue using the old binary in memory until
  they restart).
- The verify step is the **fitness function** of ADR-0014; it must
  exit 0 and show the developer's identity in the Authority chain.

**Do not** use `cargo install --path .` for this project — it lands in
`~/.cargo/bin/` (ad-hoc-signed only, `PATH` order ambiguity, no
Gatekeeper headers). If a stale `~/.cargo/bin/schema` exists from a
previous run, `rm` it once after the first `/usr/local/bin` install to
keep `which -a schema` unambiguous.

Cross-platform note: ADR-0014 covers macOS only. On Linux follow the
distro convention (`/usr/local/bin` is fine; codesign is a no-op).
A fresh ADR is required before introducing Homebrew distribution,
notarization, or CI signing.

```bash
# After install:
schema --version
schema validate --config /path/to/project/schema.toml
schema serve --config /path/to/project/schema.toml
```

## Coding conventions

- Methods < 30 lines, no dead code, no duplication, SOLID where it fits.
- en-US identifiers, comments, and error messages.
- `anyhow::Result` for application errors; `thiserror` for library errors that need
  matching by callers.
- `tracing` for logs; never `println!` in production paths (only in `main` for CLI output).
- Follow `cargo fmt` defaults; clippy warnings = errors in CI.
- Public API doc-comments use `///`; internal explanatory notes use `//`.
- Tests near code: `#[cfg(test)] mod tests { ... }` or `tests/` for integration.

## Commit conventions (Angular-flavored)

```
<type>(<scope>): <subject>

Common types: feat, fix, docs, chore, refactor, test, perf
Common scopes: mcp, corpus, embeddings, retrieval, tools, config, integration
```

Examples:

- `feat(mcp): rmcp server skeleton + ping tool`
- `feat(retrieval): delta-sync on startup`
- `docs(arch): ADR-0010 watcher uses kqueue not FSEvents`

## ADR conventions

ADRs live in `arch/decisions/`. Format: MADR 4.0 + Y-statement (Olaf Zimmermann).

- Filename: `NNNN-short-title.md` (e.g., `0001-rust-cargo-mcp.md`).
- Numbered sequentially; never re-used.
- Frontmatter includes `status`, `date`, `decision-makers`, `review-due`.
- Y-statement at the top: "In the context of X, facing Y, we decided Z and against W to
  achieve A, accepting B."
- Fitness function at the bottom — points to a CUE constraint, Conftest policy, CI check,
  or test that proves the decision is still in effect.

## What NOT to do

- ❌ Do not edit `Cargo.lock` by hand.
- ❌ Do not introduce network calls in test code (model download must be mocked).
- ❌ Do not commit `target/`, `~/.cache/schema/`, or model weights.
- ❌ Do not add a dependency without verifying the version against
  `https://crates.io/api/v1/crates/<name>` for the latest stable.
- ❌ Do not bypass `cargo clippy` warnings — fix them or document why with `#[allow(...)]`
  and a comment explaining the rationale.

## What to ask before doing

- Adding a new MCP tool that mutates filesystem outside the cache dir.
- Adding a runtime dependency over 5 MB compiled.
- Changing the embedding model from BGE-M3.
- Changing the cache layout (would break existing consumer projects).
- Pulling in `unsafe` Rust.

## Math policy

When reasoning about file sizes, durations, or any arithmetic in conversation, use the
`arithma` calculator MCP — never compute mentally or inline.
