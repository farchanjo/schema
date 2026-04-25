---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0009 — Generic MCP tools (no domain-specific behaviour)

> **Y-statement** — In the context of ADR-0003 making `schema`
> multi-project, facing the question of **what MCP tools to expose
> in FASE 1.0**, given the trade-off between Lowcow-specific tools
> (`find_threats`, `find_business_rule` — strong UX for one
> consumer) and **generic tools** (`query`, `find_decisions`,
> `glossary_lookup`, `cross_reference`, `list_corpus` — usable by
> any consumer that ships matching kinds in `schema.toml`), we
> decided for **five generic tools, no domain identifiers in the
> binary** — every domain coupling lives in the consumer's
> `schema.toml` corpus declarations and ADR-0008 cache isolation —
> against domain-specific tools (would lock the binary to one
> consumer; ADR-0003 explicitly disallows), to achieve a tool
> surface that maps cleanly to the way LLMs reason about
> documentation (general semantic search + ADR-targeted search +
> term lookup + relationship navigation + corpus introspection),
> accepting that some consumers may want sharper-typed tools (e.g.
> a Lowcow-specific `find_threats(area: "iam")`) and that those
> remain a future amendment via ADR-aligned per-consumer plug-ins
> (out of scope for FASE 1.0).

## Context and Problem Statement

MCP tools are how Claude Code (and any MCP client) interacts with
schema. Each tool has a name + JSON schema + handler. Three
considerations frame the choice:

1. **LLM ergonomics.** Sonnet picks tools by name + description.
   Generic tools with clear descriptions are easy to choose; a
   long catalogue of fine-grained tools is harder to dispatch.
2. **Domain-agnosticism (ADR-0003).** No domain identifier in the
   binary. Tool names cannot reference "Lowcow", "ADR" specifics
   (we can say "decision", which is a generic term that ADRs
   instantiate), or "threats" (Lowcow-specific concept).
3. **FASE 1.0 surface area.** Each tool is real code + tests +
   docs. More tools = more maintenance.

## Decision Drivers

- **Map to retrieval primitives.** RAG over docs has a small
  number of natural shapes: semantic search, typed search, lookup
  by id, relationship navigation, introspection. Five tools cover
  these.
- **Composable from Claude's side.** Sonnet can chain tool calls.
  Coarser tools + good descriptions beat finer tools + cluttered
  surface.
- **Hold the line on generality.** Once we ship a Lowcow-specific
  tool, every future consumer asks "where's mine?" and the
  binary's domain neutrality erodes.

## Considered Options

### Option A — domain-specific tools (rejected)

`find_threats(area)`, `find_iam_decisions()`, etc.

Implication: binary becomes Lowcow-flavoured. Future consumers
either accept Lowcow-shaped tools or fork. Violates ADR-0003.

### Option B — five generic tools (chosen)

| Tool                | What                                                           |
| ------------------- | -------------------------------------------------------------- |
| `query`             | Top-K semantic search over the entire indexed corpus.          |
| `find_decisions`    | Top-K semantic search restricted to `kind = adr-madr`.         |
| `glossary_lookup`   | Top-K semantic search restricted to `kind = glossary`.         |
| `cross_reference`   | Given an `artifact_id` (e.g. "ADR-0055"), return defining + referencing chunks. |
| `list_corpus`       | Debug: list every distinct `source_path` in the index.         |

Plus `ping` for smoke testing (FASE 1.0 keepable; can be removed
in 1.1).

Trade-offs: kind names like `adr-madr`, `glossary` are shared
vocabulary the consumer must use in `schema.toml`. The binary
treats them as opaque enum values; the *meaning* is owned by
the consumer.

### Option C — fewer tools, parameterised (rejected)

Just `query(kind?, artifact_id?)`. Argues that one tool with
optional parameters covers everything.

Implication: tool descriptions become long; LLM dispatch quality
drops because the tool's shape doesn't telegraph its intent.
Modern LLMs prefer named-intent tools over universal tools with
flags.

## Decision Outcome

Chosen option: **B — five generic tools**.

### Tool semantics

- All take a `top_k` override (default from `schema.toml` retrieval
  config).
- All return JSON-encoded `String` (no MCP-level error envelopes
  for FASE 1.0; errors encoded inline as `{"error":"..."}`).
- Embedding the query happens once per call inside the tool; the
  embedder is shared via `Mutex<Embedder>` (ADR-0005).

### Why no `summarize` / `narrate` tool

Such a tool would invoke an LLM **inside** the MCP server to
summarise chunks. That makes schema a stateful LLM dispatcher
rather than a retrieval engine. Claude Code already does that
work much better. Decision: tools return chunks, Claude Code
synthesises.

### Why `list_corpus` exists

Debugging trust. Operators (and AI agents) need to verify "did
schema actually see this file?" without inspecting LanceDB by
hand. The tool returns a flat `Vec<String>` of source paths.

## Consequences

- **Good:** five tools fit comfortably in any LLM's tool-pick
  reasoning.
- **Good:** kind-typed tools (`find_decisions`, `glossary_lookup`)
  hint at intent without coupling to a specific project.
- **Good:** `cross_reference` enables impact-analysis workflows
  ("if I change ADR-0055, what else mentions it?") without any
  domain code.
- **Bad:** consumers wanting sharper typing (e.g. `find_threats`)
  do not get it from the binary. A future plug-in mechanism (FASE
  2) could close this gap; deferred.
- **Bad:** `list_corpus` returns up to thousands of paths for big
  projects. JSON payload is bounded but not paginated. FASE 1.1
  may add pagination if it surfaces as a problem.

## Fitness function

- Every tool has an `#[tool(description = "...")]` attribute. The
  description is the LLM's only hint at the tool's purpose; rmcp
  emits the JSON Schema automatically. CI gate confirms the tool
  catalogue at build time.
- `cargo test --lib` exercises tool wiring at the unit level
  (FASE 1.1 will add integration tests that call the tools via
  the MCP transport).
- An integration smoke (FASE 1.1) calls each tool with a known
  fixture and asserts the response shape.

## More information

- `src/mcp/server.rs` — tool implementations.
- ADR-0002 — rmcp transport (the tool dispatch layer).
- ADR-0003 — multi-project architecture (the constraint that
  forced generic tools).
- ADR-0006 — LanceDB store ops powering the tools.
