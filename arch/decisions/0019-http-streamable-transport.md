---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
supersedes: ["ADR-0002"]
---

# 0019 — MCP transport: Streamable HTTP via `rmcp` 1.5 + `axum` 0.8 (supersedes ADR-0002)

> **Y-statement** — In the context of `schema serve` running as one
> process per Claude Code session because ADR-0002's stdio transport
> implies a 1:1 client-server mapping, with the operator-observed
> consequence that two concurrent Claude Code sessions on the same
> consumer project produce two `schema serve` processes each loading
> the ~2 GB BGE-M3 ONNX session and each running its own watcher,
> delta-sync, and SQLite writer against a single per-project store
> (saturating CPU at multiples of 600 % and exceeding 9 GiB resident
> per process), facing the choice between (a) **leader/follower with
> `fd_lock` advisory locking** (coordinates duplicates after the fact;
> still spawns N processes, each holding ONNX session in RAM unless
> we add lazy-load and IPC), (b) **central daemon + custom IPC**
> (clean isolation but a multi-week re-architecture, FASE 2 scope),
> (c) **isolated per-session caches** (drops shared index, multiplies
> disk and re-embed work — strictly worse), (d) **auto-spawn from the
> first connecting MCP client** (lifecycle entangled with MCP client
> behaviour, idle-exit/reconnect races, no parent for stderr), or (e)
> **MCP Streamable HTTP transport via `rmcp` 1.5 + `axum` 0.8, one
> server process per project, N Claude Code sessions connecting as
> HTTP clients**, we decided for **(e)**, against (a) (FASE 1.1 plan
> but does not solve the duplicated-embedder cost), (b) (premature
> for a single-developer tool), (c) (anti-solution), and (d)
> (lifecycle should not be a function of MCP client behaviour), to
> achieve **single embedder, single watcher, single delta-sync per
> project regardless of how many Claude Code sessions are open**,
> accepting **a service-lifecycle obligation (ADR-0020), a
> localhost-bound auth scheme (ADR-0021), and the supersession of
> ADR-0002's stdio default**.

## Context and Problem Statement

ADR-0002 picked stdio because it is the simplest MCP transport:
Claude Code spawns the binary, talks JSON-RPC over stdin/stdout, the
binary exits when stdio closes. This works perfectly for one client.

In April 2026 the operator reported sustained CPU saturation while
working with two Claude Code windows open on the same consumer
project (operator-private path; consumer name not embedded in this
public ADR). Diagnostic capture:

| pid    | started | %CPU  | RSS      | command                                                |
|--------|---------|-------|----------|--------------------------------------------------------|
| 17909  | 12:40   | 592 % | 9.9 GiB  | `/usr/local/bin/schema serve --config <project>/schema.toml` |
| 59216  | 14:43   | 603 % | 9.7 GiB  | (same command, different Claude Code session)         |

Both processes have:

- their own `FastembedEmbedder` ONNX session (~2 GB resident each)
- their own `NotifyWatcher` re-walking the corpus on every save
- their own `DeltaSync` re-hashing every file (pre-ADR-0017)
- competing writes to the same `~/.cache/schema/projects/<id>/store.db`
  (WAL handles correctness, ADR-0011 §Concurrency, but does not
  prevent the duplicated work)

ADR-0008 §Concurrency (line 42) anticipated this: *"two `schema`
instances on the same project must coordinate"*. The proposed FASE 1.1
solution was an `fd-lock` around the delta-sync write phase. That
solves correctness, not the duplicated embedder cost.

The MCP spec (revision 2025-06-18) ships **Streamable HTTP** as the
canonical HTTP transport for servers, deprecating SSE. `rmcp` 1.5
implements `transport-streamable-http-server` exposing a
`tower::Service`. Verified against upstream:

- `cargo info rmcp` confirms feature `transport-streamable-http-server`
  (depends on `transport-streamable-http-server-session`,
  `server-side-http`, and `transport-worker`).
