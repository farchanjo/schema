---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
---

# 0021 — Localhost-bound HTTP server with per-project bearer-token auth

> **Y-statement** — In the context of ADR-0019 turning `schema serve`
> into an HTTP server bound to a TCP port on the operator's machine,
> facing the choice between (a) **bind `127.0.0.1` and trust every
> client on the loopback interface** (fine for a strict single-user
> machine, leaks to any other local user, sandboxed renderer, or
> rootless container that can reach loopback), (b) **bind a Unix
> domain socket with file-permission `0600`** (kernel-enforced ACL,
> but Claude Code's MCP HTTP client expects a TCP URL, and the
> spec's Streamable HTTP transport is HTTP-over-TCP), or (c)
> **bind `127.0.0.1` plus a per-project bearer token written to a
> mode-`0600` file in the project cache directory, validated by a
> tower middleware on every request**, we decided for **(c)**,
> against (a) (the second-user / sandbox surface is not zero on a
> shared dev machine) and (b) (forecloses Streamable HTTP
> compatibility), to achieve **a localhost MCP endpoint that is
> usable only by clients that can read the token file (i.e. the
> operator's own processes)**, accepting **the marginal complexity
> of token generation, file write, and middleware validation as
> the price of not silently exposing the corpus index to every
> process on the host**.

## Context and Problem Statement

ADR-0019 introduces a per-project HTTP server bound to a kernel-
assigned port on `127.0.0.1`. By itself, that protects against
remote attackers but not against:

- Other unix users on a shared dev machine (`useradd alice; sudo -u
  alice curl http://127.0.0.1:$port/mcp/...`)
- Sandboxed processes the operator might run (browser tabs cannot
  reach loopback by default in modern browsers, but Electron apps,
  some VS Code extensions, and rootless containers can)
- Future cases where the operator wires in a remote tunnel
  (`ssh -L`, Cloudflare Tunnel) and forgets the local server is
  authless

The corpus index is not ultra-sensitive — most consumer projects
keep documentation public on GitHub — but the operator's
`schema.toml` may include private notes, and the "everything on
loopback is friendly" assumption is no longer reliable on modern
multi-user dev machines.

## Decision Drivers

- **Defense in depth.** Loopback is not a perimeter.
- **Standard pattern.** HTTP MCP clients understand
  `Authorization: Bearer <token>`. Claude Code's `.mcp.json`
  supports a `headers` field to inject the bearer.
- **Minimal complexity.** No OAuth, no JWT signing, no rotation
  schedule. A per-process random UUID written to a 0600 file is
  enough.
- **Compatibility with ADR-0019 / 0020.** The token must survive
  a server restart; therefore it lives in `endpoint.toml`, not in
  process memory only.

## Considered Options

### Option A — Bind `127.0.0.1` with no auth (rejected)

Smallest code. Trusts every local process on the machine. Fine for
a strictly single-user machine, fragile for the operator's actual
environment (ADR-0014 install path is `/usr/local/bin`, implying
the binary is shared with anyone on the box).

### Option B — Unix domain socket with `0600` permissions (rejected)

Kernel ACL via filesystem permissions. Cleaner conceptually. But:

- The MCP Streamable HTTP transport is HTTP-over-TCP. `rmcp` 1.5's
  `transport-streamable-http-client-unix-socket` exists for the
  client side, but Claude Code's HTTP transport configuration
  expects a `url`. Wiring Claude Code to a Unix socket is not in
  the official spec or shipping Claude Code releases as of the
  decision date.
- Some of Claude Code's HTTP client support depends on `reqwest`
  + `hyper`, which support Unix sockets only through workarounds.
- Deferring HTTP-over-TCP foreclosure to a later ADR; not needed
  to take here.

### Option C — `127.0.0.1` + bearer token (chosen)

Server generates a fresh UUIDv4 at startup, writes
`{ "url": "http://127.0.0.1:<port>", "token": "<uuid>" }` to
`~/.cache/schema/projects/<id>/endpoint.toml` with mode `0600`. A
tower middleware on the `axum::Router` validates the
`Authorization: Bearer <uuid>` header on every request to `/mcp`
and returns `401 Unauthorized` on mismatch. `/health` is
unauthenticated.

Token rotates on every server restart. Clients re-read
`endpoint.toml` if a request comes back `401`.

## Decision Outcome

**Option C — localhost bind plus per-project bearer token in
`endpoint.toml`.**

### Token lifecycle

- **Generation.** On every successful start of the HTTP server
  (post-`TcpListener` bind, pre-`axum::serve`), generate a fresh
  UUIDv4 with the `uuid` crate. Verified: `uuid = { version = "1",
  features = ["v4"] }` is added as a **direct** dep in ADR-0019's
  Cargo.toml deltas (rmcp 1.5's
  `transport-streamable-http-server-session` brings `uuid` as a
  transitive dep, but relying on a transitive for our own code is
  fragile — keep the direct dep).
- **Persistence.** Write
  `~/.cache/schema/projects/<id>/endpoint.toml` with mode `0600`
  enforced via `OpenOptions::mode(0o600)` *before* the first byte
  is written (POSIX-only API; the file is per-user cache and the
  whole stack is already POSIX-only post-ADR-0014). Contents:

  ```toml
  version    = 1                                # bump on schema change
  url        = "http://127.0.0.1:48291"
  token      = "f47ac10b-58cc-4372-a567-0e02b2c3d479"
  pid        = 12345
  started_at = "2026-04-26T18:14:09Z"
  ```

  **File name:** `endpoint.toml` rather than `server.toml`. Reason:
  one character off from `schema.toml` is a visual hazard; the
  consumer's `schema.toml` is the input to the server, the cache's
  `endpoint.toml` is the output that describes how to reach the
  server. Worth the cosmetic distance.

- **Removal.** On graceful SIGTERM (ADR-0020), the server unlinks
  `endpoint.toml` so a stale token does not linger.
- **Hard-kill / panic.** `endpoint.toml` is not unlinked. Next
  server start writes a fresh token; clients reading the stale
  token fail and re-read the file. Acceptable.

### Middleware

`tower-http` 0.6's `ValidateRequestHeaderLayer` exposes only
`accept(value)` and `custom(validator)` constructors (verified in
the upstream source); the previous draft's
`ValidateRequestHeaderLayer::bearer(&token)` does not exist.
Bearer-token validation on the **server** side is therefore done
via `custom(...)` with a tiny `ValidateRequest` impl, or via the
`auth::AsyncRequireAuthorizationLayer`. Going with the synchronous
custom validator — simpler, no async boilerplate.

```rust
use bytes::Bytes;
use http::{header, Request, Response, StatusCode};
use http_body_util::Full;
use tower_http::validate_request::{ValidateRequest, ValidateRequestHeaderLayer};

#[derive(Clone)]
struct BearerValidator { expected: String }

impl<B> ValidateRequest<B> for BearerValidator {
    type ResponseBody = Full<Bytes>;

    fn validate(
        &mut self,
        req: &mut Request<B>,
    ) -> Result<(), Response<Self::ResponseBody>> {
        let supplied = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        if supplied.map(|s| s == self.expected).unwrap_or(false) {
            Ok(())
        } else {
            Err(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Full::new(Bytes::from_static(b"")))
                .unwrap())   // header values are static, infallible
        }
    }
}

// Wiring: scope auth to /mcp only; /health stays open.
let validator = BearerValidator { expected: token.clone() };
let mcp_router = axum::Router::new()
    .route("/mcp", axum::routing::any_service(streamable_http_service))
    .layer(ValidateRequestHeaderLayer::custom(validator));

let app = axum::Router::new()
    .merge(mcp_router)
    .route("/health", axum::routing::get(health_handler))
    // Mark Authorization sensitive *before* the trace layer wraps
    // anything, so structured logs never serialise the bearer.
    .layer(tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer::new(
        std::iter::once(header::AUTHORIZATION).collect(),
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http());
```

**Threat model on byte-equality.** The `s == self.expected`
comparison is byte-slice equality, which is **not** constant-time.
The realistic threat (another local user attempting a timing
side-channel against a 128-bit UUIDv4 over loopback) is out of
scope: distinguishing two 128-bit values via timing requires
millions of repeated probes, which would be louder in `endpoint.log`
than the attack itself. If a future deployment needs constant-time
comparison (remote auth, multi-tenant), open a new ADR and switch
to `subtle::ConstantTimeEq`.

**Why redact `Authorization`.** `tower-http`'s `TraceLayer` logs
request headers by default. ADR-0020 routes the server's stderr
into `~/.cache/schema/projects/<id>/stderr.log`. Without the
`SetSensitiveRequestHeadersLayer`, the bearer would land in
`stderr.log` on every request — readable by anyone with read
access to the file (`0644` by default on macOS; the ADR-0020
runbook will tighten to `0600` on first write but a window of
exposure exists). The sensitive-headers layer marks the header so
that `TraceLayer` (and any other tower-http layer that respects
the bit) emits `Authorization: <redacted>` instead. The fitness
function below greps the log to prove this works.

### Client wiring (consumer side)

Consumers (Claude Code's `.mcp.json` for a project) point at the
server with both URL and token. Two delivery shapes — pick one:

**Shape A — direct URL + token in `.mcp.json`** (token captured at
install-time, regenerated on every server restart):

```json
{
  "mcpServers": {
    "schema": {
      "url": "http://127.0.0.1:48291",
      "headers": { "Authorization": "Bearer f47ac10b-58cc-..." }
    }
  }
}
```

ADR-0020's `schema mcp-config --config <path>` verb prints this
block ready to paste; the operator regenerates after every
restart. Friction: every restart rotates the token; the operator
must re-run `mcp-config` and re-paste.

**Shape B — `mcp-shim` wrapper (follow-up)** that reads
`endpoint.toml` on every Claude Code reconnect:

```json
{
  "mcpServers": {
    "schema": {
      "command": "/usr/local/bin/schema",
      "args": ["mcp-shim", "--config", "/path/to/schema.toml"]
    }
  }
}
```

`schema mcp-shim` is a thin stdio↔HTTP proxy: reads
`endpoint.toml` to learn URL+token, opens an HTTP MCP connection,
and bridges stdio JSON-RPC to it. Re-reads `endpoint.toml` on any
401/connection-refused. **This eliminates token-rotation friction
entirely**. Cost: ~150 lines of proxy code, plus a new MCP client
dep on the rmcp `transport-streamable-http-client` feature.

Shape A ships first (zero new code beyond `mcp-config`); Shape B
follows as a near-term ADR if the operator hits the rotation
friction. Both shapes live in `arch/operations/`.

### What is NOT defended

- **Token-file readability.** If another user on the machine has
  read access to `~/.cache/schema/projects/<id>/endpoint.toml`,
  they can read the token. The `0600` mode + per-user cache dir
  (`~/.cache`) is the defense.
- **Network sniffing.** `http://`, not `https://`. On loopback,
  there is no realistic packet sniffer; TLS would be theatre.
- **Replay.** Token does not expire mid-session. Restarting the
  server rotates it. Acceptable for a personal dev tool.

## Consequences

- **Good:** trivial defense against same-machine other-user access
  and accidental tunnel exposure.
- **Good:** standard `Authorization: Bearer` shape works with
  every HTTP MCP client without surprises.
- **Good:** `endpoint.toml` doubles as discovery (URL + token in
  one place), simplifying ADR-0020's install verbs.
- **Neutral:** token rotates per restart; clients that cache it
  must re-read on `401`. Claude Code's HTTP transport supports
  `headers` in `.mcp.json` statically; if a restart happens
  mid-session, the operator's MCP wrapper needs to re-read
  `endpoint.toml`. A dedicated `schema mcp-shim` proxy that always
  reads the latest token is a follow-up if friction shows up.
- **Bad:** the `endpoint.toml` file pattern is now a load-bearing
  contract. Any future change to its schema is a breaking
  change for consumer wiring; pin the format with a `version =
  1` field at the top.
- **Bad:** Unix sockets, OAuth, mTLS are all closed out for now.
  Re-opening them needs a new ADR.

## Fitness function

- **Unit test:** `endpoint.toml` writer enforces mode `0600` —
  `use std::os::unix::fs::PermissionsExt;
  assert_eq!(metadata.permissions().mode() & 0o777, 0o600);`
  (POSIX-only; the entire stack post-ADR-0014 is POSIX, so no
  cross-platform branch needed).
- **Integration test (auth required):** start server, POST `/mcp`
  with no `Authorization` header → assert `401`; with wrong
  token → assert `401`; with the token from `endpoint.toml` →
  assert `200` and the body deserialises as a JSON-RPC response.
- **Integration test (health unauthenticated):** GET `/health`
  with no auth header → `200 OK`. Body must not include the
  project name, the token, or any path under
  `~/.cache/schema/`.
- **Integration test (token redaction):** spawn server with
  stderr captured to a tempfile; issue an authenticated request;
  read the captured stderr; assert the captured trace logs
  contain `Authorization: <redacted>` (or equivalent placeholder)
  and **do not** contain the actual token string. Proves the
  `SetSensitiveRequestHeadersLayer` fires before `TraceLayer`.
- **Lint:** `endpoint.toml` schema validated via `serde` round-trip
  with `version = 1` required at the top of the file. Backwards-
  incompatible schema changes bump `version` and the parser
  rejects unknown majors.

## Cross-references and follow-ups

- **ADR-0008 — cache isolation.** `endpoint.toml` is added to the
  per-project cache layout. ADR-0008 frontmatter `Evidence and
  amendments` will record this on accept.
- **ADR-0019 — Streamable HTTP transport.** This ADR closes the
  network-surface gap opened by ADR-0019. The three ADRs
  (0019, 0020, 0021) form an atomic accept set.
- **ADR-0020 — service permanent.** Install verbs read
  `endpoint.toml` for `service status`; `schema mcp-config` verb
  emits the consumer-side JSON snippet (Shape A above).
- **`arch/operations/`.** Runbook
  `arch/operations/0021-mcp-client-wiring.md` documents both
  Shape A (paste-and-restart) and Shape B (mcp-shim) for the
  consumer side. Token-rotation friction explained.
- **Follow-up ADR:** `schema mcp-shim` proxy (Shape B). Promote
  to a real ADR once Shape A's friction is observed in practice.
- **Follow-up:** TLS via `axum-server` or `rustls` for a future
  remote-tunnel case (`ssh -L`, Tailscale). Out of scope here.
- **Follow-up:** constant-time comparison via `subtle` if the
  threat model changes (remote auth, multi-tenant). Out of scope.

## Evidence and amendments

- **2026-04-26 — implemented.** `src/adapters/auth.rs` exposes
  `BearerValidator` implementing `tower_http::validate_request::ValidateRequest`
  with `ResponseBody = axum::body::Body` (mandatory: axum's
  `Router::nest_service` homogenises the inner service body to
  `axum::body::Body`, so the validator must return the same type
  for the chain to type-check). Five unit tests cover accept,
  missing header, wrong token, wrong scheme, and case-sensitive
  prefix matching. `src/adapters/endpoint_toml.rs` writes
  `endpoint.toml` with `OpenOptions::mode(0o600)` enforced before
  the first byte; five unit tests cover the `0600` mode, round-trip,
  unsupported version rejection, ENOENT swallowing, and removal.
  In `build_router` (ADR-0019), the `SetSensitiveRequestHeadersLayer`
  for `Authorization` is layered **before** `TraceLayer::new_for_http`
  so structured logs never serialise the bearer. Validation gate
  green (53 tests, fmt, clippy `-D warnings`).

- **2026-04-26 — validator shape amended by ADR-0026.**
  ADR-0021's `BearerValidator` performs an `eq` check against
  a single token per process. ADR-0026 (one shared daemon, N
  projects) requires the validator to be **multi-tenant**:
  `Map<TokenHash, ProjectId>` populated from the daemon's
  project registry at startup, refreshed on
  `schema project register / unregister`. The middleware
  resolves the bearer to a `project_id`, attaches it to the
  request extensions, and the MCP handlers route every
  retrieval call to the matching `ProjectInstance`. Token
  rotation per restart, `0600` `endpoint.toml` mode,
  localhost-only bind, `SetSensitiveRequestHeadersLayer`
  ordering, and the constant-time-comparison follow-up are
  **all preserved** — they apply per token. New requirements
  layered in by ADR-0026: (1) the validator must reject any
  `(token, project_id)` pair where the URL or route declares a
  different `project_id` than the one bound to the token
  (defence-in-depth against B.3 hybrid routing mistakes); (2)
  the canary E2E in ADR-0026 §"Fitness function" is the
  authoritative test that bearer-A on path-B returns 403, not
  200-with-empty-results. Implementation gated on the fitness
  function. See ADR-0026 §"Decision" and ADR-0026 §"Fitness
  function".
