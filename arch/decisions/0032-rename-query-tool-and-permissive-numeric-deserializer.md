---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0009", "ADR-0016"]
---

# 0032 — Rename MCP tool `query` → `search` + permissive `Option<usize>` deserializer

> **Y-statement** — In the context of the schema MCP server (ADR-0019 +
> ADR-0027) exposing a generic semantic-search tool named `query` with
> an `Option<usize>` `top_k` override (`#[serde(default)]`,
> [`crate::adapters::mcp_server::QueryParams`]) and three sibling
> retrieval tools (`find_decisions`, `glossary_lookup`, `synthesize`)
> with byte-identical `top_k` shape, observed live during a 2026-04-27
> dogfood smoke that **only `query` rejects an integer `top_k`** with
> `MCP error -32602: failed to deserialize parameters: invalid type:
> string "3", expected usize` — confirmed by direct `curl` against the
> daemon (integer accepted, string rejected) and by side-by-side
> `mcp__schema__find_decisions(top_k=3)` + `mcp__schema__query(top_k=3)`
> reproductions where the **same Claude Code MCP client** wires
> `top_k: 3` (integer) for `find_decisions` and `glossary_lookup` and
> `top_k: "3"` (string) for `query`, isolating the defect to the
> client's per-tool argument-coercion path that special-cases the
> name `query` (likely legacy MCP-protocol shadowing of an older
> top-level `query` argument), and the `query` field that lives
> inside three different `*Params` structs (which makes the tool name
> `query` semantically noisy already — "query the query tool with a
> query"), facing the choice between (a) **status quo** (default
> `top_k=8` works without override, but any integer override fails;
> operators see the failure as a server bug and lose trust),
> (b) **deserializer-only fix** (custom `deserialize_with` that
> accepts integer-or-string for every `Option<usize>`; mantains tool
> name and ABI; works around any client that fat-fingers numeric
> coercion, not only Claude Code on `query`), (c) **rename-only fix**
> (`query` → `search`; closes the Claude Code coercion path because
> the client routes by name; keeps `Option<usize>` strict; consumer
> trees that wired the old name break), or (d) **rename + permissive
> deserializer + 30-day deprecated alias** (rename canonical to
> `search`; keep `query` registered as a deprecated alias forwarding
> to the same handler with a one-shot `WARN` per process; apply the
> permissive deserializer to all `Option<usize>` fields so future
> clients with the same coercion class are absorbed; cutover the
> alias on 2026-05-27 — same window as ADR-0031), we decided for
> **(d)**, against (a) (the failure compounds as more operators
> wire the tool; trust loss is an arch-quality cost), (b) (does
> not address the `query`-the-tool-name colliding with `query`-the-
> argument; the ubiquitous-language smell remains), and (c) (loses
> the defense-in-depth benefit and breaks every existing wiring on
> the cut), to achieve **a tool surface where the canonical
> retrieval verb (`search`) is unambiguous in the ubiquitous
> language and the integer-override happy path works on every
> compliant MCP client**, accepting **(1) one canonical tool name
> change (`query` → `search`) — breaking change to consumer-side
> tool dispatch, telegraphed via a 30-day deprecated alias and a
> startup `WARN` on every alias invocation, (2) one new private
> deserializer (`deserialize_optional_usize`) attached to every
> `Option<usize>` field on every `*Params` struct that originates
> from a tool call (`top_k` × 4 + `definition_limit` +
> `reference_limit`), and (3) the documentation cost of updating
> ADR-0009 (generic-tools rule) and ADR-0016 (tool-description
> style) with `Evidence and amendments` entries pointing here.**

## Context and Problem Statement

A 2026-04-27 dogfood session (`schema.toml` indexing the tool's own
`arch/`) produced this minimal reproduction:

```text
> mcp__schema__find_decisions(working_directory=…, query="test1", top_k=3)
  → 200 OK, 3 chunks ranked by score

> mcp__schema__query(working_directory=…, query="test2", top_k=3)
  → MCP error -32602: failed to deserialize parameters:
    invalid type: string "3", expected usize
```

Direct `curl` against the daemon proved the server is not at fault:

```text
> curl … -d '{"…","arguments":{…,"top_k":3}}'        → 200 OK
> curl … -d '{"…","arguments":{…,"top_k":"3"}}'      → -32602
```

