---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2026-10-26
---

# 0026 — One shared daemon for all projects, with strict per-project isolation at query time

> **Y-statement** — In the context of ADR-0019 collapsing N
> Claude-Code-spawned `schema serve` processes into one HTTP server
> **per project** to share a single BGE-M3 ONNX session (~1.7 GB
> resident baseline) across N concurrent Claude Code sessions, with
> the operator-observed consequence that N projects on the same
> machine still pay N × ONNX baseline plus N × post-delta-sync heap
> retention (4 daemons currently resident at 1.5 / 1.5 / 4.1 / 17.3 GB
> per `vmmap --summary`, peak 21.3 GB on the largest corpus, never
> reclaimed; see Evidence below), facing the choice between (a)
> **status quo: one daemon per project** (cleanest isolation,
> linear-in-projects RAM cost, blocks "all my projects open at
> once" workflows on a 32 GB laptop), (b) **one shared daemon
> serving every project, with strict per-project isolation enforced
> at the request-routing and storage layers** (single ONNX session
> total, harder isolation contract, single point of failure), or
> (c) **shared daemon with a single combined index keyed by a
> `project_id` column** (rejected — see Option C), we tentatively
> lean toward **(b)**, against (a) (does not scale past 3-4 active
> projects on operator hardware) and (c) (a single missing predicate
> in any retrieval path leaks chunks across projects, violating
> ADR-0008 by software contract instead of by physical layout), to
> achieve **N projects served from one process with one ONNX session,
> while preserving ADR-0008's physical isolation guarantee (one
> `store.db` per project on disk) and adding a fitness function
> proving query results never cross project boundaries**, accepting
> **(i) the routing/auth complexity of a multi-tenant HTTP MCP
> server, (ii) the supersession of ADR-0019's per-project process
> model and the partial supersession of ADR-0020's per-project
> launchd / systemd unit, (iii) a single point of failure (panic
> takes all projects down), and (iv) a higher unit-test bar — every
> retrieval path must be exercised against a two-project fixture that
> fails the build if cross-bleed is observed**.

## Context and Problem Statement

ADR-0019 was the right answer for one motivating problem: two
Claude Code windows on the **same** project should not produce two
schema processes each loading its own ONNX session. It collapsed
N-clients-per-project into 1-server-per-project.

It did not address the second-order problem: an operator working on
M **different** projects in parallel still pays M × ONNX baseline
(M × 1.7 GB) plus M × post-delta-sync heap retention. Measured
2026-04-26, four daemons resident on the operator's box:

| project | corpus paths | chunks resident | RSS | peak |
|---------|-------------|-----------------|-----|------|
| `home` (mcp-schema arch/) | 3 | small | 1.5 GB | 1.7 GB |
| `lowcow-platform` | 13 | medium | 1.5 GB | 1.7 GB |
| `alloy-spec2` | 24 | large | 4.1 GB | 4.8 GB |
| `alloy-specs` | 29 | very large (policies + cue) | 17.3 GB | 21.3 GB |

`vmmap --summary` reports `Writable resident ≈ 97 %` on every
daemon — the heap holds onto pages it took during initial
delta-sync and never returns them to the OS. Peak ≈ current →
the daemons reach their high-water mark during embedding and stay
there for the rest of their lifetime. This is **retention**, not a
classic leak (no time-linear growth observed), but it scales with
corpus size and is multiplied by project count under ADR-0019.

A second concern motivates this ADR independently of footprint:
the operator routinely runs queries that should match one project
only. Project names overlap (`alloy-spec2` vs `alloy-specs`,
`lowcow-platform` vs hypothetical future `lowcow-*` repos) and
artifact identifiers overlap by convention (`ADR-0001` exists in
every project). Any future architecture that puts more than one
project's chunks in a single retrieval index must prove, by a
fitness function the build can run on every commit, that a query
issued in one project cannot return a chunk from another. ADR-0008
(per-project cache dir) currently delivers that guarantee by
**physical layout** — each project has its own `store.db` and there
is no code path that opens two of them. This ADR is about not losing
that guarantee while changing the process boundary.

