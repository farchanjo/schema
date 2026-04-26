---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0016 — MCP tool description style: action verb + concrete example + safety hint

> **Y-statement** — In the context of giving the LLM client enough
> information to confidently choose **and** parametrise each MCP
> tool exposed by `schema` without trial-and-error retries or
> wrong-tool selection, facing the choice between (a) **terse
> one-liners** (e.g. `"Semantic search in ADRs."` — minimum tokens,
> highest ambiguity), (b) **JSON-Schema-only descriptions** (rely
> entirely on field types + struct names), or (c) **action-oriented
> descriptions with at least one concrete example baked in plus an
> explicit differentiation hint vs neighbouring tools** (the
> Anthropic guidance pattern), we decided for **(c)**, against (a)
> (LLMs guess between similar tools and waste a tool call on retry;
> the saved description tokens are spent on the wrong-call recovery
> loop) and (b) (params shapes the LLM does not predict cleanly —
> e.g. relative-vs-absolute paths, artifact id format, k bounds —
> need a concrete value example, not just a type), to achieve
> first-call-correct rate close to 1 even on tools with similar
> names (`query` vs `find_decisions` vs `glossary_lookup`) and on
> destructive operations that need operator confirmation
> (`reset_index`, `forget_source`), accepting that descriptions are
> ~3× longer than the terse alternative (~286 tokens total for the
> 8 tools today vs ~60 for the maximally compressed version), and
> that adding a new tool now carries a checklist obligation.

## Context and Problem Statement

`schema` ships 8 MCP tools today (after ADR-0015):

```
ping  query  find_decisions  glossary_lookup  cross_reference
list_corpus  reset_index  forget_source
```

Three of them (`query`, `find_decisions`, `glossary_lookup`) are
**semantic-search siblings** — same shape, different `kind` filter.
Two (`reset_index`, `forget_source`) are **destructive** and must
not fire without operator confirmation. One (`cross_reference`)
takes a non-obvious **artifact id** that the LLM has to format
correctly.

