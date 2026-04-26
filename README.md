# schema

[![lint](https://github.com/farchanjo/schema/actions/workflows/lint.yml/badge.svg)](https://github.com/farchanjo/schema/actions/workflows/lint.yml)

> MCP server that indexes project specs, ADRs, glossaries, and markdown into a
> local RAG (bge-m3 + sqlite-vec) and serves retrieval tools to LLM clients
> over stdio.

`schema` is a [Model Context Protocol](https://modelcontextprotocol.io/) server
that turns a project's documentation tree into a queryable knowledge base for
LLM-based dev tools (Claude Code, Cursor, etc.). One binary serves **any number
of consumer projects** — each project drops a `schema.toml` declaring its
corpus.

The core loop:

1. Walk every directory declared in `schema.toml`.
2. Chunk by kind (markdown, ADR/MADR, glossary, CUE, OpenAPI).
3. Embed with [BGE-M3](https://huggingface.co/BAAI/bge-m3) via
   [`fastembed`](https://crates.io/crates/fastembed) (1024-dim Float32).
4. Persist into a per-project SQLite + [`sqlite-vec`](https://github.com/asg017/sqlite-vec)
   store with FTS5, in WAL mode (ADR-0011).
5. Expose retrieval tools over MCP stdio.

A delta-sync runs on startup and a [`notify`](https://crates.io/crates/notify)-based
watcher keeps the index live during the session (kqueue on macOS, inotify on
Linux — ADR-0010).

## MCP tools

| Tool              | Purpose                                                                |
| ----------------- | ---------------------------------------------------------------------- |
| `ping`            | Liveness probe; returns `'pong'`.                                      |
| `query`           | Semantic search across the whole corpus.                               |
| `find_decisions`  | Semantic search restricted to ADRs.                                    |
| `glossary_lookup` | Term lookup in the project glossary; semantic — synonyms surface.      |
| `cross_reference` | Given an artifact id (e.g. `'ADR-0055'`): definitions + references.    |
| `list_corpus`     | Debug — list every indexed source path.                                |
| `reset_index`     | DESTRUCTIVE — wipe every chunk + manifest for this project (ADR-0015). |
| `forget_source`   | DESTRUCTIVE — drop chunks for one source path (ADR-0015).              |

Tool description style is the verb-led + concrete-example + safety-hint shape
defined in ADR-0016. Live descriptions are in
[`src/adapters/mcp_server.rs`](src/adapters/mcp_server.rs).

## CLI

```text
schema serve     [--config schema.toml]               Start MCP server over stdio.
schema validate  [--config schema.toml]               Validate schema.toml.
schema reset     [--config schema.toml] --yes         DESTRUCTIVE — wipe this project's index.
schema forget    [--config schema.toml] --path <p>    DESTRUCTIVE — drop one source path.
```

`reset` and `forget` share the same library use case as the corresponding MCP
tools (ADR-0015). The CLI requires `--yes` on `reset`; the MCP tools carry
`DESTRUCTIVE —` upfront so well-behaved LLM clients ask the operator first.

## Install (macOS, per ADR-0014)

```bash
git clone https://github.com/farchanjo/schema.git ~/dev/schema
cd ~/dev/schema
mise install                                  # Rust 1.95.0 per .mise.toml

cargo build --release
codesign --sign "Apple Development: <Your Identity>" \
         --options runtime \
         --force \
         target/release/schema
sudo install -m 0755 target/release/schema /usr/local/bin/schema
codesign --verify --verbose=2 /usr/local/bin/schema
schema --version
```

`/usr/local/bin/` is on every macOS shell's default `PATH`, so any Claude Code
spawn context resolves `schema` without per-shell setup. The Apple Development
codesign + Hardened Runtime survives moving the binary between Macs without a
Gatekeeper prompt. Full rationale in
[ADR-0014](arch/decisions/0014-install-and-codesign.md).

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
   ```

2. Validate:

   ```bash
   schema validate --config schema.toml
   ```

3. Wire Claude Code via `.mcp.json` at the consumer root:

   ```json
   {
     "mcpServers": {
       "schema": {
         "command": "schema",
         "args": ["serve", "--config", "schema.toml"]
       }
     }
   }
   ```

   On the next Claude Code session in the project directory, `schema` is
   spawned automatically.

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
| 0006 | LanceDB embedded vector store                                      | superseded by 0011|

Full index in [`arch/decisions/README.md`](arch/decisions/README.md).
Contributors: see [`CLAUDE.md`](CLAUDE.md) for the architecture-first workflow
rule (any non-trivial change starts as an ADR before code).

## Status

- **FASE 1.0** — MVP bootstrap, hexagonal layout, sqlite-vec store, cleanup
  tools, strict lint baseline, codesigned macOS install. **Done.**
- **FASE 1.1** — quality-of-life CLI (`schema doctor`, `schema reindex
  --full`, `schema gc --orphans`), required-CI checks on `main`, hybrid
  search MCP tool (FTS5 ⊕ vector), Homebrew tap evaluation.
- **FASE 2** — code chunking via tree-sitter, LLM-augmented narratives,
  Parquet export.

## License

[Apache-2.0](LICENSE).