## Decision Drivers

- **Single embedder, total.** ONNX baseline (~1.7 GB) paid once
  for the workstation, not once per project. Eliminates the
  M × baseline term entirely.
- **Physical isolation preserved.** ADR-0008's "one `store.db`
  per `project_id` keyed by hash of project root" is a
  load-bearing invariant. Whichever option is chosen, on disk
  each project still has its own store. No combined index.
- **Provable isolation at query time.** A fitness function in CI
  must demonstrate that a request bearing project A's auth token
  cannot return chunks from project B's store, **even if the daemon
  has both stores opened simultaneously**. This is the new contract
  this ADR introduces.
- **Lifecycle simplicity.** One launchd / systemd unit beats N
  units in `launchctl list` output and in operator mental model.
- **Failure containment.** A panic during one project's delta-sync
  must not crash the daemon for the other M-1 projects. Today
  ADR-0020's `KeepAlive=true` respawns one process; under shared
  daemon the equivalent must be **per-project task isolation**
  (Tokio task with `catch_unwind` boundary, or process-level
  `unwind=abort` + supervisor — see Open Questions).
- **Auth correctness.** ADR-0021's per-project bearer token must
  remain the unit of authentication. Under shared daemon, the
  token must also be the unit of authorization — i.e., it must
  identify which project the bearer is allowed to query, and the
  daemon must reject any request whose token does not match the
  project the URL/route declares.
- **Hexagonal preservation (ADR-0013).** This ADR changes the
  process boundary, not the port boundary. `Persistence`,
  `Embedder`, `Walker`, `Chunker`, `MetadataStore`, `Watcher`,
  `LlmProvider` ports stay; the wiring becomes "one composition
  root with a `Map<ProjectId, ProjectInstance>`" instead of
  "one composition root with one `ProjectInstance`".
- **Operator workflow scale.** The operator's roadmap puts at
  least 6 active projects under `~/dev/` within FASE 1.x. Linear
  RAM cost makes this unworkable on current hardware.

## Considered Options

### Option A — Status quo: one daemon per project (rejected)

Keep ADR-0019 as is. Each project gets its own
`schema serve --http`, its own ONNX session, its own watcher,
its own delta-sync, its own port and bearer token. ADR-0008
isolation is by physical layout; cross-bleed is impossible by
construction.

**Pros**: simplest mental model. Failure of one daemon is
fully contained. ADR-0019 / 0020 / 0021 stand unchanged.

**Cons**: M × ONNX baseline + M × post-delta-sync retention.
Measured today: four daemons = 24 GB resident. Six projects
extrapolated linearly = ~36 GB just for ONNX baselines + small
project heaps; ~50–60 GB if any of them carry an alloy-specs-class
corpus. Untenable on the operator's 32 GB workstation. Blocks the
"all my projects open at once" workflow that motivated ADR-0019 in
the first place (one level up).

### Option B — Shared daemon, strict per-project isolation (tentative pick)

One process: `schema daemon` (new verb, or repurposed
`schema serve` with no `--config`). Daemon owns:

- **One** BGE-M3 ONNX session (`Arc<dyn Embedder>`).
- **N** open `MetadataStore` + `VectorStore` instances, one per
  registered project, keyed by `project_id`. ADR-0008 path
  resolution unchanged — daemon opens
  `~/Library/Caches/schema/projects/<project_id>/store.db` for
  each registered project.
- **N** `DeltaSync` instances, one per project, each running its
  own initial sync at startup and its own watcher-driven
  incremental sync thereafter.
- **One** `notify`-backed watcher with N watch roots, demuxing
  events by path prefix to the matching `DeltaSync`.
- **One** `axum` router. Routing options:
  1. **URL path carries `project_id`** (e.g.,
     `POST /mcp/<project_id>` mounting one
     `StreamableHttpService` per project as a `nest_service`,
     each with its own server-side state and bearer-token layer).
  2. **Bearer token carries `project_id`** (token →
     `project_id` map maintained by daemon; rmcp service per
     request resolved via middleware that injects the project
     context into the handler). URL stays `/mcp` for all.
  3. **Hybrid**: token validates, URL pins. Mismatch = 403.
     Defence in depth.
