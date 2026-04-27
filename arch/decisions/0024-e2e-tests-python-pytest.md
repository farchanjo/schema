---
status: accepted
date: 2026-04-26
decision-makers: ["Fabricio Archanjo"]
review-due: 2027-04-26
---

# 0024 — E2E test category in Python (pytest + httpx) against the running binary

> **Y-statement** — In the context of the HTTP MCP transport landed by
> ADR-0019/0020/0021, where the unit tests in `src/**/*.rs` and the
> integration tests in `tests/http_auth.rs` exercise the auth wiring
> and the per-port logic but **not** the full life of a real
> `schema serve` process (model load, endpoint.toml writing, MCP
> initialize handshake, concurrent sessions, multi-instance race
> behaviour, error responses well-formedness), facing the choice
> between (a) **stick with Rust integration tests only** (every E2E
> ergonomic — JSON-RPC envelopes, streaming responses, subprocess
> lifecycle — re-implemented in Rust as test glue), (b) **add Rust
> integration tests that spawn a child `schema serve` and use
> `reqwest` for HTTP** (workable but heavyweight; reqwest+streaming
> in cargo-test infra is awkward, and the cargo test runner does not
> map well to long-lived service tests), or (c) **add a Python
> pytest suite under `tests/e2e/` driven by `httpx` against the
> compiled `target/release/schema` binary**, we decided for **(c)**,
> against (a) (forces every E2E concern into Rust idioms; slow
> iteration on test harness), and (b) (gains nothing over a Python
> pytest suite while paying the Rust cargo-test overhead), to achieve
> **a fast iteration loop for E2E behavioural tests that can probe
> error paths, concurrency, multi-instance races, and protocol
> conformance without dragging the unit-test suite slower or pulling
> a real BGE-M3 model load into every `cargo test`**, accepting **a
> second-language tooling boundary (Python 3.11+, pytest, httpx) and
> the explicit out-of-`cargo test` placement (E2E suite must be run
> separately via `pytest tests/e2e/`).**

## Context and Problem Statement

After ADR-0019/0020/0021/0023 landed, the project's automated test
inventory is:

| Layer        | Where                              | Count | Coverage |
|--------------|------------------------------------|-------|----------|
| Unit         | `src/**/*.rs` `#[cfg(test)] mod`   | 61    | Pure logic, port impls with fakes, config resolution, auth validator, endpoint.toml round-trip, install templates |
| Integration  | `tests/http_auth.rs`               | 5     | axum router shape with `BearerValidator`, `/health` open, `/mcp` gated — uses `tower::ServiceExt::oneshot` (no real binary) |
| E2E          | —                                  | 0     | none |

The integration layer in `tests/http_auth.rs` does not stand up a
real `SchemaServer` (would require `Persistence + Embedder + Walker
+ Chunker + MetadataStore` adapters wired to fakes; the existing
fakes live as private types inside `src/app/delta_sync.rs::tests`
and are not reusable from another integration test target). Even
if we promoted them to a `pub(crate)` `tests-fixtures` feature, the
E2E concerns most worth testing — `schema serve` subprocess lifecycle,
`endpoint.toml` writing under contention, `mcp-session-id` handshake,
graceful SIGTERM, multi-instance race behaviour — are about the
**binary as a whole**, not about the in-process router.

ADR-0019 fitness function 1 calls for an "Integration test
(initialize handshake)" against a real running server. That test
slot is empty.

## Decision Drivers

- **Iteration speed on E2E harness.** Adding a probe (set a header,
  parse a JSON-RPC field, assert an error shape) should be a one-line
  change, not a Rust trait dance.
- **Subprocess lifecycle ergonomics.** Python's `subprocess.Popen`
  with context manager + `terminate()`/`wait()` maps directly to the
  smoke procedure. Rust's `tokio::process::Command` works but adds
  async runtime overhead inside a test that is already not
  hot-path.
- **HTTP client ergonomics.** `httpx` (sync) parses JSON, handles
  Streamable HTTP chunks, manages connection pooling, all in
  ~3 lines per request. Rust `reqwest` is fine but verbose; the
  test glue would dwarf the assertions.