- `crates/rmcp/src/transport/streamable_http_server/tower.rs` (1235
  lines, upstream `modelcontextprotocol/rust-sdk`,
  `git rev fffe138` as of decision date):
  - line 462: `pub struct StreamableHttpService<S, M> { ... }`
  - line 486: `impl<RequestBody, S, M> tower_service::Service<Request<RequestBody>> for StreamableHttpService<S, M>`

HTTP transport collapses the multi-process problem by construction:
one server, N clients.

## Decision Drivers

- **Single shared embedder.** ~2 GB resident for the ONNX session
  must be paid once per project, not once per Claude Code window.
- **Single shared watcher and delta-sync.** No multiplexing of
  re-walks across processes.
- **MCP-spec-aligned.** Streamable HTTP is the current canonical
  HTTP transport (post-2025-06-18). SSE is deprecated.
- **Standard tooling.** `axum` 0.8 + `tower` 0.5 are the de-facto
  Rust async HTTP stack; the binary already depends transitively on
  `tokio`, `hyper`, and `bytes` via `rmcp` server-side feature.
- **Hexagonal preservation (ADR-0013).** The transport is an adapter
  concern; the existing `Persistence`, `Embedder`, `Walker`,
  `Chunker`, `MetadataStore` ports are untouched.

## Considered Options

### Option A — Stay on stdio + add `fd_lock` leader/follower (rejected)

ADR-0008 FASE 1.1 plan. Coordinates duplicate processes correctly
but every additional process still loads a fresh ONNX session unless
we also build a lazy-embedder + IPC bridge (effectively becoming
Option B but worse). RAM cost stays linear in process count.

### Option B — Custom daemon + IPC (deferred to FASE 2)

Clean architectural answer: `schema-daemon` owns the embedder,
watcher, store; `schema serve` becomes a thin MCP shim talking to
the daemon over Unix socket. Cost: re-architect the binary boundary,
introduce IPC versioning, lifecycle, replay-after-crash. Right
shape for a future scale jump; wrong cost for a single-developer
tool today.

### Option C — Isolated cache per Claude Code session (rejected)

Each session gets its own `~/.cache/schema/sessions/<session_id>/`.
Removes contention by removing sharing. Multiplies disk usage by N,
multiplies cold-start re-embed time by N, removes the whole point
of having a shared corpus index. Anti-solution.

### Option D — Auto-spawn from first MCP client (rejected)

The first Claude Code session that fails to find a running server
on the recorded port spawns one in the background. Issues:

- stderr is captured by no parent; logs vanish.
- Two simultaneous spawns race (sessions opened within
  milliseconds of each other).
- Idle-exit policy collides with reconnect-on-restart in MCP clients.
- Crashes lose the server until the next client connects.

OS-supervised lifecycle (ADR-0020) is the right shape for a
durable server.

### Option E — Streamable HTTP via rmcp 1.5 + axum 0.8 (chosen)

Single `schema serve --http` per project listens on
`127.0.0.1:<kernel-assigned-port>`. Claude Code sessions connect as
MCP HTTP clients. Verified end-to-end:

- `rmcp` 1.5 feature `transport-streamable-http-server` exposes
  `rmcp::transport::streamable_http_server::tower::StreamableHttpService`
  as a `tower_service::Service<Request<RequestBody>>` (citation
  above).
- `axum` 0.8 mounts arbitrary `tower::Service`s via
  `axum::routing::any_service`. axum 0.8 is the current stable
  (0.8.9 as of decision date) and pairs with `tower-http` 0.6
  natively (axum 0.8.x's own `Cargo.toml` lists
  `tower-http = "0.6.8"`).
- The Streamable HTTP service handles MCP JSON-RPC framing,
  session management (stateful mode, init handshake, session id
  header `mcp-session-id`), DNS-rebinding protection
  (`allowed_hosts`), and `Origin` validation, per the spec.