- **N** `endpoint.toml` files (ADR-0021), one per project, each
  pointing at the **same** URL but each carrying that project's
  bearer token. The `.mcp.json` snippet rendered by
  `schema mcp-config` differs only in token. Consumers don't
  notice the daemon is shared.

**Project registration**: explicit. New verbs
`schema project register --config <path>` and
`schema project unregister --project-id <id>` write to a
daemon-local registry (`~/.local/state/schema/registry.toml`).
Daemon reads registry at startup; new projects can be hot-added
via a localhost `POST /admin/projects` (bearer-gated by an
admin token written at first daemon start, separate from
project tokens).

**Pros**: 1 × ONNX baseline total. ADR-0008 physical isolation
preserved (separate `store.db` per project on disk; cross-bleed
requires the daemon to deliberately open the wrong store, which
the routing layer prevents). One launchd / systemd unit.
Watcher is unified, walking N roots once each, no
M × file-walk.

**Cons**: routing / auth must be unit-tested with a two-project
fixture and an explicit "request with project A token must not
return project B chunks" assertion (the new fitness function).
Single point of failure: a panic in any project's path takes
all projects down unless explicitly contained. Lifecycle ADRs
(0019, 0020, 0021) need supersession or amendment. Memory
retention per project (the original symptom) still applies but
is now bounded by **the largest single project**, not the sum.

### Option C — Shared daemon with a single combined index keyed by `project_id` column (rejected)

One process, one `store.db`, every chunk row tagged with a
`project_id` column. Every retrieval query carries an implicit
`WHERE project_id = $bearer.project_id` predicate. Embeddings
share the same vector index; project filtering happens at SQL.

**Pros**: simplest schema, smallest disk footprint (no per-
project DB overhead), trivial cross-project search if ever
desired (already there, just remove the predicate).

**Cons (decisive)**: a single missing predicate, in any code
path, returns chunks from another project. ADR-0008's
guarantee becomes a **software contract** instead of a
**physical-layout** guarantee. Every retrieval port
(`query`, `find_decisions`, `glossary_lookup`,
`cross_reference`, `list_corpus`, `synthesize`'s retrieval
step, `forget_source`'s deletion path, `reset_index`'s scope)
must carry the predicate, plus every future retrieval port,
plus every test, plus every migration. ADR-0008 was written
specifically to **not** rely on this. Rejected on principle.

## Decision

**Option B accepted.** Direction is locked: one shared daemon,
N open `MetadataStore` + `VectorStore` instances keyed by
`project_id`, ADR-0008 physical isolation preserved on disk,
provable per-project isolation at query time enforced by the
fitness function below.

The **prototype gate** for code landing remains: no production
code under Option B may merge until the fitness function (the
two-project canary E2E) is green in CI. Acceptance of the
direction does **not** waive that gate; it commits the team to
that direction once the gate is met. Open questions below are
implementation choices, not direction choices.

Open questions deferred to the prototype stage:

- **Routing variant** (B.1 path-carrying vs B.2 token-carrying
  vs B.3 hybrid). Hybrid is most defensible but most code; pick
  after measuring rmcp's `nest_service` cost per project on
  router build.
- **Failure containment** (Tokio `catch_unwind` per task vs
  supervisor process spawning child workers per project).
  Tokio variant is cheaper but does not contain panics in
  `Send` futures across `await` points; supervisor variant
  recovers the failure isolation we lose by leaving Option A.
- **ONNX session sharing under load**: BGE-M3 via fastembed
  appears thread-safe (`Arc<TextEmbedding>` clonable across
  tasks per fastembed 5.x docs). Confirm under concurrent
  embed calls from N project DeltaSync instances at startup
  (the cold-start case stressed today).