The first cut of descriptions (FASE 1.0) was modelled on inline
rustdoc and read like internal API docs ("Top-K semantic search
restricted to architectural decisions (ADRs). Use when you
specifically want decisions and their rationale, not general
docs."). After the cleanup-tools sprint we tried a maximally
compressed version (~60 tokens total, "Semantic search in ADRs
only.") and reverted: the LLM can ambiguate between siblings, it
struggles to predict the artifact-id format without an example,
and the safety hint on destructive ops dilutes when the description
is too short.

The right answer is the middle: **action-verb opening + a concrete
example or value pattern + an explicit differentiation hint vs
neighbour tools when ambiguity is plausible**. Total cost is ~286
tokens for 8 tools (~36 per tool on average) — measured against
a single failed tool-call retry loop (~50-200 tokens of LLM
back-and-forth), the descriptions pay for themselves on every
hesitation they prevent.

## Decision Drivers

- **First-call-correct rate.** The dominant cost is wrong-tool or
  wrong-shape calls, not description tokens.
- **LLM affordance, not human readability.** The descriptions
  should be parsed by an LLM choosing between tools — not
  literary prose. Format: verb-led + concrete + parallel structure.
- **Safety prefix for destructive operations.** `DESTRUCTIVE —`
  in caps at the start triggers most LLMs' built-in
  ask-the-operator-first reflex.
- **Param descriptions carry shape examples.** When a value's
  format is non-obvious (relative path, artifact id pattern,
  kind enum), the param's `///` doc shows a concrete example.
- **Pattern enforced by template, not by hope.** A new tool added
  later must follow the same shape, audited at PR review.

## Considered Options

### Option A — Terse one-liner (rejected)

Example: `"Semantic search in ADRs."`. Pros: minimum tokens.
Cons: ambiguates against `query` and `glossary_lookup`; LLM
must guess parameter shapes. Wrong-call retry cost (~50-200
tokens) exceeds the savings.

### Option B — JSON Schema only (rejected)

Drop the description text entirely; rely on field names + types
+ defaults. Pros: cleanest. Cons: schema cannot encode "use this
when X" or "do not use when Y" or "format: 'ADR-NNNN'". LLM
falls back to picking by name match alone.

### Option C — Action-verb + concrete example + safety hint (chosen)

Each tool description follows this template:

```
<verb-led one-line summary of what it does>.
<one-line differentiation hint vs neighbour tools, when ambiguity
is plausible>.
Example queries / Example: <concrete one-shot value or query>.
[For destructive: DESTRUCTIVE — prefix; impact + confirm + scope.]
```

Param descriptions:

```
<purpose, one short clause>, e.g. <concrete value>.
[Optional: behavioural hint (synonyms work / Higher = breadth / etc.)]
```

## Decision Outcome

Chosen option: **C**. The 8 current tool descriptions are
rewritten to this style as part of this ADR's implementation; see
the Evidence section for the verbatim text.

### Template — for any future tool added

1. **Tool description literal** (`#[tool(description = "...")]`):
   - Open with a verb-led summary in plain English. No "Top-K", no
     "the project's ...", no rustdoc style.
   - If two tools could plausibly be confused, add one short
     clause naming the differentiation ("Use for X when Y" or
     "Use after Z").
   - Include at least one concrete example: example queries (in
     quotes) for search tools, example identifier for id-shaped
     tools, example path for path-shaped tools.
   - For destructive operations, prefix with `DESTRUCTIVE —` in
     caps and include both **impact** (what gets deleted) and
     **confirmation expectation** ("ask the operator first" or
     equivalent) and **scope clarification** (what is *not*
     touched, when relevant).

2. **Param `///` doc comments**:
   - One short clause stating purpose.
   - Concrete example value when the format is not predictable
     from the field name (paths, ids, kind strings).
   - For numeric overrides (k, limits): a short hint about
     trade-off direction ("higher = more breadth", etc.) when
     useful.

3. **Struct-level `///` doc comments** above each `Params` struct:
   may stay as a one-liner for navigability; not exposed to the
   LLM as prominently as the field docs but still part of the
   schema.

### Anti-patterns (do not regress to)

- "Top-K X" prefixes — the schema return type already exposes the
  list shape; "Top-K" just means "more than one".
- "Useful for X" / "Use when you want X" prose tail — replace
  with concrete examples or a differentiation clause.
- Implementation detail in tool descriptions — "VACUUM",
  "manifest", "bge-m3 1024-dim" do not help the LLM choose; they
  belong in the runbook or in `tracing::info!` logs the operator
  reads.
- Descriptions that describe the *return shape* — that is the
  JSON Schema's job.

## Consequences

- **Good:** First-call-correct rate goes up; ambiguity between
  semantic-search siblings is resolved by the differentiation
  clause; param shapes are obvious from examples.
- **Good:** Destructive operations carry an explicit safety
  signal at the top of the description; LLMs trained on
  Anthropic's tool-use guidance handle this well.
- **Good:** New-tool checklist makes the style enforceable at
  PR review without inventing it each time.
- **Bad:** ~286 tokens for 8 tools today (~36 per tool) vs ~60
  for the maximally compressed alternative. Acceptable: each
  prevented retry recovers >50 tokens.
- **Neutral:** Descriptions get refreshed as tools are added;
  parallel structure should hold.

## Fitness function

- **Structural grep:** every `#[tool(description = "...")]`
  literal in `src/adapters/mcp_server.rs` is at least 60
  characters AND contains either an `'Example'` substring, an
  example query in quotes, or the `DESTRUCTIVE —` prefix.
  ```bash
  grep -A1 '#\[tool(description' src/adapters/mcp_server.rs
  ```
  PR review confirms by eye.
- **Param-doc check:** every non-trivial param (`String` or
  `Option<String>` shaped) has either an `e.g.` example or a
  behavioural hint in its `///` doc comment.
- **Destructive-prefix check:** every tool whose name implies
  destruction (`reset*`, `forget*`, `delete*`, `drop*`, `wipe*`)
  starts its description with `DESTRUCTIVE —`.

These are review-time checks; we do not gate CI on them today
because the rules involve human judgement on what counts as
"non-obvious format" or "plausible ambiguity". A linter that
encodes them is a follow-up.

## More information

- ADR-0009 — generic MCP tools (the parent ADR for the tool
  surface; this ADR refines the description-writing style).
- ADR-0015 — cleanup tools (`reset_index`, `forget_source`); the
  destructive prefix in those descriptions is mandated by this
  ADR.
- `src/adapters/mcp_server.rs` — the live descriptions.

## Follow-ups

- **Lint / structural check.** A `cargo xtask` or a CI script
  that asserts the structural-grep + param-doc + destructive-
  prefix checks above. Skipped today; revisit when the tool
  count > 12.
- **i18n.** Tool descriptions are currently English-only. If the
  consumer base ever needs PT-BR descriptions, an ADR for that
  is required (translating descriptions changes the LLM's
  decision surface).

## Evidence and amendments

- _2026-04-25 — Initial recording. The 8 current tool
  descriptions were rewritten in the same session that accepted
  this ADR; verbatim text below as the proof of the style being
  live._

### The 8 descriptions as of acceptance (verbatim)

```
ping
  "Liveness probe; returns 'pong'. Use as a connectivity smoke test."

query
  "Semantic search across the whole project corpus. Use for general
   questions when you don't know the kind, or when you want hits across
   ADRs, glossary, and prose at once.
   Example queries: 'how does session lifetime work',
                    'what is the chunking strategy for markdown'."

find_decisions
  "Semantic search restricted to architectural decisions (ADRs). Use
   to retrieve a decision and its rationale. Returns the ADR body plus
   surrounding context.
   Example queries: 'why did we pick LanceDB',
                    'what is our session lifetime policy'."

glossary_lookup
  "Look up a term in the project glossary. Match is semantic, so
   synonyms and related phrases surface — you don't need the exact
   word the glossary uses.
   Examples: 'RBAC' → 'role-based access control'.
             'JWT'  → 'JSON Web Token'."

cross_reference
  "Given an artifact id (e.g. 'ADR-0055'), return its defining chunks
   PLUS every chunk elsewhere in the corpus that references it.
   Useful for impact analysis: 'what depends on this decision'.
   Example: artifact_id='ADR-0055' → returns the ADR's own body and
   every other chunk that mentions ADR-0055 inline."

list_corpus
  "Debug — list every source path currently in the project's index.
   Useful for verifying that schema.toml corpus paths expanded into
   the files you expected. No arguments."

reset_index
  "DESTRUCTIVE — wipe every chunk and reset the manifest for this
   project. Ask the operator to confirm before calling.
   Use after a chunking strategy change or model swap when a full
   re-index is wanted. Next `schema serve` rebuilds from scratch
   (~30-60s for a typical corpus)."

forget_source
  "DESTRUCTIVE — drop every chunk for one source path from the index.
   The file on disk is NOT deleted; only its chunks vanish from the
   index.
   Use when a doc has gone stale or noisy.
   Example: path='docs/decisions/0042-deprecated.md' removes only
   that file's chunks."
```
