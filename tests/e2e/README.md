# `tests/e2e/` — End-to-end tests against the running `schema` binary

These tests spawn `target/release/schema serve` as a real subprocess,
parse the runtime `endpoint.toml`, and exercise the HTTP MCP transport
via `httpx`. They are **out of `cargo test`** by design (ADR-0024) — the
suite loads the BGE-M3 ONNX model (~2 GB on first run, ~5 s steady-state
warm-up) and does not belong on the `cargo test` hot path.

## Prerequisites

- Python 3.11+ (`tomllib` from stdlib).
- A `target/release/schema` binary (`cargo build --release` in the repo
  root).
- ~2 GB free disk for the BGE-M3 model on first run, or set
  `SCHEMA_E2E_CACHE_DIR=$HOME/.cache/schema` to reuse the operator's
  production cache.

## Run

```bash
cd tests/e2e
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt

# Optional: reuse production model cache so the first test does not pay
# the 2 GB download cost.
export SCHEMA_E2E_CACHE_DIR=$HOME/.cache/schema

# Run the whole suite:
pytest -v

# Or one file:
pytest -v test_lifecycle.py

# Or one test:
pytest -v test_lifecycle.py::test_endpoint_toml_appears_after_serve
```

The conftest fixture finds the binary by walking up from `tests/e2e/`
looking for `target/release/schema`, then `target/debug/schema` as a
fallback. Override with `SCHEMA_E2E_BINARY=/path/to/schema`.

## What's covered

| File                              | Concern                                                 | ADR fitness |
|-----------------------------------|---------------------------------------------------------|-------------|
| `test_lifecycle.py`               | spawn / SIGTERM / endpoint.toml mode + schema           | ADR-0019, ADR-0021 |
| `test_config_errors.py`           | missing toml / bad TOML / nice out of range / parse env | ADR-0018, ADR-0023 |
| `test_auth.py`                    | 401 missing/wrong/case-sensitive; 200 correct; redaction | ADR-0021 |
| `test_mcp_protocol.py`            | `initialize` handshake, `tools/list`, `tools/call`      | ADR-0019 |
| `test_concurrency.py`             | N parallel sessions; embedder fires once                | ADR-0019 |
| `test_multi_instance.py`          | 2 simultaneous `schema serve` — race detection          | ADR-0008 (gap), ADR-0019 |
| `test_endpoint_toml.py`           | mode 0600 / version=1 / RFC3339 / PID                   | ADR-0021 |
| `test_error_handling.py`          | invalid tool name / malformed JSON-RPC / no token leaks | ADR-0019, ADR-0021 |
| `test_workspace_context.py`       | tool returns project + corpus + embedding from config   | ADR-0009 amendment |
| `test_adr0027_isolation.py`       | canary fitness function — single endpoint, two `working_directory` parameters, ALPHA-CANARY / BETA-CANARY isolation across `query` / `find_decisions` / `workspace_context` / `reset_index`; walk-up + no-`schema.toml` error envelope. Spawns `schema daemon`, marked `slow`. | ADR-0027 |

## Tested on

- **macOS** (operator workstation, codesigned binary at
  `/usr/local/bin/schema`).
- **Linux** (Ubuntu 24.04 LTS, kernel 6.17, x86_64; Linux build VM,
  see operator-private memory for details).

Windows is **not** tested. POSIX-only assumptions: `endpoint.toml` mode
`0600` enforced via `OpenOptions::mode(0o600)`, `signal.SIGTERM` on
the spawned subprocess, `os.kill` semantics. Native Windows support
would require porting at least those three concerns; out of scope
for FASE 1.0.

## CI

Not wired to CI in FASE 1.0. The unit + Rust integration suites
(`cargo test --all-features`) run on every PR via
`.github/workflows/lint.yml`. E2E is a manual-run gate.
A nightly / release-tag GitHub Actions workflow is a FASE 1.1
follow-up (see ADR-0024 §"How to run").