`tools/list` JSON-Schema export shows `top_k` declared identically
across `query`, `find_decisions`, `glossary_lookup`, `synthesize`:

```json
{"default": null, "format": "uint", "minimum": 0, "type": ["integer","null"]}
```

Same wire schema, same client, same session, three tools accept the
integer, one (`query`) rejects it because the **client** ships a
string. The defect lives in Claude Code's per-tool argument
coercion path — the schema server cannot fix it.

The collision goes deeper than the bug. The tool is named `query`
and its primary argument inside the `*Params` struct is also named
`query`:

```rust
pub struct QueryParams {
    pub working_directory: String,
    pub query: String,            // ← same identifier as the tool
    pub top_k: Option<usize>,
}
```

Two callers in the same paragraph cannot tell whether `query`
refers to the tool or the argument. ADR-0009 (generic MCP tools)
specifies that tool surfaces must be unambiguous; `query` does not
clear that bar.

## Decision drivers

- **Restore the `top_k` happy path on every compliant client.** The
  permissive deserializer is the universal fix — any client that
  produces `"3"` instead of `3` survives the round-trip. We cover
  the future class of bugs, not just this one.
- **Resolve the ubiquitous-language collision.** Rename the tool
  to `search`, free the word `query` for use only as the argument
  name. ADR-0009's intent — "tool name describes the verb" —
  becomes literal.
- **Telegraph the breaking change with a soft window.** Same
  30-day deprecated-alias pattern ADR-0031 used for env-source
  secrets. Operators get a `WARN` log; tool wiring keeps working
  until 2026-05-27.
- **Keep the schema simple.** No special-cased deserialization for
  `query` only — the same helper applies to every `Option<usize>`
  field. `definition_limit` and `reference_limit` (in
  `CrossReferenceParams`) get the same treatment defensively, even
  though no client has tripped them yet.

## Considered options

### Option (a) — Status quo
- **Rejected.** The integer-override path is now visibly broken on
  the canonical retrieval tool. Operators who try `top_k=3` see a
  server-style error envelope and reach for "is the server
  broken?" — wrong answer, expensive triage.

### Option (b) — Deserializer-only fix
- Closes the immediate failure on every client.
- Universal: works for any future client with the same coercion
  class, on any `Option<usize>` field.
- Does not address the `query`-the-tool vs `query`-the-arg
  collision. Ubiquitous-language smell remains.
- **Held**: this is half the answer. We adopt it as part of
  option (d).

### Option (c) — Rename-only fix
- Closes the failure on Claude Code (which appears to special-case
  the literal name `query`).
- Hard breaking change. Every consumer-side wiring that currently
  calls `query` breaks at cutover.
- Does not protect against the next bug in the same class on a
  different tool.
- **Rejected** — narrow fix, full break.

### Option (d) — Rename + permissive deserializer + 30-day alias
- Rename canonical: `query` → `search`. ADR-0009's verb-as-name
  contract becomes literal.
- Register `query` as a **deprecated alias** routing to the same
  handler. Emit a one-shot `WARN secrets-style` log per process
  on first alias invocation pointing operators to the new name
  and the 2026-05-27 cutover.
- Apply `deserialize_optional_usize` to every `Option<usize>`
  field originating from a tool call. Accepts integer or
  numeric string; rejects everything else with the same
  `serde::de::Error` shape.
- Defense in depth: the rename closes the Claude-Code-specific
  hole; the deserializer closes the wider class.
- **Selected.**

## Decision

We adopt **option (d)**.

1. **Add the canonical tool `search`** with the same `*Params` and
   handler logic as today's `query`. Tool description: "Semantic
   search across the project corpus resolved from
   working_directory. Replaces the deprecated `query` tool — same
   shape, clearer name. Use for general questions when you don't
   know the kind, or when you want hits across ADRs, glossary,
   and prose at once."

2. **Keep `query` registered as a deprecated alias** forwarding
   one-to-one to the `search` handler. Description prefix:
   "**Deprecated**: use `search`. Retained until 2026-05-27 for
   wiring compatibility (ADR-0032)." On first invocation per
   process, emit `tracing::warn!` once via `std::sync::Once` —
   matches ADR-0031's env-source `WARN` pattern.

