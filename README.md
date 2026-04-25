# schema

> MCP server for indexing project specs, ADRs, contracts, and glossaries via local RAG.

`schema` is a [Model Context Protocol](https://modelcontextprotocol.io/) server that turns
any project's documentation tree into a queryable knowledge base for LLM-based development
tools (Claude Code, Cursor, etc.). It is **multi-project**: a single binary serves any number
of repositories, each declaring its own corpus via `schema.toml`.

## What it does

Each project consumer drops a `schema.toml` at the repo root declaring which directories to
index and how. `schema` indexes the corpus locally via [`fastembed`](https://crates.io/crates/fastembed)
(BGE-M3 multilingual embeddings) and persists vectors in [`lancedb`](https://crates.io/crates/lancedb)
(embedded, columnar, MVCC-safe). MCP tools expose retrieval primitives over `stdio`.

## Tools exposed (FASE 1)

| Tool               | Purpose                                              |
| ------------------ | ---------------------------------------------------- |
| `query`            | Generic RAG (top-K nearest chunks for a query).      |
| `find_decisions`   | Semantic search restricted to `kind = adr-madr`.     |
| `glossary_lookup`  | Term → definition (kind = `glossary`).               |
| `cross_reference`  | Given an artifact ID, find chunks that reference it. |
| `list_corpus`      | Debug — list everything indexed for the project.     |

## Project consumers

Each project that uses `schema` ships a `schema.toml` like:

```toml
[project]
name = "lowcow-platform"
version = "1"

[[corpus]]
path = "docs/decisions"
kind = "adr-madr"

[[corpus]]
path = "docs/business-rules"
kind = "markdown"

[[corpus]]
path = "docs/glossary.md"
kind = "glossary"

[[corpus]]
path = "schemas/integrations"
kind = "cue"

[[corpus]]
path = "schemas/api"
kind = "openapi"
```

And a `.mcp.json` to wire Claude Code to the binary:

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

## Architecture

`schema` keeps each project's index isolated under
`~/.cache/schema/projects/<name>-<hash>/`. The hash is BLAKE3 of the absolute canonical
project path, so renaming or moving a project produces a fresh cache (no accidental
cross-contamination).

On every startup, `schema` performs a **delta sync** — comparing each declared corpus file's
`mtime` and content hash against the persisted metadata, re-embedding only what changed.
Files removed from disk are pruned from the index. This means the index always reflects
the current state of the project's documentation, without the overhead of re-embedding
everything.

A `notify`-based filesystem watcher keeps the index live during the session. On macOS the
watcher uses `kqueue` (per ADR-0010); on Linux it uses `inotify`.

See `arch/decisions/` for the full rationale on every design choice.

## Install (FASE 1 — local dev)

```bash
git clone <this-repo> ~/dev/mcp-schema
cd ~/dev/mcp-schema
mise install         # installs Rust 1.95.0
cargo install --path .
```

After install, `schema` is on your `PATH`.

## CLI (FASE 1)

```text
schema serve [--config <path>]      Start MCP server over stdio (default).
schema validate [--config <path>]   Validate schema.toml without starting.
```

`reindex`, `doctor`, `search`, and `gc` are deferred to FASE 1.1.

## Status

**FASE 1.0** — MVP bootstrap. Tools functional but minimally tested.
**FASE 1.1** — quality-of-life CLI, hooks, additional kinds.
**FASE 2**   — code chunking via tree-sitter, LLM-augmented narratives, web UI.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
