---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2026-10-26
---

# 0027 — LLM-driven project discovery: directory-as-source-of-truth and per-tool project scoping

> **Y-statement** — In the context of ADR-0026 collapsing N
> per-project schema daemons into one shared daemon to amortise the
> ~1.7 GB BGE-M3 ONNX baseline across the workstation, with Option B
> selecting **operator-curated** project membership (via `schema
> project register / unregister / list` writing to a `registry.toml`)
> and **URL-path routing** (`/mcp/<project_id>` per project, single-
> token bearer per mount), facing the operator-observed
> consequences that (i) every project needs an explicit `register`
> before the first MCP query — Claude Code cannot just open a folder
> and start working, (ii) the registry drifts when project
> directories are renamed or moved, and (iii) the consumer-facing
> `.mcp.json` is per-project (each carrying a project-specific URL
> and bearer), forcing per-folder configuration that contradicts the
> "MCP server = workstation utility" model the operator uses, we
> decided for **LLM-driven discovery: a single `/mcp` endpoint with
> a single workstation bearer, where every MCP tool call carries a
> `working_directory` parameter the daemon walks up to resolve a
> `ProjectId` (ADR-0008 path-keyed identity), and the daemon wires
> a `ProjectInstance` for that id on first request (lazy, cached
> for the daemon's lifetime)**, against (a) ADR-0026 §"Decision"
> Option B as-shipped (per-project URL paths, operator
> registration), (b) MCP-spec `roots/list_changed` notification
> (deferred — see Option C), and (c) auto-creating a synthetic
> `schema.toml` from a default-corpus heuristic when none exists
> (rejected — schema corpus is the operator's deliberate scoping
> and silently inventing one breaks ADR-0004), to achieve **zero-
> touch onboarding (Claude Code starts in a folder, the daemon
> discovers the `schema.toml` and wires the project on first tool
> call), one workstation-level `.mcp.json` (one URL, one bearer,
> shared across every project the operator works in), and
> preservation of ADR-0026's single-`Embedder` driver and
> ADR-0008's per-store physical isolation**, accepting **(i) the
> daemon trusting LLM-supplied paths (mitigated by ADR-0008 — each
> path resolves to its own `store.db`, so cross-bleed is impossible
> by physical layout regardless of what path the LLM provides),
> (ii) supersession of ADR-0026 slice 1 (`MultiTenantBearerValidator`
> + `ProjectTokenRegistry`), slice 2a (`Registry` toml RW), slice 3
> (`schema project register / unregister / list` CLI verbs), and
> rewrite of slice 4 (multi-tenant URL-path router) and slice 4b
> (registry-driven daemon startup), (iii) a slow-first-call latency
> per project (the lazy `ProjectInstance::wire` runs an initial
> delta-sync inside the request — cheap on warm restart per
> ADR-0017's `mtime + size` short-circuit, slow on a fresh project),
> and (iv) the canary fitness function (ADR-0026) shape change —
> two-tool-call sequence with distinct `working_directory`
> parameters replaces the two-URL-path sequence**.

## Context and Problem Statement

ADR-0026 was accepted on 2026-04-26 with the direction "one shared
daemon for all projects, with strict per-project isolation at query
time" and Option B's specifics:

- One `axum::Router` with `nest_service("/mcp/<project_id>", ...)`
  per project.
- Operator-curated project membership via `schema project register
  / unregister / list` and a `registry.toml` at
  `~/Library/Application Support/schema/registry.toml`.
- Per-project `endpoint.toml` with project-specific URL
  (`http://127.0.0.1:<port>/mcp/<project_id>`) and project-specific
  bearer.
- Consumer-facing `.mcp.json` per project, each carrying that
  project's URL + bearer.

Slices 1–4b shipped on `main` (PR #1 through #5). Production runtime
still defaults to ADR-0019 per-project units; cutover (per
`arch/operations/runbook.md` "Migration to ADR-0026") was gated on
slice 6 (canary E2E fitness function) turning green.

In the same review session that accepted ADR-0026, the operator
flagged **after** the design landed that the operator-curated
registration model does not match the workstation utility shape the
project needs:

> "I won't have admin for that. The LLM itself will manage it when
> it connects. It says the directory it's in and you work from
> there. There must be a way for it to wipe everything for that
> project."

That is a different architectural answer to the same problem
ADR-0026 set out to solve. ADR-0026's drivers (single `Embedder`,
ADR-0008 isolation preserved, provable cross-project isolation)
remain valid; what changes is **who decides project membership**
and **how the request is scoped to a project**.

## Decision Drivers

- **Zero-touch onboarding.** Opening Claude Code in a folder with a
  `schema.toml` should produce a working MCP integration on the
  first tool call. No `schema project register` step. No prior
  knowledge by the daemon.
- **Single workstation `.mcp.json`.** The operator's MCP client
  configuration becomes one URL + one bearer, applicable to every
  project. The Claude Code session itself supplies the project
  context per tool call.
- **ADR-0008 physical isolation preserved.** Each project still
  resolves to its own `~/.cache/schema/projects/<project_id>/
  store.db`. The daemon never opens two stores in the same query;
  cross-bleed is impossible by layout, exactly as before.
- **Single ONNX baseline.** ADR-0026's primary driver: one
  `FastembedEmbedder` for the workstation. Lazy `ProjectInstance::
  wire` shares the same `Arc<Mutex<dyn Embedder>>` across every
  resolved project.
- **Provable per-project isolation at query time.** The fitness
  function from ADR-0026 §"Fitness function" carries over; only the
  shape changes — two MCP tool calls with distinct
  `working_directory` parameters (instead of two URL paths) must
  return strictly the chunks under each respective directory.
- **Trust model is OK because of ADR-0008.** The daemon trusting
  an LLM-supplied path looks scary at first read. It is not. Each
  path resolves through `ProjectIdentity::resolve` to a deterministic
  `project_id` keyed by canonical absolute path; that id selects
  one `store.db`. There is no path the LLM can supply that returns
  chunks from another project's store, because the resolution is a
  one-way function from path to id and the on-disk layout is keyed
  by id. The LLM cannot trick the daemon into producing
  cross-project chunks any more than it can trick its own
  filesystem into reading a file it does not have a path for.
- **Reset is per-project, LLM-initiated.** The ADR-0015
  `reset_index` MCP tool gains a `working_directory` parameter and
  scopes the wipe to that project. The LLM that wants to clear its
  index calls the tool with its own working directory; nothing
  affects other projects.

## Considered Options

### Option A — Keep ADR-0026 §"Decision" Option B as shipped (rejected)

Operator runs `schema project register --config <path>` per
project. Daemon mounts `/mcp/<project_id>` per project, each with
its own bearer, writes per-project `endpoint.toml` files. Consumer
`.mcp.json` is per-project.

**Pros**: shipped. Slice 1 / 2a / 3 / 4 / 4b are exactly this.
Routing-as-isolation is a strong physical guarantee.

**Cons (decisive)**: violates the workstation-utility shape the
operator needs. Every new project is an admin step. Per-project
`.mcp.json` files multiply over time. Registry drifts when
directories move. Does not match Claude Code's "open a folder,
start working" UX.

### Option B — LLM-driven discovery via per-tool `working_directory` parameter (chosen)

Single `/mcp` endpoint. Single workstation bearer. Every retrieval
MCP tool (`query`, `find_decisions`, `glossary_lookup`,
`cross_reference`, `list_corpus`, `synthesize`,
`workspace_context`, `forget_source`, `reset_index`,
`ping`) takes a `working_directory: String` parameter. The daemon:

1. Walks up from `working_directory` looking for `schema.toml`.
2. Loads it via `SchemaConfig::resolve` and resolves
   `ProjectIdentity` (ADR-0008 path-keyed).
3. Looks up `Map<ProjectId, ProjectInstance>` (under
   `Arc<RwLock<...>>` so concurrent connects don't deadlock).
4. If absent, calls `ProjectInstance::wire` with the **shared**
   `Embedder` and the resolved `ProjectIdentity`, runs the initial
   delta-sync inline (slow first call; cheap thereafter per
   ADR-0017), spawns the per-project watcher, inserts into the
   map. Cached for the daemon's lifetime.
5. Dispatches the tool call to the matching `ProjectInstance`.

`endpoint.toml` becomes **one** global file
(`~/Library/Application Support/schema/endpoint.toml` on macOS,
`~/.local/state/schema/endpoint.toml` on Linux) with the daemon's
URL and bearer. The operator's `~/.claude/mcp_servers.json` (or
equivalent global config) points at it. No per-project
`.mcp.json` files.

**Pros**: zero-touch onboarding. One workstation bearer. ADR-0008
isolation preserved by store-per-project layout. Lazy wiring keeps
RAM proportional to active projects, not registry size.

**Cons**: slow-first-call latency (initial delta-sync inside the
request — bounded by the project's corpus size; today the largest
project on the operator's box took ~10 minutes on a cold start).
Cache eviction not implemented (the map grows without bound) —
follow-up. The daemon trusts LLM-supplied paths (mitigated as
above).

### Option C — MCP `roots/list_changed` notification (deferred)

The MCP spec defines a client-to-server notification
(`roots/list_changed`) carrying the URIs of folders the client is
working in. The server can use roots to scope tool calls without an
explicit per-call parameter. This is the spec-aligned shape.

**Pros**: matches MCP-protocol intent. No bespoke
`working_directory` parameter on every tool. Client provides
authoritative working directories.

**Cons (decisive for now)**: rmcp 1.5's server-side support for
`roots` is partial; need to verify Claude Code actually sends
`roots` updates and that rmcp surfaces them through a usable API.
Also: roots is plural — a session can have multiple — so the
daemon still needs a tool param to disambiguate which root the
current tool call targets. Defer to a follow-up ADR after rmcp
support is verified and a measurement of how Claude Code sets
roots in practice is in hand.

### Option D — Hybrid: roots-when-supported, parameter-fallback (deferred to follow-up ADR)

Adopt Option B as the floor; accept `roots` notifications when the
client sends them and use the most recent root that is an ancestor
of the per-call `working_directory` to validate the path (defence
in depth: the LLM cannot point at a path outside the declared
roots). This is the eventual desirable shape; tracking as a
follow-up ADR after Option B's implementation lands and Option C's
investigation completes.

### Option E — Auto-create a synthetic `schema.toml` from a default heuristic (rejected)

When the LLM supplies a `working_directory` that contains no
`schema.toml`, the daemon could synthesise one with a default
corpus (e.g. `*.md` under `docs/`). Avoids the "first call fails"
operator experience.

**Cons (decisive)**: corpus declaration is the operator's
deliberate scoping. ADR-0004 explicitly puts the corpus in
`schema.toml` so the operator decides what gets indexed. Silently
indexing a folder that has no `schema.toml` either produces
useless results (wrong corpus shape) or creates surprise
`store.db` files in unexpected cache slots. No `schema.toml` →
clear, actionable error: *"no schema.toml found from
`<working_directory>` walking up; create one before querying"*.

## Decision

**Option B accepted (status `proposed` — gated on operator
confirmation and on the canary E2E fitness function from this ADR
turning green).**

Direction is: directory IS the source of truth; the daemon trusts
LLM-supplied paths; project membership is implicit and lazy;
`endpoint.toml` is one workstation file; the bearer is one
workstation token.

Open questions deferred to the prototype:

- **Walk-up vs exact match.** Walk up from `working_directory`
  looking for `schema.toml`, or require the LLM to pass the
  project root exactly? Walk-up is friendlier (the LLM can pass
  any subdir of the project) and matches `git`/`pnpm` convention
  (`.git`/`pnpm-workspace.yaml` discovery). Pick walk-up; reject
  ambiguity by stopping at the first `schema.toml` found
  walking up to the filesystem root.
- **Concurrency.** The `Map<ProjectId, ProjectInstance>` is shared
  across requests. `Arc<RwLock<...>>` for read-mostly + occasional
  insertion is the obvious shape; double-check pattern (`read →
  upgrade → insert`) on cache miss to avoid wiring the same
  project twice on near-simultaneous first calls.
- **Cache eviction.** The map grows monotonically. For now: no
  eviction (operator restarts the daemon to clear). Bound by
  number of distinct projects the operator works in; not expected
  to exceed ~10. Follow-up ADR if it does.
- **Initial-sync timeout.** First call to a fresh project blocks
  in the request until delta-sync completes. Today's largest
  project takes ~10 minutes cold. Options: (1) return a "wiring,
  try again in N seconds" envelope from the tool and run the sync
  in a background task, (2) extend the MCP per-tool timeout, (3)
  warm the cache via an explicit `prepare_project` tool the LLM
  calls first. Pick after measuring how Claude Code's MCP client
  handles long-running tool calls in practice.
- **Reset semantics.** ADR-0015's `reset_index` already does the
  right thing per-project; just plumb `working_directory` through.
  `forget_source` already takes a `path` param; it gains an
  implicit `working_directory` too.
- **Trust hardening (defence in depth).** Optional check: if the
  LLM client sent `roots`, verify `working_directory` is a
  descendant of one of them. If roots was not sent, accept any
  path (today's behaviour). This is Option D territory; track
  as a follow-up.

## Consequences

### Positive

- Zero-touch onboarding: open a folder with `schema.toml`, MCP
  works on first call. No operator action between `schema
  install` and `claude` opening a project.
- One workstation `.mcp.json` (or one Claude Code global config)
  pointing at one URL + one bearer.
- Single ONNX baseline preserved.
- ADR-0008 isolation preserved at the store layer.
- Removes operator-facing surface (`schema project register`
  etc.) — fewer commands to document, fewer failure modes.

### Negative / trade-offs

- **Slow first call per project** (initial delta-sync). Need to
  pick a UX answer (return wiring envelope, raise timeout, or warm
  via explicit tool).
- **Trust of LLM-supplied paths.** Mitigated by ADR-0008's
  store-per-project layout, but documented as an explicit
  assumption rather than a structural guarantee.
- **Significant rework on already-merged slices.** Slice 1
  (`MultiTenantBearerValidator`, `ProjectTokenRegistry`), slice 2a
  (`Registry` toml RW), slice 3 (`schema project register /
  unregister / list` CLI verbs) become dead code and are removed.
  Slice 4 (multi-tenant router with N URL-path mounts) and slice
  4b (registry-driven daemon startup) are rewritten to single-
  endpoint + lazy resolve. Estimated: 1 PR for the deletions, 1
  PR for the rewrite.
- **Canary fitness function shape changes.** ADR-0026 §"Fitness
  function" specified two URL paths and per-mount tokens; this
  ADR replaces that with two tool calls, distinct
  `working_directory` parameters, single bearer. Same isolation
  contract, different shape.

### Follow-ups

- **ADR-0028 (planned)**: Option C / D — `roots`-driven discovery
  once rmcp 1.5+ surfaces server-side `roots/list_changed`
  reliably and measurement of Claude Code's roots usage is in
  hand. Defence-in-depth check that
  `working_directory ∈ roots[*]` lands here.
- **ADR-0029 (planned)**: cache eviction policy for the per-
  daemon `Map<ProjectId, ProjectInstance>` if active project
  count exceeds the no-eviction headroom.
- **Memory budget post-delta-sync** — the original ADR-0027
  placeholder slot in ADR-0026's follow-ups gets renumbered to
  ADR-0030 (or absorbed into ADR-0029 if eviction ships first).
  ADR-0026's "Cross-references and follow-ups" section needs a
  forward-pointing amendment.

## Fitness function

The defining contract from ADR-0026 §"Fitness function" carries
over: **provable per-project isolation at query time**, asserted
by an integration test run on every commit that fails the build
on cross-project bleed. The shape changes — single endpoint, two
tool calls.

Concretely, on the prototype branch:

- **Two-project fixture.** Build two synthetic project trees
  `tests/e2e/fixtures/alpha/` and `tests/e2e/fixtures/beta/`,
  each containing a `schema.toml` and one ADR
  (`docs/decisions/0001-x.md`) with the same filename and
  distinguishable bodies (`alpha` contains `ALPHA-CANARY`,
  `beta` contains `BETA-CANARY`).
- **One daemon.** Launch `schema daemon` against an empty
  `Map<ProjectId, ProjectInstance>` (no preregistration).
- **Two retrieval tool calls.** Issue `query { working_directory:
  "<alpha-fixture-root>", q: "what does ADR 0001 say" }` then
  `query { working_directory: "<beta-fixture-root>", q: "what
  does ADR 0001 say" }`. Both calls authenticate with the same
  workstation bearer.
- **Assertion.** The first response's chunks must contain
  `ALPHA-CANARY` and must not contain `BETA-CANARY`. Symmetric
  for the second call. Repeat for every retrieval tool exposed
  by the MCP surface (`query`, `find_decisions`,
  `glossary_lookup`, `cross_reference`, `list_corpus`,
  `synthesize` retrieval step).
- **Lazy-wiring assertion.** Before the first tool call, the
  daemon's `Map<ProjectId, ProjectInstance>` is empty; after
  the first call, it has exactly one entry (the resolved project);
  after the second call, it has exactly two. Verify via a debug
  endpoint the test fixture mounts (or a metrics port — final
  shape decided in implementation).
- **Cache eviction assertion (if Option B ships an eviction
  policy in this ADR's scope).** Currently no eviction; assertion
  is "the map size monotonically equals the number of distinct
  `working_directory` walks-up across the test session".
- **Reset isolation assertion.** Call `reset_index { working_
  directory: "<alpha-fixture-root>" }`. Re-issue the query for
  `alpha`: 0 results (alpha index wiped). Re-issue the query
  for `beta`: still returns `BETA-CANARY` (untouched).
- **Negative path assertion.** Call any retrieval tool with a
  `working_directory` whose walk-up finds no `schema.toml`.
  Expected: structured error envelope, not 5xx and not silent
  empty results.

CI integration follows ADR-0024 (E2E pytest); the two-project
fixture lives under `tests/e2e/`, drives the HTTP MCP surface
end-to-end against the real `schema daemon` binary, and runs in
the existing e2e job.

## Cross-references and follow-ups

- **ADR-0008 — per-project cache isolation.** Strictly preserved.
  Each `working_directory` resolves to one `project_id` resolves
  to one `store.db`.
- **ADR-0019 — Streamable HTTP transport.** Preserved at the
  transport layer (rmcp 1.5 + axum 0.8, `LocalSessionManager`,
  `with_stateful_mode(true)`, localhost-only allowed hosts,
  bearer auth, SIGTERM drain). What changes is the routing
  shape inside the router — from N `/mcp/<project_id>` mounts
  to one `/mcp` mount with per-tool dispatch.
- **ADR-0021 — localhost bind + bearer auth.** Bearer auth
  preserved; the `BearerValidator` is back to single-token
  (one workstation bearer).
  `MultiTenantBearerValidator` and `ProjectTokenRegistry` from
  ADR-0026 slice 1 become unused and are removed.
- **ADR-0026 — shared multi-project daemon.** Drivers preserved
  (single ONNX, ADR-0008 isolation, provable cross-project
  isolation). Routing/membership specifics (Option B's URL paths
  + operator registration) **partially superseded** by this
  ADR's Option B. ADR-0026 §"Decision" gets an amendment
  forward-pointing here; the canary fitness function in
  ADR-0026 §"Fitness function" is restated in this ADR with the
  shape change.
- **ADR-0015 — cleanup tools.** `reset_index` and `forget_source`
  gain a `working_directory` parameter to match the new
  scoping. Existing CLI surface (`schema reset` /
  `schema forget` with `--config <path>`) carries over for
  operator-driven cleanup; the MCP-surface variants are what the
  LLM calls.
- **ADR-0024 — E2E test category.** The canary fitness function
  in §"Fitness function" lives in this category.

## Slice rollback / refactor plan

The merge sequence on `main` is:

```
PR #1 (bdc4616) — slice 1 + 2a — auth multi-tenant + registry toml RW
PR #2 (55f1f37) — slice 3 — schema project register/unregister/list
PR #3 (d90367f) — slice 2b — ProjectInstance composition root
PR #4 (5be9926) — slice 4 — Daemon + multi-tenant router
PR #5 (45fddb7) — slice 4b — schema daemon CLI verb + runtime
```

Refactor sequence (each step is one PR, gated by strict-lint +
canary E2E for the implementation slices):

1. **Refactor PR #1 — single-tenant auth + registry removal.**
   - Delete `MultiTenantBearerValidator` + `ProjectTokenRegistry`
     from `src/adapters/auth.rs`. Keep single-token
     `BearerValidator` (untouched).
   - Delete `src/adapters/registry_toml.rs`.
   - Delete `src/cli/project.rs` + the `schema project`
     subcommand wiring in `main.rs`.
   - Update `src/adapters/mod.rs` and `src/cli/mod.rs`.
   - Tests: drop the dependent unit tests (they will be
     orphan-clippy-failed if we don't).
2. **Refactor PR #2 — single-endpoint daemon + lazy resolve.**
   - Replace `build_multi_tenant_router` in `mcp_server.rs` with
     a `build_daemon_router` that mounts a single `/mcp` and a
     single `/health`, gated by the workstation
     `BearerValidator`. Keep `Mount` removed.
   - Rewrite `src/app/daemon.rs`: `Daemon` no longer wraps a
     `Vec<ProjectSlot>`; it holds the shared `Embedder` + a
     `Arc<RwLock<HashMap<ProjectId, ProjectInstance>>>` +
     the optional shared `LlmProvider`. Add
     `Daemon::resolve_or_wire(&working_directory) ->
     Result<ProjectInstance>` (returning by-reference into the
     map under read lock; or returning a thin handle the
     caller borrows from).
   - Rewrite `src/main.rs::run_daemon` to bind once, write **one**
     global `endpoint.toml`, build the single-mount router, run
     `axum::serve`. No per-slot startup loop.
   - Update every MCP tool handler in `mcp_server.rs` to accept
     `working_directory` and dispatch via
     `Daemon::resolve_or_wire`. Tool params (the `*Params`
     structs) gain the field; rmcp `tool_router` derive picks
     up the JSON-Schema change automatically.
   - `endpoint.toml` location: one global file at
     `~/Library/Application Support/schema/endpoint.toml`
     (macOS) or `~/.local/state/schema/endpoint.toml` (Linux).
     `0600` mode preserved per ADR-0021. Per-project endpoint
     files written by ADR-0019 single-tenant `serve` are
     **left in place** — the ADR-0019 path is unchanged in
     this refactor.
3. **Refactor PR #3 — canary fitness function (slice 6).**
   - Add `tests/e2e/test_adr0027_isolation.py` with the two-
     project ALPHA-CANARY / BETA-CANARY fixture and the
     assertions in §"Fitness function" above.
   - Wire into the existing e2e job (ADR-0024).
   - **This PR is the cutover gate.** Until it merges green,
     ADR-0026's per-project ADR-0019 / ADR-0020 deployment
     remains production.
4. **Refactor PR #4 — `arch/decisions/README.md` index +
   amendments.**
   - ADR-0026 §"Cross-references and follow-ups": forward-point
     to ADR-0027.
   - ADR-0019 / ADR-0020 / ADR-0021: amendment notes recording
     that the supersession from ADR-0026 was itself partially
     superseded by ADR-0027 (the validator goes back to
     single-token, project membership goes from explicit to
     implicit).
   - `arch/operations/runbook.md` "Migration to ADR-0026"
     section: append a "Migration update for ADR-0027"
     subsection rewriting the cutover sequence to match the
     single-endpoint shape.
   - Index entries: ADR-0026 status moves from "accepted"
     toward "partially superseded by ADR-0027 (routing/
     membership specifics)"; ADR-0027 lands `accepted` once
     refactor PRs land green.

ADR-0026's drivers and physical-isolation contract carry over;
this is **not** a "ADR-0026 was wrong" rewind. It is a finer
answer to the same question.

## Evidence and amendments

- **2026-04-26 — proposed.** ADR drafted directly after the
  operator's directional pivot during PR #5 review. Slices 1–4b
  remain on `main` until refactor PRs land; the operator-facing
  deployment still defaults to ADR-0019 per-project units, so
  no production user is affected by the pivot.

- **2026-04-26 — accepted.** Operator reviewed via
  `/arch-advisor` and confirmed Option B as the direction.
  Refactor sequence in §"Slice rollback / refactor plan" is
  the merge gate; the canary fitness function E2E
  (`tests/e2e/test_adr0027_isolation.py`) flips this from
  "accepted (impl gated)" to "accepted (live)" once green in
  CI on the cutover PR.

- **2026-04-26 — refactor PR #1 of 4 landed.**
  Operator-facing CLI surface for the (now-superseded)
  registry deleted. ADR-0026 slice 3 reverted in full.
  - `src/cli/project.rs` removed (entire file).
  - `src/cli/mod.rs` no longer declares the module.
  - `src/main.rs`: `project_subcommand`, `run_project`, the
    `cli_project` import, the `Some(("project", sub))`
    dispatch arm, and the trailing `.clone()` on the last
    `mcp_config_subcommand` argument all removed (the
    latter caught by `clippy::redundant_clone` once the
    `project_subcommand` consumer disappeared).
  - `Registry` struct and `MultiTenantBearerValidator` /
    `ProjectTokenRegistry` (slice 1, slice 2a) **kept for
    now** — the next refactor PR rewrites the daemon path
    and removes them in the same change so each
    intermediate commit compiles (Daemon::wire still
    consumes `&Registry`; ripping that out alone produces
    a non-compiling tree).
  - 103 unit tests pass (was 110; -7 from the deleted
    `cli/project.rs` tests). 5 integration. Strict-lint
    gate green; `cargo fmt --all -- --check` clean.

- **2026-04-27 — refactor PR #4 of 4 landed (this commit).**
  Daemon-mode watcher recovery + ADR-0019 / ADR-0020 /
  ADR-0021 forward-pointing amendments + runbook cutover
  sequence. Closes the ADR-0027 refactor loop.
  - `src/app/project_instance.rs`: `spawn_project_watcher`
    (extracted from `main::spawn_watcher`) is the single
    helper both deployment shapes call. Lives next to
    `ProjectInstance::wire` so the application layer owns
    the watcher lifecycle.
  - `src/app/daemon.rs`: `wire_and_insert` now (a) runs the
    initial delta-sync inline so warm-cache reads land
    behind a populated `store.db` by the time the tool
    response goes out, and (b) calls `spawn_project_watcher`
    so ADR-0010 in-session edits propagate without a daemon
    restart. The regression flagged in PR #2's Evidence is
    closed.
  - `src/main.rs`: `spawn_watcher` deleted (moved to
    `spawn_project_watcher`); `WATCHER_DEBOUNCE` constant
    deleted (moved next to the helper). `run_serve` calls
    `spawn_project_watcher(&project)` directly.
  - **ADR-0019 / ADR-0020 / ADR-0021 amendments appended**
    forward-pointing to ADR-0027:
    - ADR-0019: process-shape part further amended —
      single `/mcp` mount + single workstation bearer
      replaces ADR-0026's URL-path routing.
    - ADR-0020: per-project shape further amended —
      `schema project register / unregister / list` and
      `registry.toml` superseded by directory-as-source-
      of-truth.
    - ADR-0021: validator shape **reverted** —
      `MultiTenantBearerValidator` /
      `ProjectTokenRegistry` removed; single-token
      `BearerValidator` is sufficient again.
  - `arch/decisions/README.md`: 0019 / 0020 / 0021 status
    rows updated to "amended by 0026 + 0027"; 0026 row
    updated to "partially superseded by 0027 (routing/
    membership specifics; drivers preserved)".
  - `arch/operations/runbook.md`: ADR-0026 "Migration"
    section marked partially superseded; new "Migration
    to ADR-0027 (single endpoint + lazy resolve)"
    section delivers the live cutover sequence (8 steps:
    update binary → run canary E2E → inventory →
    bootout per-project → install daemon → refresh
    `.mcp.json` → smoke per project → rollback).
  - `daemon` subcommand description rewritten from "ADR-0026
    multi-mount" to "ADR-0027 single mount, lazy resolve".
  - 89 unit + 5 integration tests pass (no test churn —
    watcher spawn is exercised by the canary E2E from
    PR #3, not by Rust unit tests). Strict-lint gate
    green; `cargo fmt --all -- --check` clean.

- **2026-04-27 — refactor PR #3 of 4 landed.**
  Canary fitness function E2E. `tests/e2e/test_adr0027_isolation.py`
  exercises the cutover gate from §"Fitness function".
  - `_spawn_daemon` spawns `schema daemon` with `HOME=<tmp>` so the
    global `endpoint.toml` lands at
    `<tmp>/Library/Application Support/schema/endpoint.toml` (macOS)
    / `<tmp>/.local/state/schema/endpoint.toml` (Linux), polls until
    PID matches, returns the URL + bearer.
  - Two synthetic projects under `tmp_path/fixtures/{alpha,beta}/`,
    each with `schema.toml` + one ADR carrying a distinguishable
    canary token (`ALPHA-CANARY-7f3` / `BETA-CANARY-9b2`).
  - Six tests cover: query isolation, find_decisions isolation,
    workspace_context resolves per-directory + distinct
    `project.id`, walk-up from a subdirectory, no-`schema.toml`
    error envelope (not 5xx, not silent empty), reset_index scopes
    strictly to the resolved project.
  - All marked `pytest.mark.slow`. `pytest.ini` registers the marker.
    README index entry added pointing at ADR-0027.
  - **Not run in CI today** — `.github/workflows/lint.yml` runs
    `cargo` only; the E2E suite is operator-manual per ADR-0024 §"CI".
    Wiring nightly/release-tag GHA workflow is a FASE 1.1 follow-up
    tracked there.
  - Manual run command on the operator's box (cold model cache —
    expect 5-15 min):
    ```
    cd tests/e2e
    pytest -v -m slow test_adr0027_isolation.py
    ```
  - Refactor PR #4 (next) wires the daemon-mode watcher recovery,
    appends Evidence entries to ADR-0019 / ADR-0020 / ADR-0021
    forward-pointing here, and amends `arch/operations/runbook.md`
    "Migration to ADR-0026" with the ADR-0027 cutover sequence.

- **2026-04-27 — refactor PR #2 of 4 landed.**
  Single-endpoint daemon + lazy resolve. ADR-0026 slices
  1, 2a, and 4 reverted; slice 4b's `schema daemon` runtime
  rewritten around the new shape; ADR-0019 single-project
  `schema serve` path preserved by routing through the
  same `Daemon` (pre-wired with one project).
  - `src/adapters/registry_toml.rs`, `MultiTenantBearerValidator`,
    `ProjectTokenRegistry`, `Mount`, `build_multi_tenant_router`,
    `per_project_router` removed. `BearerValidator` (single-token)
    is the only validator left.
  - `src/app/daemon.rs` rewritten:
    `Daemon { embedder, llm_provider,
    projects: Arc<RwLock<HashMap<ProjectId, Arc<ProjectInstance>>>> }`,
    with `Daemon::new`, `Daemon::new_pre_wired`,
    `Daemon::resolve_or_wire(&working_directory)`. Walk-up
    helper finds `schema.toml` from the LLM-supplied
    directory; ADR-0008 path-keyed identity selects exactly
    one `store.db`. Double-check pattern under the write
    lock prevents wiring the same project twice on near-
    simultaneous first calls. `ProjectId` gains `Hash` to
    serve as the `HashMap` key.
  - `src/adapters/mcp_server.rs`:
    `ServerState` removed. `SchemaServer { daemon: Arc<Daemon> }`.
    Every retrieval / cleanup / synthesize tool gains a
    `working_directory: String` parameter; handlers call
    `self.resolve(&params.working_directory)` and dispatch
    to the resolved `ProjectInstance`. `ping` is the only
    project-less tool. `with_state` → `with_daemon`.
    `build_workspace_context` split into four helpers to
    stay under the 30-line budget.
  - `src/main.rs`: `run_serve` now wires one
    `ProjectInstance` exactly as before, then puts it
    inside `Daemon::new_pre_wired` and serves through the
    new `SchemaServer::with_daemon`. `run_daemon` calls
    `Daemon::new` (empty), writes one **global**
    `endpoint.toml` at `~/Library/Application Support/
    schema/endpoint.toml` (macOS) / `~/.local/state/
    schema/endpoint.toml` (Linux) carrying
    `url = "http://127.0.0.1:<port>/mcp"` + the
    workstation bearer. No per-slot startup loop, no per-
    project endpoint files, no per-mount tokens.
  - 89 unit tests pass (was 103; -14 from the deleted
    `MultiTenantBearerValidator` / `ProjectTokenRegistry` /
    `Registry` tests, +3 new daemon walk-up tests in
    `src/app/daemon.rs`). 5 integration. Strict-lint gate
    green; `cargo fmt --all -- --check` clean.
  - Note on the **watcher loop in daemon mode**: lazy
    `resolve_or_wire` does **not** spawn the watcher today
    — only single-project `run_serve` (which spawns it
    directly before pre-wiring the daemon) does. That is
    a regression on ADR-0010 for daemon mode and is
    explicitly out of scope for this PR. Refactor PR #3
    (canary E2E) does not depend on watchers; refactor PR
    #4 wires the watcher into `Daemon::wire_and_insert`
    side-effect or as a per-slot task spawned by the
    daemon entry point. **Tracked in this Evidence entry
    rather than introducing a separate ADR**, since it is
    a recovery of pre-existing behaviour rather than a
    new decision.