- **MCP protocol shape testing.** Every tool's response is a JSON
  envelope. Python's dict/list assertions are more readable than
  `serde_json::Value` matching in test code that nobody loves
  reading.
- **Out-of-`cargo test` placement.** E2E loads BGE-M3 (~2 GB on
  first run, ~5 s steady state). Pulling that into `cargo test`
  would either skip the test (defeats the purpose) or slow the
  unit-test loop by 5+ s every run. Separation by language makes
  the `cargo test` / `pytest tests/e2e/` boundary explicit.

## Considered Options

### Option A — Rust integration tests only (rejected)

Promote `delta_sync.rs::tests` fakes to `pub(crate)`, add a new
`tests/e2e_initialize.rs` that constructs a `SchemaServer` with
those fakes, mounts it via `build_router`, exercises full MCP
handshake using `reqwest` over a `tokio::net::TcpListener`. Works
but: `reqwest` Streamable HTTP semantics inside a cargo-test
process are awkward; subprocess-based tests cannot run inside a
`#[tokio::test]` cleanly without manual lifecycle management.

### Option B — Rust integration test that spawns child `schema serve` (rejected)

Cargo integration test launches `target/release/schema serve` as
a subprocess via `tokio::process::Command`. Same end result as
Option C but with all the test glue rewritten in Rust + reqwest.
Loses Python's REPL-friendly debugging and the wide ecosystem of
HTTP-debugging idioms.

### Option C — Python pytest suite under `tests/e2e/` (chosen)

A new `tests/e2e/` directory housing a pytest suite. Runtime deps
declared in `tests/e2e/requirements.txt`:

```
pytest>=8.0
httpx>=0.27
```

Standard pytest fixtures spawn `target/release/schema serve` per
test (or per session, if model warm-up dominates), parse
`endpoint.toml`, run assertions via `httpx`, terminate cleanly.
Out-of-`cargo test`: explicit `pytest tests/e2e/` invocation.

## Decision Outcome

**Option C — Python pytest suite at `tests/e2e/`.**

### Layout

```
tests/e2e/
├── README.md                       how to run; model cache note
├── requirements.txt                pytest + httpx pins
├── conftest.py                     server-spawn fixture, endpoint.toml parse
├── fixtures/
│   └── schema_minimal.toml         small valid corpus
├── test_lifecycle.py               spawn / SIGTERM / endpoint.toml mode
├── test_config_errors.py           missing toml, bad TOML, validation
├── test_auth.py                    401 missing/wrong/case; 200 correct
├── test_mcp_protocol.py            initialize, tools/list, tools/call
├── test_concurrency.py             N parallel sessions
├── test_multi_instance.py          2 simultaneous schema serve
├── test_endpoint_toml.py           mode 0600, version=1, RFC3339, PID
├── test_error_handling.py          invalid tool name / params, no leaks
└── test_workspace_context.py       project + corpus + embedding match
```

### How to run

Local:

```bash
cd tests/e2e
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
# Optional: persistent model cache to avoid 2 GB re-download per run.
export SCHEMA_E2E_CACHE_DIR=$HOME/.cache/schema
pytest -v
```

CI: out of scope for FASE 1.0. A future `.github/workflows/e2e.yml`
would install Python, pip-install the requirements, and run pytest
against a release build. Not added to the existing `lint` workflow
because:

- E2E adds 3-5 minutes to a single workflow run (model download +
  per-test server boot).
- E2E failures are integration-flake-prone in shared CI runners
  (port collisions, kqueue inheritance on macOS GHA runners,
  unreliable BGE-M3 download from HF Hub on cold cache).
- Better posture: E2E runs nightly / on release-tag, not on every
  PR. Decision deferred to its own ADR (FASE 1.1).

### Cache strategy for the BGE-M3 model

The fastembed embedder (ADR-0005) downloads ~2 GB of ONNX weights
on first use, caching at `XDG_CACHE_HOME/schema/models/`. Per-test
isolation would require either:

- **Tempdir cache per test** — re-download every run, ~3-5 min
  per test, unworkable.
- **Shared cache via `SCHEMA_E2E_CACHE_DIR` env** — operator points
  at `~/.cache/schema/` (production cache); first test populates
  it; subsequent tests reuse. **Chosen.**
- **CI: pre-warm in a setup step**, mount cache as a workflow
  cache. Out of scope.

The conftest fixture honours `SCHEMA_E2E_CACHE_DIR` (default:
operator's `~/.cache/schema/`) and sets `XDG_CACHE_HOME` for
the spawned server.

### Out-of-`cargo test` boundary

The `tests/e2e/` directory contains **no `*.rs` files**. Cargo's
integration-test discovery only registers `*.rs` in `tests/`, so
the Python suite is invisible to `cargo test`. The `cargo test
--all-features` gate (ADR-0012) keeps running fast; E2E is
explicitly opt-in via `pytest`.

## Consequences

- **Good:** real binary tested end-to-end. Bug classes that unit
  tests cannot reach (subprocess lifecycle, endpoint.toml race,
  signal handling, MCP wire protocol) become reachable.
- **Good:** test harness in Python is fast to iterate — REPL
  debugging, easy fixture extension, ecosystem (pytest-asyncio,
  pytest-mock, hypothesis) available if needed.
- **Good:** BGE-M3 cost paid once across the suite via shared
  cache.
- **Bad:** introduces Python as a development-time language.
  Operators / CI need Python 3.11+. Documented in `tests/e2e/README.md`.
- **Bad:** test-stack drift risk — the Python suite depends on
  the `endpoint.toml` schema (ADR-0021), MCP wire format
  (ADR-0019), and CLI verbs (ADR-0020). Any of those changing
  without updating the E2E suite produces a false-positive pass.
  Mitigation: tests assert on the exact schema fields and reject
  unknown ones (`assert set(endpoint.keys()) == EXPECTED`),
  forcing a code+test paired update.
- **Bad:** out of `cargo test` means a contributor running
  `cargo test --all-features` does **not** run E2E. Mitigation:
  README + runbook spell out `pytest tests/e2e/` as the second
  half of the test-pyramid invocation.

## Fitness function

- **`tests/e2e/test_lifecycle.py::test_endpoint_toml_appears_after_serve`** —
  Spawns the server; polls `endpoint.toml` until it appears with
  mode `0600`; asserts the file's `version`, `url`, `token`,
  `pid`, `started_at` fields are present and well-formed (RFC3339
  for `started_at`, integer for `pid`, `http://127.0.0.1:N` for
  `url`).
- **`tests/e2e/test_lifecycle.py::test_sigterm_removes_endpoint_toml`** —
  Spawns server, sends SIGTERM, waits for exit, asserts
  `endpoint.toml` is gone.
- **`tests/e2e/test_mcp_protocol.py::test_initialize_handshake`** —
  POST `/mcp` with `initialize` JSON-RPC envelope; asserts response
  carries `serverInfo.name == "schema"` and a non-empty
  `mcp-session-id` header (the canonical ADR-0019 fitness function 1).
- **`tests/e2e/test_concurrency.py::test_n_parallel_sessions`** —
  Spawns N=5 concurrent sessions; each completes `initialize` +
  one `tools/call`; all 5 succeed; the trace log shows
  `fastembed.try_new` once.
- **`tests/e2e/test_multi_instance.py::test_two_servers_same_project`** —
  Spawns two `schema serve` against the same `schema.toml`; both
  bind successfully (different ports because `127.0.0.1:0`); the
  later-started one's `endpoint.toml` overwrites the earlier; both
  serve queries; **documents** (does not enforce) that this is
  expected-undefined behaviour — the operator should use ADR-0020
  service mode to avoid double-spawn.

## Cross-references and follow-ups

- **ADR-0019 — HTTP transport.** This ADR fills its fitness slot 1
  ("integration test, initialize handshake").
- **ADR-0021 — bearer auth.** This ADR fills its redaction-grep
  fitness function via `tests/e2e/test_auth.py` + log capture.
- **ADR-0022 — tokio-uring evaluation.** Bench harness lands as
  Python or Rust; if the operator opts for Python (consistent with
  this ADR), the bench can extend the same `tests/e2e/conftest.py`
  with timing assertions.
- **Follow-up:** `.github/workflows/e2e.yml` adding nightly E2E
  against tagged releases. Out of scope; FASE 1.1.
- **Follow-up:** if a new test category beyond Unit / Integration /
  E2E is needed (perf, chaos, security, fuzzing), ADR per
  CLAUDE.md global rule.

## Evidence and amendments

- **2026-04-26 — Initial recording.** Suite scaffolding lands at
  `tests/e2e/` alongside this ADR's acceptance. Operator runs
  `pytest tests/e2e/` manually for now; CI wiring is FASE 1.1.

- **2026-04-26 — Suite implemented + first run on Linux VM.**
  39 tests across 9 files (`test_lifecycle.py`,
  `test_config_errors.py`, `test_auth.py`, `test_mcp_protocol.py`,
  `test_concurrency.py`, `test_multi_instance.py`,
  `test_endpoint_toml.py`, `test_error_handling.py`,
  `test_workspace_context.py`). Run on the build VM with
  `SCHEMA_E2E_BINARY=target-wip/release/schema`,
  `SCHEMA_E2E_CACHE_DIR=/mnt/volumes/build/schema-e2e-cache`:

  ```
  38 passed, 1 xfailed in 83.51s (0:01:23)
  ```

  The single `xfail` is intentional —
  `test_two_simultaneous_servers_lock_each_other_out` documents
  that the binary today does not refuse a second concurrent
  `schema serve` per project (ADR-0008 §"Concurrency" mitigation
  was deferred to FASE 1.1; ADR-0020 service mode prevents this
  case in practice).

  The first execution caught a real bug: SIGTERM was not handled
  in `main.rs::run_axum_until_shutdown` (only SIGINT). Fix landed
  alongside this evidence — `wait_for_shutdown_signal()` now
  `tokio::select!`s over both signals. ADR-0019 evidence updated
  with the fix.

- **Conftest design notes:**
  - `project_id` **and** `project_cache_dir` resolved
    deterministically by parsing the `project id` and
    `cache dir` lines from `schema validate` output. Python
    avoids re-implementing the blake3 hash and the per-OS
    `dirs::cache_dir()` rules; the binary already prints both
    values during validation.
  - `endpoint.toml` polling matches by `pid == process.pid` so a
    stale file from a previous spawn (sibling instance,
    hard-killed predecessor) cannot leak into the test fixture.
  - Model cache is shared via `SCHEMA_E2E_CACHE_DIR` env (default
    on Linux: `~/.cache/schema/`). The fastembed model loads
    once for the whole pytest session; per-test fixtures only
    re-create the project root, not the cache.

- **2026-04-26 — macOS compliance verified.** Initial run on
  macOS surfaced a conftest assumption that broke 33 of 39
  tests: `dirs::cache_dir()` returns `~/Library/Caches/` on
  macOS regardless of `XDG_CACHE_HOME`, while the conftest
  built the expected `endpoint.toml` path from
  `XDG_CACHE_HOME/schema/projects/<id>/`. Fix: parse both
  `project id` and `cache dir` from `schema validate` output
  (binary is the source of truth for the OS-specific cache
  resolution). Both platforms now green:

  | Platform | Result | Wall clock |
  |---|---|---|
  | Linux (Ubuntu 24.04 LTS, kernel 6.17, build VM) | 38 passed + 1 xfailed | 83.5 s |
  | macOS (operator workstation, Apple silicon) | 38 passed + 1 xfailed | 44.5 s |

  macOS is faster for the suite because the Apple silicon
  hardware is faster than the LXC container the Linux build
  VM runs in; both produce identical pass/xfail/error counts,
  confirming no platform-specific test flakiness.