stdio transport is **dropped** from the binary; the
`transport-io` `rmcp` feature is removed from `Cargo.toml`. CLI
verb `schema serve` remains; its only effect is "start the HTTP
MCP server on the kernel-assigned port and write
`endpoint.toml`". Discovery on the client side (Claude Code's
`.mcp.json`) reads the URL/token from
`~/.cache/schema/projects/<id>/endpoint.toml` (auth + discovery
schema authoritative in ADR-0021; install-time wiring in ADR-0020).

## Decision Outcome

**Option E — MCP Streamable HTTP via `rmcp` 1.5 + `axum` 0.8.**

### Stack additions

```toml
# Cargo.toml deltas

# Update rmcp features:
rmcp = { version = "1.5", features = [
    "server",
    "macros",
    "transport-streamable-http-server",   # new
    # "transport-io",                      # removed
] }

# New direct deps (versions verified to pair: axum 0.8 ships with
# tower 0.5 + tower-http 0.6 in its own Cargo.toml):
axum       = "0.8"
tower      = "0.5"
tower-http = { version = "0.6", features = ["auth", "trace", "sensitive-headers"] }
uuid       = { version = "1", features = ["v4"] }
```

`tokio` features stay `["full", "test-util"]` — the multi-thread
runtime is required to run the HTTP server alongside watcher,
embedder (`spawn_blocking`), and SQLite writer (`spawn_blocking`).

### Server topology

One `schema` process per project. The process:

1. loads `schema.toml`, resolves `ProjectIdentity`;
2. wires `Persistence`, `Embedder`, `Walker`, `Chunker`,
   `MetadataStore` (unchanged from today);
3. constructs the `SchemaServer` (rmcp tool router) — unchanged from
   today;
4. wraps it in `StreamableHttpService` via
   `rmcp::transport::streamable_http_server::tower::StreamableHttpService::new(...)`;
5. mounts the service on an `axum::Router` at path `/mcp`, with a
   `/health` GET handler returning `200 OK`;
6. binds `TcpListener` on `127.0.0.1:0` (kernel chooses port), reads
   the assigned port back, writes
   `~/.cache/schema/projects/<id>/endpoint.toml` (perms 600) with
   `url = "http://127.0.0.1:<port>"` and the bearer token from
   ADR-0021;
7. calls `axum::serve(listener, router)` with graceful shutdown on
   SIGTERM (handled in ADR-0020).

### What goes away

- The stdio transport path in `main.rs` (`SchemaServer::run_stdio`).
- The `rmcp` `transport-io` feature in `Cargo.toml`.
- The implicit "spawn one process per Claude Code window"
  consumer-side mental model. The new `.mcp.json` for consumers
  (lowcow-platform documents) points at an HTTP URL read from
  `endpoint.toml`.

### What stays

- All hexagonal ports (ADR-0013).
- All MCP tools (vector_search, find_by_artifact_id, find_mentioning,
  list_sources, reset, forget) — they live in `SchemaServer` and are
  agnostic to the transport.
- ADR-0008 cache layout (`~/.cache/schema/projects/<id>/`).
- ADR-0011 SQLite + sqlite-vec store.
- ADR-0014 install at `/usr/local/bin` + Apple codesign on macOS;
  binary contents change but install path and codesign procedure
  remain (additional install verbs come from ADR-0020).

## Consequences

- **Good:** N Claude Code sessions on one project incur exactly one
  embedder load, one watcher, one delta-sync — eliminating the
  operator-reported double-CPU saturation.
- **Good:** the binary is debuggable from the shell:
  `curl -N -H "Authorization: Bearer $TOKEN" http://127.0.0.1:$PORT/mcp`
  reproduces what Claude Code does.
- **Good:** the MCP spec direction (Streamable HTTP) is honored; SSE
  is not introduced as a temporary measure.
- **Good:** `axum` + `tower-http` add structured request tracing,
  deadline propagation, and rate-limit primitives if we ever need
  them.