- **Heap retention reduction**: Option B does not on its own
  fix the retention symptom this ADR opens with. A follow-up
  ADR-0027 will pick streaming-per-file vs explicit-drop +
  `madvise(MADV_FREE)` once Option B's prototype produces
  measurable per-project resident-after-sync numbers.
- **Project registration discoverability**: should
  `~/.local/state/schema/registry.toml` be operator-edited, or
  is the only path through `schema project register`? Default
  to "CLI-only, registry is internal state, not a config file"
  to avoid the failure modes of Option C-style implicit
  contracts.

## Consequences

### Positive

- One ONNX session for the workstation. Saves
  ~(M-1) × 1.7 GB resident once M ≥ 2.
- Fitness-function-tested isolation contract. The build
  proves cross-project bleed cannot occur, on every commit.
- One launchd / systemd unit. Cleaner operator dashboard.
- Watcher unified; per-project file-walk consolidated.

### Negative / trade-offs

- Single point of failure unless containment is explicit.
- Routing / auth complexity: the daemon now multiplexes,
  and every retrieval port must thread `project_id`
  through to the store lookup.
- Prototype + ADR-0027 (memory) work before Option B can
  flip from `proposed` to `accepted`.
- Supersedes process-shape parts of ADR-0019, ADR-0020,
  and ADR-0021. They become "atomic accept set" history;
  the new shape is one daemon, one unit, one URL with N
  routes (or N tokens), N stores on disk, N tokens
  outstanding.

### Follow-ups

