---
status: accepted
date: 2026-04-27
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-27
refines: ["ADR-0008", "ADR-0011", "ADR-0033"]
---

# 0035 — Recall CLI: global multi-project cache for cross-session arqueology

> **Y-statement** — In the context of ADR-0033 introducing the
> `recall` bounded context with a strict-ephemeral MCP path (one
> stdio process per Claude Code window, in-memory only,
> session-scoped), and the operator-side use case "in a terminal,
> three days later, find what we did about the HTTP session-id
> bug last week" — which the MCP path explicitly does not serve
> because it sees only the *current* session — facing the choice
> between (a) **operator opens Claude Code, asks the LLM,
> resigns to lossy LLM memory** (defeats the purpose of having
> recall at all), (b) **MCP path widens to all sessions**
> (breaks ADR-0033 ephemeral promise; forces persistent storage
> on the LLM-facing side; gives the LLM access to operator
> data from sessions long past, which is a capability creep
> with no demand), (c) **CLI mode of the recall binary maintains
> a persistent multi-project cache** under `~/.cache/recall/global/`
> with the last 50 sessions per project indexed via
> `sqlite-vec`, refreshed by mtime delta-sync per
> `session.jsonl`, exposed via `recall query "X"`,
> `recall files`, `recall diffs`, `recall errors`, `recall
> thread`, `recall artifacts` subcommands — same verbs as the
> MCP path, different scope, same binary, different process
> instantiation, or (d) **separate `recall-history` binary**
> just for the persistent path (three binaries instead of two;
> ADR-0033 considered and rejected this direction), we decided
> for **(c)** — the recall binary has two modes (`mcp-server`
> for the LLM, plain CLI subcommands for the operator); each
> mode has its own scope; the modes do not share state at
> runtime; the CLI mode maintains the persistent cache, the
> MCP mode does not, against (a) (operator-side dig is real
> and recurring; ignoring it preserves token waste in a
> different shape), (b) (LLM access to past sessions is
> capability creep without demand; if it ever becomes a real
> need, a future ADR can add an `--with-history` opt-in flag
> to `recall mcp-server`, but starting closed is safer), and
> (d) (three binaries to ship and codesign for what is
> conceptually one tool with two modes is unjustified
> overhead; one binary, two subcommand families is the
> conventional shape), to achieve **operator-side cross-
> project arqueology with sub-second query latency on a
> bounded cache size, while preserving the MCP path's
> ephemeral guarantee**, accepting **(1) one new persistent
> store at `~/.cache/recall/global/index.db` (sqlite-vec,
> mirrors ADR-0011's storage choice for the schema HTTP
> daemon), (2) bounded retention — the last 50 sessions per
> project, oldest evicted on insert — keeps cache size
> predictable (~ 200 MB worst case, ~ 50 MB typical) and
> avoids a long-tail of stale sessions polluting recall, (3)
> the CLI mode is invisible to the LLM at runtime — the MCP
> path will never reach into the global cache, and there is
> no `recall_history` MCP tool surface, (4) the cache is
> per-operator (lives under `$HOME`); cross-machine syncing
> is out of scope, mirrors ADR-0008 isolation; ADR-0008
> Evidence amended to record that recall introduces a
> sibling cache directory under `~/.cache/recall/` rather
> than reusing `~/.cache/schema/`, (5) cache rebuild on
> corrupted index is a single-command recovery
> (`recall cache rebuild`); the cache is by design
> reproducible from the source `session.jsonl` files, so
> losing it is a cost, not a disaster.**

## Context and Problem Statement

ADR-0033 §"Decision §3 Scope (MCP path)" pinned the recall MCP
server to **current session only**. That decision is correct
for the LLM-facing use case: when the LLM is in the middle of a
conversation, dragging in transcripts from sessions weeks past
risks polluting the response and defeating the bounded-
context-of-now intent.

