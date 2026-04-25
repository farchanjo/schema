---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0003 — Multi-project architecture (one binary, many consumers)

> **Y-statement** — In the context of `schema` originating as a tool to
> serve `lowcow-platform`'s ADRs and threat models but being likely to
> serve **`lowcow-site`** and future projects (each with their own
> domain vocabulary, ADR conventions, and corpus paths), facing the
> design choice between (a) **hard-coding the Lowcow-specific paths
> and rules into the binary**, (b) **shipping per-consumer forks**,
> or (c) **a config-driven model where each project ships a
> `schema.toml` that the binary reads at startup**, we decided for
> **option C — one `schema` binary, every consumer drops a
> `schema.toml` in its repo root naming its corpus paths and kinds**,
> against forks (maintenance disaster) or hard-coding (defeats reuse)
> to achieve a single tool reusable across the operator's project
> portfolio with zero-knowledge of any specific domain inside the
> binary, accepting that consumer projects must author and maintain
> their own `schema.toml`, and that swapping projects requires
> spawning a different daemon instance (one per project / Claude
> Code session — see ADR-0008 cache isolation).

## Context and Problem Statement

`schema` was conceived during a Lowcow architecture conversation: the
operator wanted Claude Code to know about `lowcow-platform`'s ADRs.
The naive design has the binary hard-code:

- `docs/decisions/*.md` as the ADR location.
- Glossary at `docs/glossary.md`.
- Lowcow-specific kinds (`adr-madr`, `cue`, …).

The operator runs multiple repos. `lowcow-site` (the implementation
side of Lowcow) has different docs paths. Future projects will have
different conventions altogether (some use Diátaxis, some use Notion-
exported markdown, some have OpenAPI specs at `api/`).

Three response shapes:

1. Hard-code Lowcow's paths into the binary.
2. Fork the binary per consumer.
3. Read a per-project config file at startup.

## Decision Drivers

- **Reuse across projects.** The operator runs ≥ 2 repos today and
  expects to add more.
- **Schema's purpose is generic.** RAG over ADRs/contracts is a
  *pattern*, not a *Lowcow feature*. Embedding the pattern at the
  tool level decouples it from any specific domain.
- **Domain knowledge belongs to the consumer.** The consumer knows
  which directories hold ADRs vs API specs vs glossary. The tool does
  not need to learn this.
- **Single source of truth for tool installation.** `cargo install
  --path .` once; drop a `schema.toml` per repo. Adding a new project
  is an O(1) operation.

## Considered Options

### Option A — hard-coded paths (rejected)

```rust
const ADR_PATH: &str = "docs/decisions";
const GLOSSARY: &str = "docs/glossary.md";
```

Implication: every new consumer requires a binary edit + recompile +
release. Coupling between the tool and one project's filesystem
shape. Scales as O(N) consumers × O(M) per-consumer changes.

### Option B — per-consumer forks (rejected)

`mcp-schema-lowcow`, `mcp-schema-other`, etc.

Implication: every shared improvement (a bugfix, a new tool) must be
ported across forks. Maintenance disaster within 6 months.

### Option C — config-driven with `schema.toml` (chosen)

The binary knows nothing about Lowcow. Every consumer drops a
`schema.toml` at the repo root:

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
```

The binary reads this at `serve` startup, walks the declared corpus
entries, embeds, and indexes. ADR-0004 details the schema.

Implication: every consumer authors and maintains its own
`schema.toml`. The tool stays domain-agnostic.

## Decision Outcome

Chosen option: **C — config-driven multi-project architecture**.

### Project identity

Each consumer is identified by `(project.name, blake3-hash(absolute
canonical path))`. ADR-0008 details cache isolation per identity.

### One process per project

Each Claude Code session spawns one `schema` instance, scoped to one
project (the project whose `schema.toml` the session's `.mcp.json`
points at). Switching to another project = different session, different
spawn. The binary never multiplexes projects within a single process.

### What the binary does NOT know

- Naming conventions (whether the project uses MADR, ADRs-tools,
  or some custom format).
- Domain vocabulary (no hard-coded "ADR", "Igris", "Lowcow", or any
  business term in the binary).
- Tool consumers (the binary doesn't know it's talking to Claude Code
  vs Cursor vs Continue — it speaks MCP, period).

## Consequences

- **Good:** one binary serves any number of consumer projects;
  installing in a new project is `cp schema.toml.example schema.toml
  && edit`.
- **Good:** the binary's source has no domain-specific identifiers,
  making future open-sourcing trivial.
- **Good:** consumer projects own their own `schema.toml` lifecycle —
  edits don't ripple to the binary.
- **Bad:** consumers must author `schema.toml`. A future "schema init"
  command (FASE 1.1) can scaffold one from project structure.
- **Bad:** no global view of "all projects schema knows about" — each
  spawn is project-scoped. A future "schema gc" command (FASE 1.1)
  reasons globally over `~/.cache/schema/projects/`.

## Fitness function

- `src/config/schema_toml.rs` validates every `schema.toml`. CI in
  consumer projects (e.g. `lowcow-platform`'s `pnpm validate`) calls
  `schema validate --config schema.toml` to gate the file's shape.
- Search the binary's source for "lowcow", "adr-madr", or any other
  domain term: only `schema.toml` parsing recognises these strings;
  no business logic depends on them.

## More information

- ADR-0004 — `schema.toml` shape.
- ADR-0008 — per-project cache isolation.
- `examples/schema.toml` (FASE 1.1) — sample for new consumers.