- **Prototype** Option B's router on a two-project fixture
  (mcp-schema's own arch/ + a small synthetic project) and
  measure: (1) ONNX baseline residence stays at ~1.7 GB
  after both projects sync; (2) the fitness function below
  is automatable in CI; (3) panic in one project's chunker
  does not bring the daemon down.
- **ADR-0027** (memory budget post-delta-sync) — follow-up
  to address the retention symptom orthogonally to the
  daemon-shape decision. Will only become load-bearing once
  Option B confirmed; until then, Option A's per-process
  retention remains an operator concern.
- **README.md index** — entry added under "proposed" once
  this ADR lands; status flips on acceptance.

## Fitness function

The defining contract this ADR introduces is **provable
per-project isolation at query time**. The fitness function
is therefore an integration test, run on every commit, that
fails the build on cross-project bleed.

Concretely, on the prototype branch:

- **Two-project fixture.** Spin up the shared daemon with
  two registered projects, `alpha` and `beta`. Each has one
  ADR with the **same** filename (`docs/decisions/0001-x.md`)
  and a **distinguishable** body (`alpha` ADR contains the
  literal token `ALPHA-CANARY`, `beta` ADR contains
  `BETA-CANARY`).
- **Two clients.** Issue an MCP `query` for the natural-
  language prompt "what does ADR 0001 say" with project
  `alpha`'s bearer token, then again with project `beta`'s
  token.
- **Assertion.** The `alpha` response **must** contain
  `ALPHA-CANARY` and **must not** contain `BETA-CANARY`.
  Symmetric for `beta`. Repeat for every retrieval port
  exposed by the MCP surface (`query`, `find_decisions`,
  `glossary_lookup`, `cross_reference`, `list_corpus`,
  `synthesize` retrieval step). Mark a port `isolation-
  exempt` only with an explicit ADR-amendment justifying
  why (none envisaged today).
- **Negative authorization assertion.** A request to
  `/mcp/<beta_project_id>` carrying `alpha`'s bearer must
  return 403 (or an MCP-level auth error), regardless of
  payload. Mismatched URL/token combinations are the most
  likely source of accidental bleed in B.3 (hybrid)
  routing.
- **Watcher demux assertion.** Modify a file under
  `alpha`'s root; the daemon's `DeltaSync` for `beta` must
  observe zero events (assert via injected metrics
  port — count of `BetaSync.handle_event` calls).

Secondary fitness, runtime:

- **One ONNX baseline.** After both projects' initial
  delta-sync completes, daemon `Physical footprint`
  reported by `vmmap --summary` is within 1.10 × the
  single-project ONNX baseline + Σ per-project chunk-cache
  budgets. Today's per-project chunk cache is unbounded
  (the symptom in this ADR's preamble); ADR-0027 will
  define that budget.

CI integration follows ADR-0024 (E2E pytest) — the two-
project fixture lives under `tests/e2e/`, drives the
HTTP MCP surface end-to-end, and runs in the existing
e2e job.

## Cross-references and follow-ups

- **ADR-0008 — per-project cache isolation.** Strictly
  preserved. The shared daemon opens N `store.db` files,
  one per project, at the paths ADR-0008 already defines.
  No combined index.
- **ADR-0019 — Streamable HTTP transport.** Process-shape
  decision (one server per project) is the part this ADR
  proposes to supersede. Transport choice (Streamable HTTP
  via rmcp + axum) and session model (`LocalSessionManager`,
  `with_stateful_mode(true)`, localhost-only allowed hosts)
  carry over unchanged.
- **ADR-0020 — service permanent lifecycle.** Per-project
  launchd / systemd unit becomes one shared unit. Templates
  and verbs change shape. `schema install --service`
  becomes a workstation-level install (idempotent, registers
  the daemon once). Project registration moves to
  `schema project register`. Amendments needed.
- **ADR-0021 — localhost bind + bearer auth.** Auth model
  retained — bearer token per project — but the validator
  is now a multi-tenant `Map<Token, ProjectId>` instead of
  a single-token `eq` check. Token rotation, secrecy, and
  0600 file mode unchanged.
- **ADR-0007 — delta-sync at startup.** Per-project
  delta-sync is per-project still; the daemon runs N
  initial syncs at startup (sequential or pooled, see
  prototype open question).
- **ADR-0011 — sqlite-vec store.** Per-project. ADR-0008
  paths used for each project's store. WAL concurrency is
  per-store; opening N stores is functionally identical to
  N processes opening one each.
- **ADR-0013 — hexagonal architecture.** Preserved. The
  composition root grows a `Map<ProjectId, ProjectInstance>`;
  the ports inside each `ProjectInstance` are the same
  ports as today.
- **Follow-up ADR-0027 (planned)** — memory budget
  post-delta-sync. Not blocked by this one; either ADR can
  land first. Together they answer the question "how much
  RAM does the workstation pay for N projects".

## Evidence and amendments

- **2026-04-26 — accepted.** Direction locked on Option B
  (one shared daemon, N stores opened by `project_id`,
  fitness function as the merge gate). Amendments landed
  in ADR-0019 (process-shape superseded; transport carries
  over), ADR-0020 (lifecycle moves to workstation-level
  unit + `schema project register` verb), and ADR-0021
  (auth validator becomes multi-tenant `Map<Token,
  ProjectId>`). Migration plan appended to
  `arch/operations/runbook.md` under the new
  "Migration to ADR-0026 (shared daemon)" section.
  Implementation pending the prototype + fitness function
  gate; no production code yet on the shared-daemon path.

- **2026-04-26 — slice 1 + 2a landed (PR #1, merged as
  `bdc4616` on `main`).** Additive scaffolding for the
  shared-daemon path; no production runtime touched.
  - `src/adapters/auth.rs`: new `MultiTenantBearerValidator`
    + `ProjectTokenRegistry`. The validator resolves
    `bearer → ProjectId` via the shared registry and
    injects the resolved id into the request's
    `extensions_mut()` so the eventual router can
    dispatch by project. Lock poisoning recovers via
    `PoisonError::into_inner` (registry holds only owned
    `String` / `ProjectId` values; recovery is strictly
    safer than crashing the daemon for every other
    registered project). The single-token
    `BearerValidator` is **retained unchanged** for the
    ADR-0019 / ADR-0020 per-project daemon path that
    ships in production today.
  - `src/adapters/registry_toml.rs`: `Registry` /
    `ProjectEntry` reader/writer. Path conventions per
    ADR-0026 amendment to ADR-0020 (`~/Library/Application
    Support/schema/registry.toml` on macOS,
    `~/.local/state/schema/registry.toml` on Linux).
    `load` treats missing-file as empty (fresh-install
    case). `save_atomic` writes via tempfile + rename at
    the default `0644` (file holds paths and ids only —
    bearer tokens stay in per-project `endpoint.toml` at
    `0600`, ADR-0021).
  - 14 unit tests added (97 → 105 in `cargo test
    --all-features`). `cargo clippy --all-features
    --all-targets --workspace -- -D warnings` exits 0;
    `cargo fmt --all -- --check` clean.

- **2026-04-26 — slice 3 landed (PR #2, merged as
  `55f1f37` on `main`).** Operator-facing CLI surface for
  the registry. Filesystem-only (the daemon picks up
  changes on next restart; hot-reload via a localhost
  admin endpoint is a follow-up slice; production runtime
  unchanged).
  - `src/cli/project.rs`: `register / unregister / list`
    verbs operating on `Registry::default_path()`.
    `register` resolves identity from the consumer's
    `schema.toml`, canonicalises the absolute path,
    upserts, saves; re-register replaces in place
    (preserves order, rotates `registered_at`).
    `unregister` is idempotent — missing `project_id` is
    a non-fatal warning at exit code 0. `list` prints in
    insertion order; empty registry message is explicit.
  - `src/main.rs`: `project` subcommand wired with three
    sub-verbs; `Registry::default_path()` resolved once
    per invocation so the daemon and the CLI agree on
    the registry location without an extra flag.
  - 7 unit tests added (105 → 112). Manual smoke:
    `schema project list` on a fresh box prints
    `no projects registered (~/Library/Application
    Support/schema/registry.toml)`; `schema project
    --help` lists the three verbs. Strict-lint gate
    green; `cargo fmt --all -- --check` clean.

- **2026-04-26 — slice 2b landed (PR #3, this commit).**
  Composition-root refactor: `Wiring + Services` (private
  to `main.rs`) replaced with a single named, file-resident
  `ProjectInstance` in `src/app/project_instance.rs`.
  - `ProjectInstance` aggregates per-project ports
    (`Persistence`, `MetadataStore`, `Walker`, `Chunker`)
    and per-project app-layer use cases (`DeltaSync`,
    `Query`, `Cleanup`, optional `Synthesize`). Hexagonal
    placement: application-layer (`src/app/`); domain
    untouched. ADR-0013 dependency rule preserved
    (`adapters → application → domain`).
  - `ProjectInstance::wire(config, identity, &embedder,
    llm_provider)` accepts the embedder by reference, so
    the same factory works in single-project mode
    (`run_serve` builds one `Embedder` and passes it in)
    and in shared-daemon mode (a future `Daemon` will
    build one `Embedder` and pass the **same** `Arc`
    clone to every registered project). The factory
    splits into `WiredPorts::open` and
    `WiredUseCases::build` to keep each function under
    the 30-line cognitive budget.
  - `main.rs`: `Wiring`, `Services`, `wire_ports`,
    `build_services`, `build_synthesize` removed;
    `wire_serve_services` rebuilt on
    `ProjectInstance::wire`. `build_cleanup` (used by
    `schema reset` / `schema forget`) inlines a
    persistence + metadata wiring directly to **avoid**
    paying the ~2 GB ONNX load cost a `ProjectInstance`
    would imply for a one-shot cleanup.
  - Manual `Debug` impl on `ProjectInstance` (the field
    types include `Arc<dyn Trait>` whose traits do not
    require `Debug`); `finish_non_exhaustive` documents
    the partial display as intentional.
  - 107 unit tests still passing — refactor preserves
    all FASE 1.0 behaviour. Strict-lint gate green;
    `cargo fmt --all -- --check` clean.

- **2026-04-26 — fitness-function gate status.**
  Implementation slices 1, 2a, 3, 2b have all landed
  additively or as preserving refactors. The shared
  daemon's runtime path (slice 4 — `Daemon` aggregator
  + multi-tenant router; slice 6 — canary E2E
  fitness function) is **not yet shipped**, so the
  per-project ADR-0019 / ADR-0020 deployment remains
  the production model. Cutover (per
  `arch/operations/runbook.md` "Migration to ADR-0026")
  is gated on slice 6 turning green in CI.

- **2026-04-26 — slice 4 landed (PR #4, this commit).**
  `Daemon` aggregator + multi-tenant `axum` router
  shipped in `src/app/daemon.rs` + `src/adapters/mcp_server.rs`.
  Per-request runtime not wired yet (see follow-up
  list); the routing topology and isolation contract
  are in place.
  - `src/adapters/mcp_server.rs`: `Mount` struct +
    `build_multi_tenant_router(mounts, &cancellation)`
    helper. One `nest_service("/mcp/<project_id>", …)`
    per project (B.1 routing per ADR-0026 §"Decision");
    each mount gated by a single-token
    [`crate::adapters::auth::BearerValidator`] tied to
    that project's bearer. Empty `mounts` produces a
    `/health`-only router. `build_router` (single-tenant
    ADR-0019 path) preserved untouched.
  - `src/app/daemon.rs`: `Daemon` struct + `Daemon::wire`
    factory taking a `Registry`, the shared `Embedder`,
    and the optional shared `LlmProvider`. For each
    `ProjectEntry`: resolves config + identity, calls
    `ProjectInstance::wire` with the **shared**
    `Embedder` (`Arc::clone`), mints a fresh UUIDv4
    bearer, builds the per-project `SchemaServer`. The
    `Daemon::into_router` consumer-by-value method moves
    every slot's server into a `Mount` and delegates to
    `build_multi_tenant_router`. Sequential project
    wiring; parallel wiring deferred until ONNX session
    contention is measured (open question in §"Open
    questions").
  - **Routing-vs-validator decision recorded in module
    docs of `src/app/daemon.rs`.** ADR-0021 amendment
    by ADR-0026 sketches a `Map<TokenHash, ProjectId>`
    validator. Slice 4 ships a stricter variant:
    single-token `BearerValidator` per mount, each
    accepting **only** that project's token. Effect is
    the same — bearer-A on `/mcp/<project_b_id>` returns
    401 — but isolation is by routing construction
    instead of by predicate. `MultiTenantBearerValidator`
    + `ProjectTokenRegistry` (slice 1) remain in tree
    as the daemon's introspection surface (token
    rotation, future hot-reload admin endpoint), not as
    the per-request gate. The ADR-0021 amendment text
    still applies to any future request path that does
    NOT use B.1 routing.
  - 3 unit tests added (107 → 110): empty-daemon
    `/health`, empty-daemon 404 on `/mcp/...`, and
    `len`/`is_empty` reflecting an empty registry.
    Strict-lint gate green; `cargo fmt --all -- --check`
    clean. Real-`Embedder` paths exercised end-to-end in
    slice 6's canary E2E (which gates merge of the
    cutover commit).
  - Follow-ups still pending before cutover:
    - **Slice 4b**: `schema daemon` CLI verb, HTTP
      listener wiring, per-project `endpoint.toml`
      writes pointing at the shared URL with the
      project's token, watcher loop per slot.
    - **Slice 5**: launchd / systemd template collapse
      — `{project_id}` slot removal, workstation-level
      unit per ADR-0020 amendment.
    - **Slice 6**: `tests/e2e/test_adr0026_isolation.py`
      canary fitness function (ALPHA-CANARY /
      BETA-CANARY) — the **merge gate** for any commit
      that flips the operator-facing path from ADR-0019
      per-project to ADR-0026 shared.

- **2026-04-26 — slice 4b landed (PR #5, this commit).**
  `schema daemon` CLI verb + runtime entry point. The
  shared daemon now runs end-to-end against a registry,
  but the operator-facing deployment still defaults to
  ADR-0019 per-project units until slice 5 (template
  collapse) and slice 6 (canary E2E gate) ship.
  - `src/main.rs`: new `daemon` subcommand (no
    `--config` flag — registry is the source of truth
    per ADR-0026 amendment to ADR-0020). `run_daemon`
    loads the registry from `Registry::default_path()`,
    builds **one** `FastembedEmbedder` (BGE-M3 ONNX
    session — the ~1.7 GB workstation baseline ADR-0026
    is built around), resolves the LLM provider from
    env (`SCHEMA_LLM_PROVIDER` / `SCHEMA_LLM_MODEL` /
    `*_API_KEY`), and calls `Daemon::wire`. An empty
    registry logs a warning but still serves
    `/health`-only — operators can register projects
    after start-up, and a future hot-reload admin
    endpoint (slice 4c) will pick them up without a
    restart.
  - `serve_daemon` binds `127.0.0.1:0`, runs per-slot
    startup side effects, builds the multi-tenant
    router, runs `axum::serve` with the shared
    `CancellationToken` (one SIGTERM drains every
    in-flight session in parallel), then unlinks every
    `endpoint.toml` it wrote.
  - `startup_each_slot` for each `ProjectSlot`: runs
    initial delta-sync, spawns the per-project
    `NotifyWatcher` (ADR-0010), and writes the
    project's `endpoint.toml` at
    `~/.cache/schema/projects/<project_id>/
    endpoint.toml` with `url = "http://127.0.0.1:<port>
    /mcp/<project_id>"` and `token = <slot's UUIDv4
    bearer>`. The URL **includes** the project's path
    segment — consumers can paste it directly into
    `.mcp.json` (ADR-0021 fitness function: file mode
    is still `0600`, written via tempfile + rename).
  - `resolve_llm_provider_for_daemon` reads
    `SCHEMA_LLM_PROVIDER` / `SCHEMA_LLM_MODEL` from
    env (the registry has no `[llm]` section — that
    knob is per-project today and would conflict if
    two projects pinned different providers; the
    daemon's shared provider is workstation-level by
    construction). Default is `auto`, matching the
    single-project resolver.
  - `cli/install.rs`, `cli/templates/launchd.plist
    .template`, and `cli/templates/systemd.service
    .template` are **untouched** in this slice. They
    still render per-project units. Slice 5 collapses
    them to a single workstation-level unit invoking
    `schema daemon`.
  - 110 unit tests still pass (no behavioural drift
    in single-project paths). Manual smoke:
    `schema daemon --help` prints the new verb's
    description; `schema --help` lists `daemon`
    alongside `serve` / `validate` / `project` / etc.
    Strict-lint gate green; `cargo fmt --all --
    --check` clean.
  - Open work blocking cutover:
    - **Slice 4c (deferred)**: hot-reload admin
      endpoint at `POST /admin/projects/refresh` so
      `schema project register / unregister` apply
      without a daemon restart.
    - **Slice 5**: template collapse — workstation-
      level launchd plist / systemd unit invoking
      `schema daemon`, no per-project `{project_id}`
      slot.
    - **Slice 6**: canary E2E (`tests/e2e/
      test_adr0026_isolation.py`). With slice 4b
      shipped, the test fixture can finally spin up
      the real daemon binary and assert ALPHA-CANARY /
      BETA-CANARY isolation against
      `/mcp/<project_a_id>` and `/mcp/<project_b_id>`.

- **2026-04-26 — diagnostic that motivated this ADR.**
  Four daemons resident on the operator's box per
  `vmmap --summary`:

  | project | corpus paths | RSS | peak | writable resident |
  |---------|-------------|-----|------|-------------------|
  | `home` | 3 | 1.5 GB | 1.7 GB | 1.7 GB |
  | `lowcow-platform` | 13 | 1.5 GB | 1.7 GB | 1.7 GB |
  | `alloy-spec2` | 24 | 4.1 GB | 4.8 GB | 4.4 GB |
  | `alloy-specs` | 29 | 17.3 GB | 21.3 GB | 17.6 GB |

  Total resident across the four daemons: ~24 GB,
  trending toward laptop-OOM at 5+ projects. Operator
  flagged that names overlap (`alloy-spec2` /
  `alloy-specs`, `lowcow-platform` / future `lowcow-*`)
  and motivated the "isolation must be provable, not
  trusted" framing.
