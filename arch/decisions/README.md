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
| 0002 | [rmcp 1.5 over stdio for MCP transport](./0002-rmcp-stdio.md)                           | superseded by 0019 | 2026-04-25 |
| 0003 | [Multi-project architecture (one binary, many consumers)](./0003-multi-project-architecture.md) | accepted | 2026-04-25 |
| 0004 | [Config-driven projects via `schema.toml`](./0004-config-driven-projects.md)            | accepted | 2026-04-25 |
| 0005 | [bge-m3 via fastembed for text embeddings](./0005-fastembed-bge-m3.md)                  | accepted | 2026-04-25 |
| 0006 | [LanceDB embedded vector store](./0006-lancedb-vector-store.md)                          | superseded by 0011 | 2026-04-25 |
| 0007 | [Delta-sync at startup (no in-process index)](./0007-delta-sync-startup.md)              | accepted | 2026-04-25 |
| 0008 | [Per-project cache isolation in `~/.cache/schema/`](./0008-cache-isolation-by-project.md) | accepted | 2026-04-25 |
| 0009 | [Generic MCP tools (no domain-specific behaviour)](./0009-tools-design-generic.md)       | accepted | 2026-04-25 |
| 0010 | [Filesystem watcher uses `kqueue` on macOS](./0010-watcher-kqueue-not-fsevents.md)        | accepted | 2026-04-25 |
| 0011 | [SQLite + `sqlite-vec` embedded store](./0011-sqlite-vec-store.md)                       | accepted | 2026-04-25 |
| 0012 | [Strict lint baseline (Layer A `forbid` + Layer B activation)](./0012-strict-lint-baseline.md) | accepted | 2026-04-25 |
| 0013 | [Hexagonal architecture (ports & adapters)](./0013-hexagonal-architecture.md)            | accepted | 2026-04-25 |
| 0014 | [Install at `/usr/local/bin` + Apple codesign on macOS](./0014-install-and-codesign.md)  | accepted | 2026-04-25 |
| 0015 | [Cleanup tools at both MCP and CLI surfaces](./0015-cleanup-tools.md)                     | accepted | 2026-04-25 |
| 0016 | [MCP tool description style: verb + example + safety hint](./0016-mcp-tool-description-style.md) | accepted | 2026-04-25 |
| 0017 | [`mtime + size` short-circuit before `blake3` hash](./0017-mtime-size-shortcircuit.md) | accepted | 2026-04-26 |
| 0018 | [Embedder CPU cap via process nice](./0018-cap-onnx-threads.md) | accepted | 2026-04-26 |
| 0019 | [MCP transport: Streamable HTTP via `rmcp` 1.5 + `axum` 0.8](./0019-http-streamable-transport.md) | accepted (process-shape part amended by 0026) | 2026-04-26 |
| 0020 | [Permanent service lifecycle: launchd + systemd, one per project](./0020-service-permanent-lifecycle.md) | accepted (per-project shape amended by 0026) | 2026-04-26 |
| 0021 | [Localhost bind + per-project bearer-token auth](./0021-localhost-bearer-auth.md) | accepted (validator shape amended by 0026) | 2026-04-26 |
| 0022 | [Bench-driven evaluation of `tokio-uring` (D-3, Linux only)](./0022-tokio-uring-bench-evaluation.md) | accepted (gate) | 2026-04-26 |
| 0023 | [Config resolution: walk-up + ENV overlay](./0023-env-overlay-and-walk-up-config-resolution.md) | accepted | 2026-04-26 |
| 0024 | [E2E test category in Python (pytest + httpx)](./0024-e2e-tests-python-pytest.md) | accepted | 2026-04-26 |
| 0025 | [LLM `synthesize` tool + `LlmProvider` port (Anthropic / OpenAI)](./0025-llm-synthesize-tool-and-provider-port.md) | accepted | 2026-04-26 |
| 0026 | [One shared daemon for all projects, with strict per-project isolation](./0026-shared-multi-project-daemon-with-strict-isolation.md) | accepted (drivers preserved; routing/membership specifics partially superseded by 0027) | 2026-04-26 |
| 0027 | [LLM-driven project discovery: directory-as-source-of-truth](./0027-llm-driven-project-discovery.md) | proposed (refactor PRs gated on operator confirmation + canary fitness) | 2026-04-26 |