- **Neutral:** binary size grows by ~5 MB stripped (axum + tower +
  hyper-util pulled in directly rather than transitively). Build
  time grows by ~30 s on a clean release build.
- **Bad:** lifecycle is now an explicit operator concern.
  Claude Code no longer auto-spawns the server, so the server must
  be started separately. ADR-0020 addresses this with launchd /
  systemd integration.
- **Bad:** localhost network surface where there was none. ADR-0021
  adds bearer-token auth.
- **Bad:** ADR-0002 is superseded — the project's first transport
  decision is being walked back. Documented honestly here; the
  motivating workload (multiple concurrent sessions) was not visible
  at ADR-0002 acceptance time.

## Fitness function

- **Unit test (router shape only):** `axum::Router` construction
  in `src/adapters/mcp_server.rs::build_router` returns a `Router`
  that registers `/mcp` (POST + chunked-response method router)
  and `/health` (GET); verified by `Router::routes_count` /
  recognised path lookup. *Does not* attempt protocol calls — a
  oneshot GET against the Streamable HTTP service returns 405/406
  trivially and is not a meaningful test of the MCP path.
- **Integration test (initialize handshake):** start the server on
  `127.0.0.1:0`, read the port, POST an MCP `initialize` JSON-RPC
  envelope to `/mcp` with a valid bearer token; assert the response
  body contains a `serverInfo` block matching `name = "schema"`
  and that the response carries a non-empty `mcp-session-id`
  header. This is the smallest test that actually exercises
  `StreamableHttpService`.
- **Integration test (concurrent clients):** start the server,
  spawn two HTTP MCP clients in parallel each completing a real
  `initialize` + 10 `vector_search` calls; assert all 20 calls
  succeed and the embedder initialisation appears exactly once
  in the captured `tracing` output (`fastembed.try_new` span
  fires once per process lifetime).