But the operator has a different need. Three days after closing
Claude Code, in a terminal, looking at a regression: "what did
we change in the auth path? was that this branch or main? was
that two sessions ago or last Friday?". The transcripts are on
disk under `~/.claude/projects/`, but they are line-delimited
JSON-LD — `grep` works for literal terms, not for semantic
match.

Schema's HTTP daemon is **not** the answer. It indexes the
project's persistent corpus (ADRs, glossary, etc.) — not
session history.

The LLM does not need this capability either. If we put it on
the MCP path the LLM gets implicit cross-session memory which:

- Bloats prompts — the LLM can pull chunks from sessions far
  outside the current scope.
- Risks privacy creep — past prompts surface in current replies
  without the operator realising.
- Breaks ADR-0033's ephemeral promise — the moment the MCP
  server reads from a persistent store, the "process-per-
  window, dies on EOF" guarantee no longer holds end-to-end.

The operator-side need is real. The LLM-side need is not.

## Decision drivers

- **Operator can dig across sessions.** `recall query "X"` in
  a terminal returns top-K chunks across the last N sessions
  of the current project (and optionally cross-project with
  `--all` or `--project`).
- **MCP path stays ephemeral.** ADR-0033's promise holds
  end-to-end. The `recall mcp-server` process never reads
  from `~/.cache/recall/global/`.
- **One binary, two modes.** Same `recall` binary, two
  subcommand families. The CLI mode is the operator's; the
  MCP mode is the LLM's. They share the embedder loader and
  the chunker (via `schema-core`, ADR-0034) but **not**
  runtime state.
- **Bounded retention.** Cache size predictable. Pruning by
  count (last 50 sessions per project) avoids time-window
  surprises.
- **Reproducible from source.** The cache is a derived
  artefact of the `session.jsonl` files. Losing the cache
  costs CPU to rebuild, not data.

## Considered options

### Option (a) — No CLI mode, operator falls back to grep + LLM
- Operator pain unaddressed.
- The recall binary becomes pure-LLM-tooling.
- **Rejected.**

### Option (b) — Widen MCP path to all sessions
- LLM gets implicit cross-session memory.
- ADR-0033 ephemeral promise broken; persistent store on the
  LLM-facing side.
- Capability creep with no demand.
- **Rejected.**

### Option (c) — CLI mode in same binary, separate persistent cache (chosen)
- `recall mcp-server` (MCP, ephemeral, current session) and
  `recall query "X"` / `recall files` / etc. (CLI, persistent,
  cross-session) coexist as two subcommand families.
- The persistent cache lives under `~/.cache/recall/global/`
  (sibling to `~/.cache/schema/`, mirrors ADR-0008 isolation
  pattern).
- Schema-core (ADR-0034) provides the embedder + chunker
  shared between both modes; runtime state is independent.
- **Selected.**

### Option (d) — Separate `recall-history` binary
- Three binaries to ship, codesign, document.
- Same conceptual tool, split across two install paths.
- Conceptual cleanliness gained; operational overhead
  doubled.
- **Rejected.**

## Decision

We adopt **option (c)**. The recall binary has two
subcommand families.

### Subcommand surface (CLI side)

```
recall query "<query>" [--top-k N] [--project <path>] [--all]
                        [--since <date>]
recall files [--project <path>] [--all]
recall diffs [--path <pattern>] [--project <path>] [--all]
recall errors [--source bash|test|clippy] [--project <path>] [--all]
recall thread <turn-id-short> [--before N] [--after N]
recall artifacts [--type file_created|file_edited|commit|adr]
                  [--project <path>] [--all]
recall list-sessions [--project <path>]
recall cache rebuild [--project <path>]
recall cache prune  [--project <path>]
recall cache stats
```

Default scope when no flag is given: **current project**
(walked up from CWD to find `~/.claude/projects/<encoded(cwd)>/
sessions/`). `--all` widens to every project the operator has
ever opened. `--project <path>` pins a specific project.

The `mcp-server` subcommand stays separate and never
reaches into the persistent cache.

