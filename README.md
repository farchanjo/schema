# schema

[![lint](https://github.com/farchanjo/schema/actions/workflows/lint.yml/badge.svg)](https://github.com/farchanjo/schema/actions/workflows/lint.yml)

> MCP server that indexes project specs, ADRs, glossaries, and markdown into a
> local RAG (bge-m3 + sqlite-vec) and serves retrieval tools to LLM clients
> over **Streamable HTTP** (ADR-0019; one server per project, N clients).

`schema` is a [Model Context Protocol](https://modelcontextprotocol.io/) server
that turns a project's documentation tree into a queryable knowledge base for
LLM-based dev tools (Claude Code, Cursor, etc.). One binary serves **any number
of consumer projects** — each project drops a `schema.toml` declaring its
corpus, and one OS service (launchd / systemd) per project owns the running
HTTP MCP daemon.

The core loop:

1. Walk every directory declared in `schema.toml`.
2. Chunk by kind (markdown, ADR/MADR, glossary, CUE, OpenAPI).
3. Embed with [BGE-M3](https://huggingface.co/BAAI/bge-m3) via
   [`fastembed`](https://crates.io/crates/fastembed) (1024-dim Float32).
4. Persist into a per-project SQLite + [`sqlite-vec`](https://github.com/asg017/sqlite-vec)
   store with FTS5, in WAL mode (ADR-0011).
5. Expose retrieval tools over MCP Streamable HTTP (rmcp 1.5 + axum 0.8) on
   `127.0.0.1:<kernel-assigned-port>` with per-project bearer-token auth
   (ADR-0019, ADR-0021).

A delta-sync runs on startup with a `mtime + size` short-circuit
(ADR-0017 — idle re-syncs avoid `blake3`-hashing unchanged files), and a
[`notify`](https://crates.io/crates/notify)-based watcher keeps the index live
during the session (kqueue on macOS, inotify on Linux — ADR-0010).

> **Verified on:** macOS (operator workstation, ADR-0014 codesign) and
> Ubuntu 24.04 LTS (Linux build VM, kernel 6.17, glibc 2.39). Tests use
> POSIX-only APIs (`OpenOptions::mode(0o600)` for `endpoint.toml`,
> `unix::fs::PermissionsExt`) and have not been validated on Windows; the
> binary's CI matrix is Linux + stable Rust 1.95.0.

## MCP tools

| Tool                | Purpose                                                                |
| ------------------- | ---------------------------------------------------------------------- |
| `ping`              | Liveness probe; returns `'pong'`.                                      |
| `workspace_context` | Project + corpus + embedding context. Useful at session start to confirm `.mcp.json` points at the right server (ADR-0009 amendment, ADR-0023). |
| `query`             | Semantic search across the whole corpus.                               |
| `find_decisions`    | Semantic search restricted to ADRs.                                    |
| `glossary_lookup`   | Term lookup in the project glossary; semantic — synonyms surface.      |
| `cross_reference`   | Given an artifact id (e.g. `'ADR-0055'`): definitions + references.    |
| `list_corpus`       | Debug — list every indexed source path.                                |
| `reset_index`       | DESTRUCTIVE — wipe every chunk + manifest for this project (ADR-0015). |
| `forget_source`     | DESTRUCTIVE — drop chunks for one source path (ADR-0015).              |

Tool description style is the verb-led + concrete-example + safety-hint shape
defined in ADR-0016. Live descriptions are in
[`src/adapters/mcp_server.rs`](src/adapters/mcp_server.rs).

## CLI

```text
schema serve       [--config schema.toml]                           Start the MCP server over Streamable HTTP (ADR-0019).
schema validate    [--config schema.toml]                           Validate schema.toml.
schema reset       [--config schema.toml] --yes                     DESTRUCTIVE — wipe this project's index.
schema forget      [--config schema.toml] --path <p>                DESTRUCTIVE — drop one source path.
schema install     [--config schema.toml] --service [--binary-path] Render + write the per-project launchd plist / systemd unit (ADR-0020).
schema uninstall   [--config schema.toml] --service                 Remove the per-project plist / unit.
schema service     status [--config schema.toml]                    Show URL + lifecycle for the running server.
schema mcp-config  [--config schema.toml]                           Print mcpServers JSON snippet for .mcp.json (ADR-0021).
```

Path resolution for `--config` follows ADR-0023: explicit `--config` wins,
otherwise `SCHEMA_CONFIG` env, otherwise walk-up CWD → FS root looking for
`schema.toml` (cargo-style). Per-knob overrides via `SCHEMA_*` env vars
override `schema.toml` values; the server logs the resolved source on startup.

`reset` and `forget` share the same library use case as the corresponding MCP
tools (ADR-0015). The CLI requires `--yes` on `reset`; the MCP tools carry
`DESTRUCTIVE —` upfront so well-behaved LLM clients ask the operator first.

## Install (macOS, per ADR-0014)

```bash
git clone https://github.com/farchanjo/schema.git ~/dev/schema
cd ~/dev/schema
mise install                                  # Rust 1.95.0 per .mise.toml

cargo build --release
sudo install -m 0755 target/release/schema /usr/local/bin/schema
sudo codesign --sign "Apple Development: <Your Identity>" \
              --options runtime \
              --force \
              /usr/local/bin/schema
codesign --verify --verbose=2 /usr/local/bin/schema
schema --version
```

`/usr/local/bin/` is on every macOS shell's default `PATH`, so any Claude Code
spawn context resolves `schema` without per-shell setup. **Codesign is signed
in place at the final destination** — signing the cargo output and then
`sudo install`-copying introduces filesystem-attribute / quarantine-flag
race conditions on certain APFS/SMB combinations. Signing under `sudo` at
`/usr/local/bin/schema` is atomic and inherits no `com.apple.quarantine`.
Full rationale in [ADR-0014](arch/decisions/0014-install-and-codesign.md).

**Codesign is optional.** Without an Apple Development identity, skip the
`codesign` step — the binary runs unsigned and Gatekeeper warns once on
first launch (right-click → Open or `xattr -d com.apple.quarantine
/usr/local/bin/schema` clears it). Service mode (`schema install --service`,
ADR-0020) is more reliable with a codesigned binary; without codesign,
drop the Hardened Runtime flag from the rendered plist or accept that a
future macOS may refuse to load the LaunchAgent.

`cargo install --path .` is **not** the supported install — it lands in
`~/.cargo/bin/` (ad-hoc-signed only, `PATH` order ambiguity).

## Install (Linux)

```bash
cargo build --release
sudo install -m 0755 target/release/schema /usr/local/bin/schema
schema --version
```

Codesign is a macOS concept; ADR-0014 scopes only macOS.

## Wire a consumer project

In the consumer repo (e.g. `~/dev/lowcow-platform`):

1. Drop a `schema.toml` at the repo root:

   ```toml
   [project]
   name = "lowcow-platform"
   version = "1"

   [[corpus]]
   path = "docs/decisions"
   kind = "adr-madr"

   [[corpus]]
   path = "docs/glossary.md"
   kind = "glossary"

   [[corpus]]
   path = "docs/business-rules"
   kind = "markdown"

   # Optional knobs (ADR-0018):
   [embedding]
   nice = 5     # OS scheduler nice value, 0..=19, default 5
   ```

2. Validate (works from any subdirectory; walk-up finds the file —
   ADR-0023):

   ```bash
   schema validate                          # walk-up CWD → /
   schema validate --config schema.toml     # explicit
   ```

3. Install the per-project service (ADR-0020):

   ```bash
   schema install --service --config schema.toml
   ```

   Renders a launchd plist (macOS) or systemd user unit (Linux) and prints
   the `launchctl bootstrap` / `systemctl --user enable --now` command to
   load it.

4. Generate the consumer-side `.mcp.json` snippet (ADR-0021):

   ```bash
   schema mcp-config --config schema.toml
   ```

   Output is the `mcpServers.schema` block carrying the URL + bearer
   token from `endpoint.toml`. Paste into the consumer's `.mcp.json`:

   ```json
   {
     "mcpServers": {
       "schema": {
         "url": "http://127.0.0.1:48291",
         "headers": {
           "Authorization": "Bearer f47ac10b-58cc-..."
         }
       }
     }
   }
   ```

   The token rotates on every server restart; re-run `schema mcp-config`
   and re-paste after a restart. A `schema mcp-shim` proxy that re-reads
   `endpoint.toml` on `401` is the planned follow-up (ADR-0021).

5. Open Claude Code in the project directory; it connects to the running
   HTTP MCP server. Confirm with `claude mcp list` or by asking the LLM
   to call the `workspace_context` tool.

## Configuration via ENV (ADR-0023)

`SCHEMA_*` env vars override `schema.toml` knobs at process-start with
12-factor precedence (`ENV > file > compiled-in default`). The server
logs the resolved source for every knob on startup.

| ENV                                  | Maps to                       | Default     |
|--------------------------------------|-------------------------------|-------------|
| `SCHEMA_CONFIG`                      | `--config` path               | walk-up CWD |
| `SCHEMA_EMBEDDING_MODEL`             | `[embedding] model`           | `bge-m3`    |
| `SCHEMA_EMBEDDING_NICE`              | `[embedding] nice`            | `5`         |
| `SCHEMA_RETRIEVAL_TOP_K_DEFAULT`     | `[retrieval] top_k_default`   | `8`         |
| `SCHEMA_RETRIEVAL_CHUNK_SIZE_MAX`    | `[retrieval] chunk_size_max`  | `8192`      |
| `SCHEMA_RETRIEVAL_FILE_SIZE_MAX`     | `[retrieval] file_size_max`   | `5242880`   |
| `SCHEMA_SECURITY_FOLLOW_SYMLINKS`    | `[security] follow_symlinks`  | `false`     |

`RUST_LOG` controls log verbosity (`info`, `debug`, `trace`); not prefixed
`SCHEMA_*` because it is the standard `tracing` filter env.

## Architectural patterns

The codebase uses three overlapping disciplines, idiomatically rather than
ceremonially:

| Discipline | How it shows up | Anchor |
|---|---|---|
| **Hexagonal** (Ports & Adapters) | `src/{domain,ports,app,adapters}/` — pure types and trait boundaries inside, concrete impls outside; one-file persistence swap (LanceDB → sqlite-vec) was the proof | ADR-0013 |
| **DDD tactical** | Value Objects (`Chunk`, `FileMeta`, `Endpoint`, `DiscoveredFile`), Repositories (`Persistence`, `MetadataStore`), Application Services (`Query`, `Cleanup`, `DeltaSync`), domain Specifications (`Metadata::classify`, ADR-0013 amendment) | implicit |
| **DDD strategic** | one bounded context (`schema corpus indexer`); ubiquitous language (chunk, corpus, project_id, artifact_id) consistent across code + ADRs + runbook | implicit |
| **GoF — 9 of 23 patterns** | Adapter (every adapter), Decorator/Chain of Responsibility (tower middleware in `build_router`), Factory Method (`new_*` / `with_state` / `open`), Singleton (`Once` for sqlite-vec extension load), Strategy (rmcp factory closure), Command (each `#[tool]`), Facade (`Query`, `Cleanup`), Observer (notify watcher → `mpsc` channel), Iterator (walkdir, vec) | implicit |

Patterns we **do not** force: Builder, Abstract Factory, Visitor, Interpreter,
Memento, Mediator, State, Flyweight, Composite — Rust traits + `derive(Default)`
+ closures cover the same intent without class-hierarchy ceremony. Patterns we
**do not need yet**: full Aggregate-Root with domain-event bus (one bounded
context, no event sourcing), Anti-Corruption Layer (no legacy system to
shield from). Both are explicit non-decisions in ADR-0013 §amendments.

## Architecture

```text
src/
├── main.rs          driver adapter (CLI; clap builder API)
├── lib.rs           module wiring
├── domain.rs        pure types (Chunk, ChunkRecord, FileMeta, Metadata, …)
├── ports.rs         async traits — Persistence, Embedder, Walker, Watcher,
│                                   Chunker, MetadataStore
├── app/             application services
│   ├── delta_sync.rs        startup + watcher-driven re-embed
│   ├── query.rs             query / find_decisions / glossary_lookup /
│   │                        cross_reference orchestration
│   ├── watcher_consumer.rs  debounce + flush
│   └── cleanup.rs           reset_index / forget_source use cases
└── adapters/
    ├── lancedb_store.rs            (deleted, see ADR-0011)
    ├── sqlite_vec_store.rs         Persistence — SQLite + sqlite-vec + FTS5 + WAL
    ├── fastembed_embedder.rs       Embedder
    ├── filesystem.rs               Walker + Watcher
    ├── markdown_chunker.rs         Chunker
    ├── toml_config.rs              schema.toml loader
    ├── project_identity.rs         ProjectIdentity (cache path resolution)
    ├── metadata_store.rs           MetadataStore (TOML)
    └── mcp_server.rs               rmcp driving adapter
```

Hexagonal-lite per [ADR-0013](arch/decisions/0013-hexagonal-architecture.md):
domain and ports have zero adapter-specific imports; adapters implement ports
and may import any external dep; `app/` services depend only on domain + ports
+ stdlib + tokio. The persistence swap (ADR-0011) was a one-file change in
`adapters/`.

## Cache layout

```text
~/.cache/schema/
├── models/
│   └── bge-m3/                         ~2 GB ONNX weights, shared across projects
└── projects/
    └── <project-name>-<blake3-16hex>/
        ├── store.db                    SQLite + sqlite-vec store
        ├── store.db-wal                WAL journal
        ├── store.db-shm                WAL shared memory
        ├── metadata.toml               delta-sync manifest
        └── lock                        advisory lock (FASE 1.1)
```

`<project-name>` is the sanitised `[project] name`; the hash is the first 64
bits of BLAKE3 over the canonical absolute project path. Renaming or moving
a project produces a fresh cache directory (ADR-0008).

Wipe an index in-session via the MCP tool `reset_index`; off-session via
`schema reset --yes`. Drop one stale doc via `forget_source` /
`schema forget --path …`. Files on the consumer's disk are never touched.

## Development

```bash
mise install                                                    # Rust 1.95.0
cargo build                                                     # debug
cargo test --all-features                                       # unit + integration
cargo fmt --all                                                 # format
cargo fmt --all -- --check                                      # CI-style
cargo clippy --all-features --all-targets --workspace -- -D warnings
```

Strict lint baseline (ADR-0012): Layer A `forbid` for safety lints, Layer B
groups + 29 quality denies, Layer C `unsafe_code = "deny"` (one narrow
`#[expect(unsafe_code, reason)]` block at the sqlite-vec extension load site).
The CI workflow `.github/workflows/lint.yml` runs the canonical clippy command
on every push and PR.

## Architectural decisions

Every non-trivial decision lives in [`arch/decisions/`](arch/decisions/) as a
[MADR 4.0](https://adr.github.io/madr/) record with a Y-statement and a fitness
function. The latest are:

| #    | Title                                                              | Status            |
| ---- | ------------------------------------------------------------------ | ----------------- |
| 0011 | SQLite + sqlite-vec embedded store                                 | accepted          |
| 0012 | Strict lint baseline (Layer A forbid + Layer B activation)         | accepted          |
| 0013 | Hexagonal architecture (ports & adapters)                          | accepted          |
| 0014 | Install at `/usr/local/bin` + Apple codesign on macOS              | accepted          |
| 0015 | Cleanup tools at both MCP and CLI surfaces                         | accepted          |
| 0016 | MCP tool description style: verb + example + safety hint           | accepted          |
| 0017 | `mtime + size` short-circuit before `blake3` hash                  | accepted          |
| 0018 | Embedder CPU cap via process-level scheduler nice                  | accepted          |
| 0019 | MCP transport: Streamable HTTP via rmcp + axum                     | accepted          |
| 0020 | Permanent service lifecycle: launchd + systemd, one per project    | accepted          |
| 0021 | Localhost bind + per-project bearer-token auth                     | accepted          |
| 0022 | Bench-driven evaluation of `tokio-uring` (Linux only, gated)       | accepted (gate)   |
| 0023 | Config resolution: walk-up + 12-factor ENV overlay                 | accepted          |
| 0002 | rmcp 1.5 over stdio for MCP transport                              | superseded by 0019|
| 0006 | LanceDB embedded vector store                                      | superseded by 0011|

Full index in [`arch/decisions/README.md`](arch/decisions/README.md).
Contributors: see [`CLAUDE.md`](CLAUDE.md) for the architecture-first workflow
rule (any non-trivial change starts as an ADR before code).

## Status

- **FASE 1.0** — MVP bootstrap, hexagonal layout, sqlite-vec store, cleanup
  tools, strict lint baseline, codesigned macOS install. **Done.**
- **FASE 1.0.1 (2026-04-26)** — Streamable HTTP MCP transport (ADR-0019),
  per-project launchd / systemd service (ADR-0020), bearer-token auth
  (ADR-0021), `mtime + size` short-circuit (ADR-0017), embedder CPU cap
  via scheduler nice (ADR-0018), `SCHEMA_*` ENV overlay + walk-up config
  resolution (ADR-0023), `workspace_context` MCP tool (ADR-0009 amend).
  **Done.**
- **FASE 1.1** — quality-of-life CLI (`schema doctor`, `schema reindex
  --full`, `schema gc --orphans`), required-CI checks on `main`, hybrid
  search MCP tool (FTS5 ⊕ vector), `schema mcp-shim` proxy that re-reads
  `endpoint.toml` on `401` (kills the token-rotation friction in ADR-0021),
  bench harness for ADR-0022 if cold-reindex P3 baseline warrants.
- **FASE 2** — code chunking via tree-sitter, LLM-augmented narratives,
  Parquet export, multi-project routing in a single daemon (ADR-0020
  sub-2.B option).

## OS prerequisites for tests

The integration tests under `tests/` and many of the unit tests use
POSIX-only APIs:

- `std::os::unix::fs::OpenOptionsExt::mode(0o600)` to enforce
  `endpoint.toml` permissions.
- `std::os::unix::fs::PermissionsExt` to assert mode bits.
- `tokio::signal::ctrl_c()` for graceful shutdown.

`cargo test --all-features` therefore requires a Unix-like host. The
project's CI matrix runs on `ubuntu-latest` (GitHub Actions, ADR-0012
strict lint baseline). The operator's local matrix is **macOS** (primary
workstation, ADR-0014 codesign) plus a **Linux build VM** (Ubuntu 24.04
LTS, kernel 6.17, glibc 2.39, x86_64) used for cross-platform smoke
before tagged releases. **Windows is not tested**; the binary may run on
WSL2 (untested) but native Windows requires porting the POSIX permission
helpers — out of scope for FASE 1.0.

## License

[Apache-2.0](LICENSE).