- **End-to-end smoke (manual on operator's box):** with all three
  ADRs (0019, 0020, 0021) applied, `claude mcp list` lists the
  schema MCP server as `connected` from a fresh Claude Code
  session opened on the consumer project. Documented in
  `arch/operations/` runbook.
- **CI lint:** `cargo build --release` in
  `mcp-schema/.github/workflows/lint.yml` removes the
  `transport-io` feature from the build matrix; the binary cannot
  be run with `--stdio`.

### Atomic accept set

ADR-0019, ADR-0020, and ADR-0021 form an **atomic accept set**.
Merging any one of the three without the others leaves the system
in a broken state:

- ADR-0019 alone: an HTTP server with no lifecycle and no auth.
- ADR-0019 + 0020 without 0021: a long-running localhost server
  with no auth — wrong default for a multi-user dev box.
- ADR-0019 + 0021 without 0020: a server that has to be started
  manually and disappears on every reboot.

The supersession of ADR-0002 (stdio) is therefore conditional on
all three ADRs landing together. If any one stalls, ADR-0002
remains the live transport decision.

## Operator migration runbook (out-of-band)

A one-shot migration script outline is added to
`arch/operations/0019-stdio-to-http-migration.md` on acceptance.
Outline:

1. `launchctl unload` / `systemctl --user stop` any existing
   per-project `schema serve` (none, today — sessions spawn
   directly).
2. `kill` any running `schema serve` processes that came from
   stdio Claude Code sessions; close those Claude Code windows.
3. `cargo build --release && codesign ...; sudo install ...`
   (ADR-0014 procedure, unchanged).
4. For each consumer project:
   `schema install --service --config /path/to/schema.toml`.
5. For each consumer project: regenerate `.mcp.json`
   (`schema mcp-config --config /path/to/schema.toml > .mcp.json.fragment`),
   merge into the consumer's `.mcp.json`.
6. Reopen Claude Code; `claude mcp list` confirms `connected`.

## Cross-references and follow-ups

- **ADR-0002 — rmcp stdio.** Will be marked
  `status: superseded by ADR-0019` when this ADR is accepted; until
  then the supersede edge is documented here.
- **`arch/operations/`.** New runbook
  `arch/operations/0019-stdio-to-http-migration.md` written alongside
  the implementation, covering the migration steps above and the
  recovery path when Claude Code cannot reach the server.
- **ADR-0008 — cache isolation.** `endpoint.toml` is added to the
  project cache layout. ADR-0008 frontmatter gets an
  `Evidence and amendments` entry on acceptance.
- **ADR-0011 — SQLite + sqlite-vec.** Unchanged; WAL still covers
  multi-reader during writer.
- **ADR-0013 — hexagonal.** Transport is an adapter; ports unchanged.
- **ADR-0014 — install + codesign.** Binary contents change; install
  path and codesign procedure unchanged (ADR-0020 adds new install
  verbs).
- **ADR-0015 — cleanup tools.** `schema reset` / `schema forget`
  remain CLI verbs; HTTP equivalents on the MCP surface are tools
  already.
- **ADR-0017 — mtime-size short-circuit.** Independent; both apply
  to the surviving single delta-sync.
- **ADR-0018 — cap ONNX threads.** Independent; caps the surviving
  single embedder.
- **ADR-0020 — service permanent (proposed).** Solves the lifecycle
  obligation introduced by this ADR.
- **ADR-0021 — localhost bearer auth (proposed).** Solves the
  network surface introduced by this ADR.
- **ADR-0022 — tokio-uring evaluation (proposed).** Independent
  bench-driven optimisation of the surviving single delta-sync.

### Why not SSE

The MCP spec (2025-06-18) lists Streamable HTTP as the canonical
HTTP transport and marks SSE as deprecated. `rmcp` 1.5's server-side
SSE support is implicit through the `sse-stream` dep used internally
by Streamable HTTP for streaming responses; there is no public
"SSE-only" server transport in 1.5 (verified in
`crates/rmcp/src/transport/`). One transport with one connection
per session is simpler than SSE's two-connection model, debuggable
with `curl -N`, and aligned with where the spec is going.

### Why not auto-spawn

Auto-spawn (the first Claude Code that connects starts the server)
was considered. Failure mode: idle exit conflicts with reconnect on
session restart; race conditions on simultaneous spawn from two
sessions; stderr capture is unowned (no launchd/systemd parent).
Operator-managed launchd / systemd service (ADR-0020) is simpler to
reason about.

## Evidence and amendments

- **2026-04-26 — implemented.** Cargo.toml updated:
  `rmcp` `transport-io` removed, `transport-streamable-http-server`
  added; new direct deps `axum = "0.8"`, `tower = "0.5"`,
  `tower-http = "0.6"` (features `validate-request`, `trace`,
  `sensitive-headers`), `tokio-util = "0.7"`, `http = "1"`,
  `bytes = "1"`, `http-body-util = "0.1"`, `uuid = "1"`. New helper
  `src/adapters/mcp_server.rs::build_router` composes the
  `StreamableHttpService` (mounted via `Router::nest_service("/mcp")`)
  with the bearer-auth + sensitive-headers + trace layers. `main.rs`
  binds `127.0.0.1:0`, generates a UUIDv4 token, writes
  `endpoint.toml`, runs `axum::serve` with `with_graceful_shutdown` on
  `ctrl_c`, removes `endpoint.toml` on exit. The `SchemaServer::run_stdio`
  method is gone. CLI `schema serve` runs HTTP only. `schema --version`
  and `schema --help` confirmed clean from a release build.
  Validation gate (53 tests, fmt, clippy `-D warnings`) all green.

### Session liveness (rmcp default behaviour) — added 2026-04-26

`StreamableHttpServerConfig::default()` (verified in upstream source at
`crates/rmcp/src/transport/streamable_http_server/tower.rs:106`) ships:

- `sse_keep_alive: Some(Duration::from_secs(15))` — SSE comment
  heartbeat keeps the long-poll connection alive across HTTP
  intermediaries that drop idle streams (proxies, ssh tunnels with
  default keepalive settings, Cloudflare's 100-second idle close).
- `sse_retry: Some(Duration::from_secs(3))` — client retry hint
  emitted in the priming SSE event; Claude Code reconnects within 3 s
  of a network blip rather than the spec default (~5 s).
- `stateful_mode: true` — `mcp-session-id` headers persist server-side
  state across requests; required for tools that need session-scoped
  context.
- `allowed_hosts: ["localhost", "127.0.0.1", "::1"]` — DNS-rebinding
  protection out of the box; rejects `Host` headers that resolve
  outside loopback (mitigates a CSRF vector specific to MCP-over-HTTP
  on developer machines).

`LocalSessionManager::evict_expired_channels()` runs periodically
(`completed_cache_ttl` Duration default in upstream) to garbage-collect
channels of completed sessions. Active sessions never expire — they
live as long as the underlying TCP connection holds.

`build_router` accepts these defaults via
`StreamableHttpServerConfig::default()`. The only knob it explicitly
sets is `cancellation_token` (for graceful SIGTERM drain). To make
the contract visible to readers of the source, follow-up amendment
(2026-04-26 — knobs explicit) re-renders the construction with
explicit `.with_sse_keep_alive(...)`, `.with_sse_retry(...)`,
`.with_allowed_hosts(...)`, `.with_stateful_mode(...)` calls — same
values as the rmcp default, but readable from the call site without
diving into the rmcp source.

### Known gaps (FASE 2 follow-ups) — added 2026-04-26

- **TCP-level `SO_KEEPALIVE`** is **not** set on the
  `tokio::net::TcpListener` accept loop. Localhost-only deployment is
  unaffected (loopback is reliable). If the operator ever exposes the
  daemon over `ssh -L` or Tailscale, the kernel-default TCP keepalive
  (~2 hours on Linux, ~75 minutes macOS) is too long and half-open
  connections survive a NAT drop. Mitigation when needed: wire
  `socket2::SockRef::from(&accepted).set_keepalive(true)` into the
  per-connection setup. **Out of scope for FASE 1.0.**
- **HTTP idle timeout** in axum/hyper is not set. Client decides when
  to close the underlying TCP. Acceptable for localhost MCP where the
  client is a long-lived Claude Code process; revisit if multi-tenant
  or shared-host deployment.
- **CORS `allowed_origins`** is empty. Browser-based MCP clients (none
  ship today, but the spec allows them) would need their origin
  whitelisted via `with_allowed_origins(...)`. Out of scope until a
  consumer asks.
- **Session GC tuning.** `completed_cache_ttl` uses the rmcp default;
  if a future workload generates many short-lived sessions, expose
  it as a config knob (would extend `[server]` section in
  `schema.toml`, ADR amendment).

- **2026-04-26 — SIGTERM handler added.** Original
  `run_axum_until_shutdown` used `tokio::signal::ctrl_c()` only,
  which on Unix listens for **SIGINT only**. SIGTERM (sent by
  launchd `bootout`, systemd `stop`, `kill <pid>`, and pytest
  `Popen.terminate()`) bypassed the handler and the kernel killed
  the process with default action — `endpoint.toml` cleanup never
  ran, the next start saw a stale file. The bug was caught by the
  ADR-0024 E2E suite (`test_sigterm_removes_endpoint_toml`,
  `test_token_rotates_per_restart`). Fix landed in `main.rs::
  wait_for_shutdown_signal`: `tokio::select!` over `ctrl_c()` and
  `signal(SignalKind::terminate()).recv()`, whichever fires first
  triggers `cancellation.cancel()` → `axum::serve` graceful drain
  → `endpoint.toml` unlink. Both signals now produce clean
  shutdown with exit code 0. Validation: 39/39 E2E tests green
  on Linux, build VM 2026-04-26.