### Persistent cache layout

```
~/.cache/recall/global/
├── index.db                   sqlite-vec store
├── manifest.toml              { sessions: [{path, project_id,
│                                            cursor_bytes,
│                                            mtime, hash}] }
└── meta/
    └── projects/<project_id>/ optional per-project notes
```

Schema mirrors ADR-0011: `sqlite-vec` is the vector store.
This re-uses the lessons learned from the schema HTTP daemon's
cache (mode 0600 on the file, FTS5 + vec0 triggers, WAL mode
for concurrent reads).

### Retention policy

- **Per-project cap:** the last 50 sessions of a given project
  stay in the index. Older sessions are evicted on insert.
- **Global cap:** none. Multi-project totals are bounded by
  the per-project cap × project count.
- **Eviction is logical only.** The `session.jsonl` source
  files under `~/.claude/projects/` are not touched. A
  `recall cache rebuild` can re-read evicted sessions if the
  operator manually raises the cap (future flag: `--keep N`).

### Refresh model

- **On every CLI invocation:** the binary scans
  `~/.claude/projects/**/sessions/*.jsonl`, compares mtime
  against `manifest.toml`, and indexes only the delta. Mirrors
  ADR-0007 delta-sync for schema.
- **On `recall cache rebuild`:** drops `index.db` and
  re-builds from scratch using all sessions in the retention
  window.
- **No daemon.** No watcher. The cache is touched only when
  the operator runs the CLI.

### MCP path isolation (mandatory invariant)

`recall mcp-server` **must not** open `~/.cache/recall/`. The
binary's composition root checks the subcommand and refuses
to wire the persistent-cache outbound adapter for the
mcp-server path. A debug log line at startup confirms the
chosen mode:

```
INFO recall: mode=mcp-server (ephemeral, current session)
INFO recall: source=/Users/.../sessions/abc.jsonl
```

vs.

```
INFO recall: mode=cli (persistent global cache)
INFO recall: cache=~/.cache/recall/global/index.db
INFO recall: scope=current-project (cwd=/Users/.../proj-X)
```

A unit test asserts that the mcp-server constructor cannot be
invoked with a non-`None` cache path; the type system enforces
the split via `enum Mode { Mcp { ... }, Cli { ... } }` at the
composition root.

### Tool/CLI verb parity

Same six verbs the MCP path exposes (recall, files, diffs,
errors, thread, artifacts) appear as CLI subcommands with the
same semantics. Two adapters drive them:

- **Inbound MCP stdio:** wraps each verb as an rmcp `#[tool]`,
  scope = current session.
- **Inbound CLI:** wraps each verb as a clap subcommand, scope
  = current project (default) / cross-project (`--all`).

Application services (use cases, ADR-0013) are shared.
Outbound adapters differ: MCP wires `JsonlReader` only; CLI
wires `JsonlReader` **and** `SqliteVecCache`.

## Consequences

- **Good.** Operator gets cross-session arqueology with sub-
  second latency after the first build of the cache.
- **Good.** MCP path's ephemeral promise (ADR-0033) is
  preserved end-to-end. The LLM never reaches into the
  persistent cache.
- **Good.** Re-uses ADR-0011's `sqlite-vec` choice — no new
  storage technology to evaluate.
- **Good.** Re-uses ADR-0008's isolation pattern — sibling
  directory under `~/.cache/recall/` rather than commingling
  with schema's cache.
- **Good.** Cache is pure derived state. Loss is recoverable
  via `recall cache rebuild`.
- **Bad / accepted.** Two mental models for the same tool —
  operator must understand "MCP scope is current session, CLI
  scope is multi-session". Documented in the runbook section
  to be added.
- **Bad / accepted.** First CLI invocation after a hot
  laptop restart pays the cost of indexing the retention
  window from scratch. For a heavy operator with 50 sessions
  per 5 projects = 250 jsonls totaling perhaps 500 MB of
  text, the embed cost is non-trivial (10-20 minutes with
  MiniLM-L6 on CPU). Mitigation: the cache is incremental
  thereafter; restarts are rare; an explicit `recall cache
  rebuild` is the one slow command.
