# Architecture Decision Records — `mcp-schema`

ADRs governing the **`schema` tool itself**. Decisions about the consumer
projects (Lowcow, future) live in their own repos.

Format: **MADR 4.0 + Y-statement** — each ADR opens with a one-sentence
Y-statement (Olaf Zimmermann pattern) and closes with a Fitness function
pointing at the CI check that proves the decision is still in effect.

## Conventions

- Filename: `NNNN-short-title.md` (e.g., `0001-rust-cargo-mcp.md`).
- Sequentially numbered; never re-used.
- Status lifecycle: `proposed` → `accepted` → (`deprecated` | `superseded by ADR-NNNN`).
- Frontmatter: `status`, `date`, `decision-makers`, `review-due`.
- Decision proper (Context, Decision, Consequences) is immutable once
  accepted; superseding requires a new ADR.
- `Evidence and amendments` section is a living log — date-stamped entries
  for incidents, real-world results, vendor changes.

## Index

| #    | Title                                                                                  | Status   | Date       |
| ---- | -------------------------------------------------------------------------------------- | -------- | ---------- |
| 0001 | [Rust + Cargo for the schema binary](./0001-rust-cargo-mcp.md)                          | accepted | 2026-04-25 |
| 0002 | [rmcp 1.5 over stdio for MCP transport](./0002-rmcp-stdio.md)                           | accepted | 2026-04-25 |
| 0003 | [Multi-project architecture (one binary, many consumers)](./0003-multi-project-architecture.md) | accepted | 2026-04-25 |
| 0004 | [Config-driven projects via `schema.toml`](./0004-config-driven-projects.md)            | accepted | 2026-04-25 |
| 0005 | [bge-m3 via fastembed for text embeddings](./0005-fastembed-bge-m3.md)                  | accepted | 2026-04-25 |
| 0006 | [LanceDB embedded vector store](./0006-lancedb-vector-store.md)                          | accepted | 2026-04-25 |
| 0007 | [Delta-sync at startup (no in-process index)](./0007-delta-sync-startup.md)              | accepted | 2026-04-25 |
| 0008 | [Per-project cache isolation in `~/.cache/schema/`](./0008-cache-isolation-by-project.md) | accepted | 2026-04-25 |
| 0009 | [Generic MCP tools (no domain-specific behaviour)](./0009-tools-design-generic.md)       | accepted | 2026-04-25 |
| 0010 | [Filesystem watcher uses `kqueue` on macOS](./0010-watcher-kqueue-not-fsevents.md)        | accepted | 2026-04-25 |
