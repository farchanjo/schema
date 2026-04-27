---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0009", "ADR-0013", "ADR-0019"]
---

# 0033 — Recall: session-transcript retrieval as a separate bounded context

> **Y-statement** — In the context of this repository today exposing
> the **schema** MCP server (HTTP daemon, ADR-0019, ADR-0027) for
> retrieval over **persistent project corpora** (ADRs, glossary,
> code under `schema.toml`) but lacking any retrieval surface over
> the **session transcript** that Claude Code already writes per
> session in `~/.claude/projects/<encoded>/sessions/<id>.jsonl`
> (every assistant reply, every tool result, every diff applied,
> every prompt the operator sent), and the operator pain that the
> LLM repeatedly re-reads files via the `Read` tool to recover
> context it has already produced minutes earlier in the same
> session — burning context tokens and forcing context-window
> compaction at exactly the time clarity matters most — facing the
> choice between (a) **do nothing** (status quo: LLM re-reads
> arbitrarily, context wastes inevitable), (b) **bolt new tools
> onto schema** that index the transcript alongside the corpus
> (mixes two bounded contexts in one server, blurs the
> ubiquitous-language: `Chunk` of an ADR vs `Turn` of the
> transcript become the same noun in `tools/list`), (c) **ship
> recall as its own MCP server** in a separate process with a
> separate vocabulary, separate transport (stdio for the LLM
> path, ephemeral per-session), and a separate CLI for operator
> arqueology (cross-project search lives in CLI mode only;
> ADR-0035), or (d) **fold transcript indexing into Claude
> Code itself** (out of our scope; we own the MCP side, not the
> client), we decided for **(c) — recall is its own bounded
> context** with a stdio MCP server that lives only as long as
> the Claude Code session that spawned it, against (a) (the
> token waste is real and recurs every session), (b) (mixes
> persistent-corpus and ephemeral-session vocabularies; once
> mixed, splitting later is a breaking change to every wired
> consumer), and (d) (out of our scope), to achieve **a tool
> surface that lets the LLM say `recall(query)` instead of
> `Read /path/to/file.rs`** and lets the operator say
> `recall query "X"` in a terminal to dig into past sessions,
> with a **separate process per Claude Code window** so two
> sessions on the same project never cross-talk and a
> **strict ephemeral scope for the MCP path** (no disk, no
> snapshot — when the session closes the index is gone),
> accepting **(1) one new binary `recall` ships in this
> repository's Cargo workspace (ADR-0034), (2) two consumers
> register in `.mcp.json` (`schema` + `recall`), so the LLM
> sees ~17 tools instead of ~11, (3) the recall index is
> rebuilt from scratch every session — bootstrap cost paid
> once per Claude Code window (≤ 5 s for typical sessions
> with the default MiniLM-L6 embedder), (4) the operator-side
> CLI mode has a different scope (cross-project, persistent
> cache, ADR-0035) that is intentionally not visible to the
> LLM during the session — operator-mediated only.**

## Context and Problem Statement

The operator runs Claude Code in `~/dev/mcp-schema` (or any other
project) for hours. The LLM:

1. Reads `src/cli/mcp_shim.rs` once via the `Read` tool to debug a
   failure.
2. Edits `src/adapters/mcp_server.rs`.
3. Twenty turns later, asks: "remind me what `forward_request`
   returned on success" — and the LLM fires `Read` on
   `src/cli/mcp_shim.rs` *again* to recover context it already
   produced.

The session transcript (`session.jsonl`) **already contains** the
answer. Claude Code writes one JSON-LD line per turn:

- `user_prompt` — the operator message
- `assistant` — the LLM reply text
- `tool_use` / `tool_result` — every Read, Edit, Bash, Grep,
  WebSearch, WebFetch, NotebookEdit, TaskWrite call and result

Today no MCP tool exposes this transcript to the LLM at runtime.
The LLM has no way to ask "what did I just say about X" without
re-running the work. Every re-read costs:

- LLM context tokens (the file is chunked again into the prompt)
- API latency (Anthropic call to ingest)
- Cognitive friction (the operator notices the LLM is "going
  blank")

For long debugging sessions (4 h+, 200+ turns), this is the
single largest source of token waste in the project.

The schema HTTP daemon (ADR-0019, ADR-0027) is **not** the answer.
It indexes **persistent project corpora**: ADRs, glossary,
markdown under `schema.toml`. It has nothing to say about
"what did the LLM tell me 20 turns ago". Mixing the two
vocabularies — `Chunk` of an ADR vs `Turn` of a session — into
one server makes both surfaces worse:

- The schema tool descriptions get longer ("…for both your project
  corpus and the current session transcript…").
- The `working_directory` walk-up (ADR-0027) doesn't apply to
  transcripts (they live under `~/.claude/`, not the project tree).
- Ephemeral state (transcript) and persistent state (corpus) end
  up sharing the same daemon lifetime — which is **wrong** for
  transcript (should die with the session) and **right** for
  corpus (should outlive the session).

Two bounded contexts (DDD term: a slice of the system with its
own vocabulary and rules). The right move is to keep them apart
from day one.

## Decision drivers

- **Stop token waste from re-Read.** The LLM should be able to
  ask `recall(query)` and get back chunks of the very transcript
  Claude Code has been writing.
- **Ubiquitous-language hygiene.** `Chunk`/`Embedding` are
  schema's vocabulary; `Turn`/`ToolResult`/`Diff` are recall's.
  Don't mix.
- **Lifecycle alignment with data lifetime.** The transcript is
  ephemeral — gone the moment Claude Code closes the window.
  The recall MCP server should live only as long as that
  transcript matters: per-session, per-window, in-memory only.
- **Single-process-per-window safety.** Each Claude Code window
  spawns its own recall process via stdio MCP. No cross-talk
  between two windows on the same project, two windows on
  different projects, or any combination. State stays in RAM,
  scoped to PID.
- **Separation of LLM vs operator audiences.** The LLM, during
  a session, only needs the *current* session's transcript.
  The operator, in a terminal between sessions, sometimes
  needs to dig into *past* sessions across projects — that
  use case is real but lives in CLI mode (ADR-0035), not in
  the MCP surface the LLM sees.

## Considered options

### Option (a) — Status quo
- LLM keeps re-Read'ing.
- Token waste is the cost of inaction.
- **Rejected** — the cost compounds; every session pays it.

### Option (b) — Bolt onto schema
- Add `recall_*` tools to the existing schema HTTP daemon.
- Mixes vocabularies; same lifetime; same daemon owns both
  persistent corpus and ephemeral transcript.
- The `working_directory` walk-up doesn't fit transcript
  resolution (~/.claude/ paths).
- **Rejected** — once mixed, the surface can't be cleanly
  split later without breaking every consumer's `.mcp.json`.

### Option (c) — Recall as separate bounded context (chosen)
- New binary `recall` in this repo's Cargo workspace
  (ADR-0034).
- stdio MCP transport — process-per-Claude-Code-window.
- In-memory only for MCP path. No disk. No snapshot.
- Six MCP tools, all retrieval-shaped, all session-scoped.
- A separate CLI mode (ADR-0035) covers the operator-side
  cross-project arqueology with its own persistent cache —
  intentionally invisible to the LLM at runtime.
- **Selected.**

### Option (d) — Fold transcript indexing into Claude Code itself
- Out of our scope — we own the MCP side, not the client.
- **Rejected.**

## Decision

We adopt **option (c)**.

### 1. Bounded context

`recall` is its own bounded context. It owns the vocabulary
`Turn`, `ToolResult`, `Diff`, `Artifact`. It does **not**
import from schema's vocabulary (`Chunk`, `Embedding`,
`CorpusKind`).

### 2. Transport

stdio MCP server, spawned by Claude Code via `.mcp.json`:

```json
{
  "mcpServers": {
    "schema": {
      "type": "stdio",
      "command": "/usr/local/bin/schema",
      "args": ["mcp-shim"]
    },
    "recall": {
      "type": "stdio",
      "command": "/usr/local/bin/recall",
      "args": ["mcp-server"]
    }
  }
}
```

Recall **does not** speak HTTP. It does **not** run as a daemon.
One process per Claude Code window, dies on stdin EOF.

### 3. Scope (MCP path)

**Current session only.** Strict ephemeral. The recall MCP
process indexes only the `session.jsonl` of the session that
spawned it. Other sessions of the same project, other projects
on the same machine — invisible to the MCP path.

The operator-side CLI mode (ADR-0035) has a different scope
and is intentionally unreachable from the MCP surface.

### 4. Discovery

The recall MCP server resolves "which `session.jsonl` is mine"
via:

1. **Primary:** `CLAUDE_SESSION_ID` env var if Claude Code
   passes it on spawn (to be validated; if absent, fall back).
2. **Fallback:** the most-recently-modified `*.jsonl` under
   `~/.claude/projects/<encoded(CLAUDE_PROJECT_DIR)>/sessions/`.
3. **Operator override:** `recall mcp-server --jsonl <path>`
   for testing.

A startup `tracing::info` line records which path was selected
and via which mechanism so debugging is traceable.

### 5. Refresh model (d.3 from design)

Hook-driven preheat + on-demand jsonl tail.

- Claude Code's `~/.claude/settings.json` registers two hooks
  that fire on **every** turn boundary:
  - `UserPromptSubmit` — fires when the operator submits a prompt.
  - `PostToolUse` — fires after every tool call.
- Each hook is a one-line shell command that touches a dirty
  flag: `touch /tmp/recall-dirty-${CLAUDE_SESSION_ID}`.
- The recall MCP process tails the `session.jsonl` from a
  cursor (bytes consumed). On every `recall(query)` call, it
  checks the dirty flag mtime — if newer than the cursor's
  last-checked timestamp, it re-tails from the cursor, chunks
  the new turns, embeds, and inserts into the in-memory HNSW
  before performing the search.
- Result: refresh is lazy from the recall process's
  perspective and synchronous on the next call. The operator
  pays no perceptible latency between turns; the LLM pays at
  most a few hundred ms on a `recall(query)` after a long
  pause of activity.

### 6. Tool surface

Six retrieval-shaped tools. **No** `recall_summary` — recall
does not call any external LLM (no `LlmProvider` dependency,
no secret-management concerns mirroring ADR-0031). The LLM
caller already does synthesis with the chunks recall returns.

```
recall(query: String, top_k: Option<usize>)
  → Vec<Chunk> ranked by score; each chunk carries
    (turn_id_short, role, kind, score, content, metadata)
  → metadata includes `skipped_lines` count if the parser
    encountered malformed lines this refresh

recall_files()
  → Vec<{path, first_seen_turn, last_seen_turn, op_count}>
  → every file path that appeared in Read or Edit during
    this session

recall_diffs(path: Option<String>)
  → Vec<{turn_id_short, path, before_excerpt, after_excerpt}>
  → all Edit/Write operations; optionally filtered by path

recall_errors()
  → Vec<{turn_id_short, source: "bash"|"test"|"clippy"|"compile",
                       excerpt}>
  → semantic search over Bash/test/clippy outputs that match
    error/warning/failed patterns

recall_thread(turn_id_short: String,
              before: Option<usize>,
              after: Option<usize>)
  → Vec<Turn> — the turn plus N before and N after; default
    before=3, after=3

recall_artifacts()
  → Vec<{type: "file_created"|"file_edited"|"commit"|"adr",
         …}>
  → see §"Artifact taxonomy" below
```

`turn_id_short` is the first 8 hex chars of the per-turn UUID
written in `session.jsonl`. Short, copy-pasteable, low
collision in any realistic session (birthday-paradox at ~16k
turns; typical session has hundreds).

### 7. Indexed content (all 8 types)

Every turn type produced by Claude Code is chunked and indexed:

| Type            | Content                                  | Chunk strategy     |
|-----------------|------------------------------------------|--------------------|
| (a) assistant   | LLM reply text                           | paragraph (markdown chunker) |
| (b) user_prompt | operator message                         | paragraph |
| (c) read_result | file content from `Read`/`Edit` tool     | line-window with overlap |
| (d) bash_output | `Bash` stdout/stderr                     | line-window 20 lines, overlap 5 |
| (e) grep_result | `Grep`/`Glob` matches                    | metadata-only (path list ≠ retrieval body) |
| (f) web_result  | `WebSearch`/`WebFetch` content           | paragraph |
| (g) todo        | `TaskCreate`/`TaskUpdate` payloads       | paragraph (small) |
| (h) diff        | `Edit`/`Write` before/after blocks       | line-window with overlap (preserves hunk shape) |

The line-window strategy for (c)/(d)/(h) is necessary because
those payloads are often large (a 2000-line file Read produces
~50 chunks at line-50 windows). Paragraph chunking would lose
intra-block locality.

For (e) `Grep`/`Glob`: the result *is* a list of paths or
match locations — embedding it adds nothing; we store as
metadata-only (queryable by path substring or exact) and
return path lists when matched.

### 8. Embedder

- **Default:** MiniLM-L6 (384-dim, ~80 MB resident).
- **Opt-in:** bge-m3 (1024-dim, ~600 MB resident) via
  `--embedder bge-m3` flag on `recall mcp-server`.
- Per-window 2 GB RAM cap is the operator-tunable headroom for
  large sessions or larger embedders.

### 9. Failure modes

The session.jsonl tail is best-effort, not strict:

- **Malformed line:** skip, log `WARN`, continue. Each
  `recall(query)` result includes a `skipped_lines: usize`
  counter in metadata so the LLM (and operator) can see when
  data is degraded.
- **Last-line race** (Claude Code mid-write): hold last line,
  retry on the next `recall(query)` call after 100 ms (best-
  effort).
- **Hard size cap:** 500 MB per session.jsonl. Larger refuses
  to index with a clear error pointing at the CLI snapshot
  mode (ADR-0035).
- **File missing:** return empty result with a `WARN` to
  stderr; the LLM sees zero chunks and can decide.

### 10. Lifetime

```
SessionStart      → recall stdio process spawns (or earlier on
                    --preheat from a SessionStart hook follow-up)
during session    → recall stays alive, indexing on demand
SessionStop / EOF → stdin closes, recall drops index + embedder,
                    exits 0
```

No daemon. No service unit. No `endpoint.toml`. No
`secrets.toml`. None of the schema HTTP daemon's operational
surface applies.

### 11. Composition root

`crates/recall/src/main.rs` is the recall composition root.
Per ADR-0013 hexagonal:

- Domain: `crates/recall/src/domain/` — `Turn`, `Chunk`,
  `Artifact`, ubiquitous-language entities.
- Application: `crates/recall/src/application/` — use cases
  per tool (`Recall`, `RecallFiles`, `RecallDiffs`, etc).
- Adapters:
  - **Inbound:** `crates/recall/src/adapters/inbound/mcp_stdio.rs`
    (rmcp `transport-io`), `crates/recall/src/adapters/inbound/cli.rs`
    (operator subcommands; the CLI scope is governed by
    ADR-0035).
  - **Outbound:** `crates/recall/src/adapters/outbound/jsonl_reader.rs`,
    `embedder.rs` (fastembed), `hnsw.rs` (instant-distance or
    hnsw_rs).

Shared kernel via `crates/schema-core/`: embedder loader,
chunker (markdown / line-window), and any general retrieval
infrastructure that schema also uses. ADR-0034 covers the
workspace migration that creates `schema-core`.

## Consequences

- **Good.** LLM stops re-Read'ing files mid-session; recall
  returns the chunk it already produced.
- **Good.** Two servers in `.mcp.json` keeps the
  ubiquitous-language clean: `search` (schema) is for the
  *project's persistent corpus*; `recall` is for the
  *current session's transcript*.
- **Good.** No new persistent state on disk in the MCP path —
  `recall mcp-server` is process-per-window, in-memory only.
- **Good.** Hook-driven preheat (decisions 13) is one-line
  shell `touch`, fire-and-forget; no possibility of breaking
  Claude Code's hook chain.
- **Good.** The CLI mode (ADR-0035) covers cross-project
  arqueology without polluting the MCP surface.
- **Bad / accepted.** Two stdio MCP servers means more spawn
  cost: every Claude Code window pays 2 process forks
  instead of 1. Mitigation: schema's mcp-shim is already a
  per-window fork; recall adds one more, both are cheap
  compared to the LLM round-trip.
- **Bad / accepted.** Each Claude Code window holds its own
  embedder in RAM. For 5 simultaneous windows: 5 × ~80 MB
  (MiniLM) = ~400 MB; with bge-m3, 5 × ~600 MB = 3 GB.
  Operator decides via the embedder flag; the 2 GB per-
  window cap (decision 4) is the explicit headroom.
- **Bad / accepted.** First `recall(query)` of a session pays
  the cost of indexing from scratch (~ 1-3 s for typical
  sessions, up to ~30 s for very long debugging sessions
  with many large `Read` results). The MCP cold start is
  worse than schema HTTP (which amortizes across sessions),
  but the trade-off was made deliberately for the ephemeral
  scope.

## Fitness function

- **Unit test (chunker per type):** for each of (a)…(h), feed a
  synthetic turn and assert chunk count, kind, and metadata
  (`turn_id_short`, role) match expectations. Lives in
  `crates/recall/src/application/chunker.rs#[cfg(test)]`.
- **Unit test (jsonl tail with cursor):** seed a fixture
  `session.jsonl`, call `tail_from_cursor(0)`, assert all
  lines parsed; append two more lines, call
  `tail_from_cursor(prev_cursor)`, assert only the two new
  ones returned.
- **Unit test (skipped_lines metadata):** fixture jsonl with
  one malformed line in the middle. `recall(query)` returns
  results from valid lines and the `skipped_lines: 1` counter.
- **Integration test (MCP stdio handshake):** start a recall
  process via `Stdio::piped`, send `initialize` →
  `notifications/initialized` → `tools/list` →
  `tools/call recall {query: "x"}`. Assert correct response
  shape on each.
- **Integration test (turn_id_short collision):** fabricate
  100 turns with crafted UUIDs whose first 8 chars collide in
  pairs. Assert `recall_thread(turn_id_short)` returns an
  error envelope ("ambiguous; provide more chars") rather
  than a wrong turn.
- **Integration test (multi-window isolation):** spawn two
  recall processes pointing at two different fixture
  jsonls. Issue `recall(query)` to each. Assert results never
  cross-contaminate.
- **Doc lint:** the runbook (`arch/operations/runbook.md`)
  has a "Recall" section pointing at `recall mcp-server`
  setup and the hook config.

## Cross-references and follow-ups

- **ADR-0009 — generic MCP tools rule.** Recall's six tools
  follow ADR-0009: each is a single verb with a clear
  description. ADR-0009 gets an `Evidence and amendments`
  entry on accept.
- **ADR-0013 — Hexagonal.** Recall is its own hex stack
  (`domain/application/adapters`). Schema-core
  (ADR-0034) is shared kernel between schema and recall.
- **ADR-0019 — schema HTTP transport.** Unchanged. Schema
  stays HTTP daemon. Recall lives next to it, doesn't
  replace it.
- **ADR-0030 — `mcp-shim`.** Schema continues to wire via
  shim. Recall does not need a shim because it talks stdio
  directly.
- **ADR-0034 — Cargo workspace migration.** Pre-condition
  for shipping the recall binary: the repo must first be
  split into a workspace (`schema-core` + `schema` bin +
  `recall` bin).
- **ADR-0035 — recall CLI global cache.** The other side of
  the recall coin: the operator-mediated CLI scope with
  cross-project, persistent storage. Intentionally
  invisible to the MCP surface.
- **Follow-up:** validate `CLAUDE_SESSION_ID` env var
  presence on stdio MCP spawn. Spike before
  implementation. If absent, the fallback heuristic must
  cover the multi-window same-project edge case (rare,
  documentable).
- **Follow-up:** measure `session.jsonl` size on a real 8 h
  debugging session before committing to the 500 MB cap.
  Adjust if the typical case clusters above 200 MB.
- **Follow-up:** evaluate fastembed model-mmap optimization
  to share the embedder ONNX file across multiple recall
  processes (saves resident memory across windows).

## Evidence and amendments

(none yet — this ADR is `accepted` ahead of the implementation
slice; follow-up commits will record measured cold-start
times, real session.jsonl sizes, and any environment-variable
discoveries during the spike.)