- **Bad / accepted.** The 50-session per-project cap is a
  guess. If the typical operator's debugging cadence puts
  meaningful state in session 60 for a given project, that
  state is not retrievable. Adjustable via a future
  `recall config set retention.sessions <N>` knob; not
  implemented at v1.

## Fitness function

- **Unit test (mode isolation):** the binary's composition
  root has a `Mode` enum; constructing
  `RecallApp::new(Mode::Mcp(…))` does not produce any
  `SqliteVecCache` reference. A unit test asserts the type
  system enforces this — attempting to construct
  `Mode::Mcp { cache: Some(_) }` does not compile.
- **Unit test (delta sync):** seed `~/.cache/recall/global/`
  fixtures with manifest pointing at two `session.jsonl`s;
  modify one of the source jsonls (append turns); call the
  delta-sync; assert only the modified jsonl is re-indexed.
- **Integration test (CLI scope default):** run
  `recall query "X"` in a fixture project directory; assert
  the resolved scope is `current-project` and the search hits
  only sessions under that project.
- **Integration test (CLI scope --all):** run
  `recall query "X" --all`; assert the search returns chunks
  from at least two distinct project_ids.
- **Integration test (retention eviction):** seed 51
  sessions for one project; run `recall cache prune`;
  assert the oldest is evicted, count is 50.
- **Integration test (rebuild reproducibility):** snapshot
  `recall query "X"` results; run `recall cache rebuild`;
  assert the same query returns identical top-K (modulo
  embedding determinism).
- **Doc lint:** the runbook (`arch/operations/runbook.md`)
  has a "Recall CLI" section pointing at
  `recall query`, `recall list-sessions`, and `recall
  cache rebuild` with example output.

## Cross-references and follow-ups

- **ADR-0007 — delta-sync.** The CLI cache uses the same
  pattern (cursor + mtime per source). ADR-0007 Evidence
  amended on accept.
- **ADR-0008 — cache isolation by project.** Recall introduces
  a sibling root `~/.cache/recall/` next to
  `~/.cache/schema/`. Same isolation pattern, different
  data. ADR-0008 Evidence amended.
- **ADR-0011 — `sqlite-vec` store.** Re-used for the recall
  CLI cache. Same WAL, FTS5 + vec0 triggers, mode-0600 file.
- **ADR-0013 — Hexagonal.** CLI is an inbound adapter
  (clap-driven), parallel to the stdio MCP inbound adapter.
  Application services are shared.
- **ADR-0033 — recall bounded context.** This ADR is the
  CLI side of the same context; ADR-0033 is the MCP side.
  Read together.
- **ADR-0034 — Cargo workspace migration.** Pre-condition.
  CLI and MCP modes coexist in `crates/recall/src/main.rs`
  via `#[derive(clap::Subcommand)]` (or builder API per
  ADR-0012's clap-derive incompatibility note) dispatching
  to different adapters.
- **Follow-up:** measure typical operator cache size after
  one month of daily use. Adjust the 50-session cap if
  realistic data points outside the window.
- **Follow-up:** decide on cross-machine sync. Today the
  cache is per-operator-machine. If multi-machine becomes
  a need, a separate ADR covers replication strategy.
- **Follow-up:** evaluate `recall config` subcommand for
  operator-tunable knobs (retention, default scope, embedder
  model). Not implemented at v1.
- **Follow-up:** consider an `--with-history` flag on
  `recall mcp-server` that opt-in widens the MCP path's
  scope to read the global cache. Held until at least one
  operator reports the lack as friction during a real
  session.

## Evidence and amendments

(none yet — this ADR is `accepted` ahead of the implementation
slice; follow-up commits will record measured cache sizes,
delta-sync timings on real sessions, and any retention-cap
adjustments.)
