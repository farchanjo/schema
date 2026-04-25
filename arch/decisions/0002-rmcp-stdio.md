---
status: accepted
date: 2026-04-25
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-25
---

# 0002 — rmcp 1.5 over stdio for MCP transport

> **Y-statement** — In the context of needing a Rust implementation of
> the Model Context Protocol that Claude Code (and other MCP clients)
> can spawn as a subprocess and communicate with via JSON-RPC, facing
> the choice between using **rmcp 1.5** (the semi-official Anthropic-
> aligned Rust SDK with `#[tool_router]` ergonomics), an **alternative
> community SDK** (less mature, less-tested with Claude Code), or
> **rolling our own JSON-RPC handler** (50 lines but reinventing
> protocol details), we decided for **rmcp 1.5 with the `server`,
> `macros`, and `transport-io` features**, transport set to **stdio**
> (no SSE / WebSocket), and the `#[tool_router(server_handler)]`
> attribute pattern to expose tools, against rolling our own (protocol
> compliance footgun, every spec revision is rework) or alternative
> SDKs (lower test coverage with Claude Code), to achieve a thin,
> protocol-correct binding that ships only what the tool needs,
> accepting one third-party trait dependency on the rmcp crate
> evolution and a small per-tool boilerplate (every tool is a method
> on `SchemaServer` decorated with `#[tool(description = "...")]`).

## Context and Problem Statement

MCP clients (Claude Code, Cursor, Continue) spawn MCP servers as
subprocesses or connect via SSE/WebSocket. Three transports exist in
the spec: **stdio** (default for local subprocesses), **SSE/HTTP**
(for remote servers), and **WebSocket** (bidirectional, server-push).

`schema` is always spawned by Claude Code as a local subprocess. Two
process-level realities:

- The client owns the process lifecycle.
- stdin/stdout are dedicated to JSON-RPC; stderr is for logs.

Three implementation paths:

1. **rmcp** — the Rust MCP SDK published under
   `github.com/modelcontextprotocol/rust-sdk`.
2. **Alternative SDK** — community implementations (less common
   at time of writing).
3. **Hand-rolled JSON-RPC** — `serde_json` + a stdio loop.

## Decision Drivers

- **Protocol compliance.** MCP is evolving (initialize handshake,
  tool/list, tool/call, prompt/list, resources, sampling). Tracking
  every spec revision in our own code is real maintenance burden.
- **Tool ergonomics.** rmcp's `#[tool]` macro emits the JSON Schema
  for tool parameters automatically (via `schemars`); hand-rolled
  code reimplements that schema-emission per tool.
- **Stdio simplicity.** No port allocation, no auth, no CORS — the
  parent process owns the pipes. Right transport for our shape.
- **Single-tenant.** Each Claude Code session spawns its own
  daemon. No multi-client coordination needed.

## Considered Options

### Option A — rmcp 1.5 (chosen)

```rust
#[derive(Clone)]
struct SchemaServer { state: Arc<ServerState> }

#[tool_router(server_handler)]
impl SchemaServer {
    #[tool(description = "...")]
    async fn query(&self, Parameters(p): Parameters<QueryParams>) -> String { ... }
}

#[tokio::main]
async fn main() -> Result<()> {
    let service = SchemaServer::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

Trade-offs: takes a transitive dep on the SDK. Stable at 1.x (5.6 M
downloads as of audit). Active development.

### Option B — alternative community SDK (rejected)

Multiple community implementations exist; coverage with Claude Code
specifically is unproven. Switching later is cheap (the protocol is
JSON-RPC); switching now buys nothing.

### Option C — hand-rolled JSON-RPC over stdio (rejected)

Possible in ~50 lines for a "ping" server; balloons quickly when
parameter validation, error envelopes, capabilities negotiation, and
the initialize handshake enter scope. Reinvents the protocol layer
without ergonomic gain.

## Decision Outcome

Chosen option: **rmcp 1.5**, features `server` + `macros` +
`transport-io`. Transport: **stdio only** (no SSE/HTTP/WebSocket).

### Tool exposure pattern

Every tool is an async method on `SchemaServer` annotated with
`#[tool(description = "...")]`. Parameter types implement
`schemars::JsonSchema` so the SDK emits the tool's JSON schema
automatically. Return type is `String` (JSON-encoded body); errors
are encoded inline (no MCP-level error envelopes for FASE 1.0 to keep
the surface small).

### Server cloning

`SchemaServer` is `Clone` because rmcp clones the server per in-flight
request. State that must persist (config, embedder, store) lives
behind `Arc<...>` so cloning is cheap (just bumps a refcount).

## Consequences

- **Good:** automatic JSON Schema for tool params; no hand-written
  schema per tool.
- **Good:** stdio transport requires zero networking config; no
  ports, no firewalls, no auth.
- **Good:** stable 1.x SDK; breaking changes signalled via semver.
- **Bad:** transitive dep on rmcp evolution. A future major-version
  bump may require migration work (mitigated by version pinning).
- **Bad:** no support for SSE/WebSocket today — if `schema` ever
  needs to serve remote MCP clients, this ADR must be amended.
- **Neutral:** the `Clone` requirement forces all mutable state
  behind interior mutability (`Mutex<Embedder>` is the only such
  case in FASE 1.0).

## Fitness function

- The crate's `serve(stdio())` flow is exercised on every Claude
  Code session that connects to schema; failures surface as
  inability to list/call tools, immediately observable to the user.
- An integration smoke test (FASE 1.1) will spawn the binary,
  send a `tools/list` JSON-RPC, and assert the expected tool set
  ships. Until then, manual smoke via Claude Code.

## More information

- `src/mcp/server.rs` — every tool definition, the `tool_router`.
- `src/main.rs` — clap CLI; `schema serve` wires the rmcp transport.
- ADR-0001 — Rust + Cargo (the host).
- rmcp docs: <https://docs.rs/rmcp/1.5.0/rmcp/>
- MCP spec: <https://modelcontextprotocol.io/>