3. **Add `deserialize_optional_usize`** to a shared module
   (`crate::adapters::mcp_server::serde_helpers`). Accepts
   `Value::Null`, `Value::Number(integer)`, `Value::String(s)`
   with `s.parse::<usize>()`. Rejects everything else with
   `serde::de::Error::invalid_type`.

4. **Apply the helper** to every `Option<usize>` on a `*Params`
   struct: `QueryParams.top_k`, `FindDecisionsParams.top_k`,
   `GlossaryLookupParams.top_k`, `SynthesizeParams.top_k`,
   `CrossReferenceParams.definition_limit`,
   `CrossReferenceParams.reference_limit`. Existing
   `#[serde(default)]` is kept; the new attribute is
   `#[serde(default, deserialize_with = "deserialize_optional_usize")]`.

5. **Cutover 2026-05-27.** On the first daemon build released
   after that date, the `query` alias is removed from the tool
   registry. Operators who still wire `query` after the cutover
   see `MCP error: tool not found`. Document the cutover in an
   `Evidence and amendments` entry.

6. **Update ADR-0009 and ADR-0016** with `Evidence and
   amendments` entries pointing here. ADR-0009's "tool name
   describes the verb" intent gets the literal `search` rename
   as its concrete witness.

## Consequences

- **Good:** integer-override path works on every client across
  every retrieval tool — universal fix to the deserialization
  class.
- **Good:** ubiquitous-language collision (`query` tool vs `query`
  argument) is gone after cutover.
- **Good:** the soft alias window mirrors ADR-0031's env-source
  deprecation, so operators see the same shape of breaking
  change twice and learn to expect the 30-day pattern.
- **Neutral:** two registrations for the same handler during the
  window. Trivial cost.
- **Bad:** consumer-side wiring that calls `query` breaks at
  cutover unless updated. Mitigated by the `WARN` log and the
  30-day window.
- **Bad:** the new `deserialize_with` attribute is on six fields.
  Adding a seventh field that should accept the same input
  requires remembering the attribute. Mitigated by a search-and-
  match doc lint at the bottom of the helper module that lists
  every consumer.

## Fitness function

- **Unit test (deserializer happy paths):** integer `3` → `Some(3)`;
  string `"3"` → `Some(3)`; `null` / absent → `None`. Lives next
  to the helper.
- **Unit test (deserializer rejection paths):** boolean `true`,
  array `[3]`, object `{}`, non-numeric string `"abc"`,
  negative-integer-as-string `"-1"`, integer overflow on
  `usize::MAX + 1` — all return `Err(serde::de::Error)` with the
  expected `invalid_type` / `invalid_value` shape.
- **Integration test (alias warns once):** spawn daemon → call
  `query` twice → assert exactly one `WARN secrets-style` log
  line on stderr containing `deprecated: use search`. Lives in
  the existing `tests/http_auth.rs` style.
- **Live MCP smoke (post-impl):** `mcp__schema__search(top_k=3)`
  succeeds; `mcp__schema__query(top_k=3)` succeeds (alias) and
  emits the WARN; `mcp__schema__synthesize(top_k="3")` succeeds
  (permissive deserializer); validation gate green.
- **Doc lint (cutover marker):** `arch/operations/runbook.md`
  carries a "Tool name cutover 2026-05-27" subsection pointing
  at this ADR.

## Cross-references and follow-ups

- **ADR-0009 — generic tools.** This ADR's `search` rename is a
  literal application of ADR-0009's "tool name describes the
  verb" rule. ADR-0009 gets an `Evidence and amendments` entry
  on accept.
- **ADR-0016 — tool description style.** The `query` deprecated
  description prefix (`**Deprecated**: use \`search\`. …`) is a
  small but new pattern under ADR-0016's "verb + example +
  safety hint". ADR-0016 gets an `Evidence and amendments`
  entry codifying the deprecation prefix.
- **ADR-0031 — secret management.** Same 30-day soft-deprecation
  window pattern. Operators see two cutovers on 2026-05-27 (env
  secrets and `query` alias) — the runbook should call them
  out together.
- **Follow-up (held):** report the Claude Code MCP client
  coercion bug upstream so future tool authors do not need
  this workaround.
- **Follow-up (held):** if a second tool name in this codebase
  triggers the same client coercion (none observed yet),
  capture the pattern in this ADR's Evidence as a recurring
  reserved-word list.

## Evidence and amendments

(none yet — this ADR is `accepted` and the implementation slice
follows in this same change set)
