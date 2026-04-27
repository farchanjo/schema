"""ADR-0027 canary fitness function — provable per-project isolation.

The contract this test enforces (ADR-0027 §"Fitness function"): given
the shared daemon serving a single `/mcp` endpoint with a single
workstation bearer, two MCP tool calls with distinct
`working_directory` parameters must each return chunks strictly from
the directory they name. Cross-bleed in either direction fails the
build.

The test spins the real `schema daemon` binary against a pair of
synthetic project trees (`alpha/`, `beta/`) with the **same** filename
(`docs/decisions/0001-x.md`) and **distinguishable** canary tokens
(`ALPHA-CANARY-7f3` vs `BETA-CANARY-9b2`).

Marked `slow` because it exercises the BGE-M3 ONNX session and runs
two cold delta-syncs in-band; expect ~30-120 s per project on a fresh
model cache. Operators run via `pytest -m slow tests/e2e/`.
"""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import dataclasses
import json
import os
import platform
import signal
import subprocess
import time
import tomllib
from collections.abc import Generator
from pathlib import Path

import httpx
import pytest

from conftest import _resolve_binary  # type: ignore[attr-defined]


ALPHA_CANARY = "ALPHA-CANARY-7f3"
BETA_CANARY = "BETA-CANARY-9b2"


# ---------------------------------------------------------------------------
# Project fixture builder
# ---------------------------------------------------------------------------


def _build_canary_project(root: Path, name: str, canary: str) -> Path:
    """Construct a one-ADR project rooted at `root/<name>/` with `canary`
    as the body of `docs/decisions/0001-x.md`."""
    project_root = root / name
    project_root.mkdir(parents=True)
    (project_root / "docs" / "decisions").mkdir(parents=True)
    (project_root / "docs" / "decisions" / "0001-x.md").write_text(
        "---\n"
        "status: accepted\n"
        "date: 2026-04-27\n"
        "decision-makers: [\"e2e\"]\n"
        "review-due: 2027-04-27\n"
        "---\n"
        "\n"
        f"# 0001 — Canary for {name}\n"
        "\n"
        f"This ADR carries the canary token {canary} for the ADR-0027\n"
        "isolation fitness function.\n",
    )
    (project_root / "schema.toml").write_text(
        f'[project]\nname = "{name}"\nversion = "1"\n\n'
        '[[corpus]]\npath = "docs/decisions"\nkind = "adr-madr"\n\n'
        "[embedding]\nnice = 5\n",
    )
    return project_root


# ---------------------------------------------------------------------------
# Daemon lifecycle
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class DaemonHandle:
    process: subprocess.Popen[bytes]
    url: str
    token: str
    home: Path
    stderr_log: Path

    @property
    def auth_headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.token}"}


def _global_endpoint_path(home: Path) -> Path:
    """Mirror `main::global_endpoint_path` — macOS vs Linux suffix."""
    if platform.system() == "Darwin":
        return home / "Library" / "Application Support" / "schema" / "endpoint.toml"
    return home / ".local" / "state" / "schema" / "endpoint.toml"


def _spawn_daemon(binary: Path, home: Path) -> DaemonHandle:
    """Spawn `schema daemon` with `HOME=home` so the global
    `endpoint.toml` and per-project caches land under the test
    sandbox, not the operator's real `~/Library/Application Support/
    schema/`."""
    home.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["HOME"] = str(home)
    env["RUST_LOG"] = env.get("RUST_LOG", "info")

    stderr_log = home / "stderr.log"
    stderr_target = open(stderr_log, "wb")

    process = subprocess.Popen(
        [str(binary), "daemon"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=stderr_target,
    )

    endpoint_path = _global_endpoint_path(home)
    deadline = time.monotonic() + 90.0
    endpoint: dict | None = None
    while time.monotonic() < deadline:
        if endpoint_path.is_file():
            try:
                with open(endpoint_path, "rb") as fh:
                    candidate = tomllib.load(fh)
            except (OSError, tomllib.TOMLDecodeError):
                time.sleep(0.1)
                continue
            if int(candidate.get("pid", -1)) == process.pid:
                endpoint = candidate
                break
        if process.poll() is not None:
            stderr_body = stderr_log.read_text() if stderr_log.exists() else "(none)"
            raise RuntimeError(
                "schema daemon exited before writing endpoint.toml "
                f"(rc={process.returncode}). stderr:\n{stderr_body}"
            )
        time.sleep(0.25)
    if endpoint is None:
        process.kill()
        process.wait(timeout=5)
        stderr_body = stderr_log.read_text() if stderr_log.exists() else "(none)"
        raise TimeoutError(
            "schema daemon did not write endpoint.toml within 90 s. "
            f"expected pid={process.pid} at {endpoint_path}. "
            f"stderr:\n{stderr_body}"
        )

    return DaemonHandle(
        process=process,
        url=str(endpoint["url"]),
        token=str(endpoint["token"]),
        home=home,
        stderr_log=stderr_log,
    )


def _terminate_daemon(handle: DaemonHandle) -> None:
    if handle.process.poll() is not None:
        return
    handle.process.send_signal(signal.SIGTERM)
    try:
        handle.process.wait(timeout=20.0)
    except subprocess.TimeoutExpired:
        handle.process.kill()
        handle.process.wait(timeout=5.0)


@pytest.fixture
def daemon(tmp_path: Path) -> Generator[DaemonHandle, None, None]:
    binary = _resolve_binary()
    home = tmp_path / "home"
    handle = _spawn_daemon(binary, home)
    try:
        yield handle
    finally:
        _terminate_daemon(handle)


# ---------------------------------------------------------------------------
# MCP request helpers
# ---------------------------------------------------------------------------


def _initialize(daemon: DaemonHandle) -> str:
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "clientInfo": {"name": "e2e-canary", "version": "0"},
            "capabilities": {},
        },
    }
    response = httpx.post(
        daemon.url,
        json=payload,
        headers={
            **daemon.auth_headers,
            "Accept": "application/json, text/event-stream",
        },
        timeout=30.0,
    )
    assert response.status_code in (200, 206), (
        f"initialize failed: {response.status_code} {response.text[:300]}"
    )
    session_id = response.headers.get("mcp-session-id")
    assert session_id, "initialize response missing mcp-session-id header"
    return session_id


def _call_tool(
    daemon: DaemonHandle,
    session_id: str,
    name: str,
    args: dict | None = None,
    timeout: float = 600.0,
) -> dict:
    """Issue `tools/call`. Long timeout because cold `resolve_or_wire`
    runs the initial delta-sync inside the request (ADR-0027 §"Open
    questions" — initial-sync timeout)."""
    payload = {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": name, "arguments": args or {}},
    }
    response = httpx.post(
        daemon.url,
        json=payload,
        headers={
            **daemon.auth_headers,
            "mcp-session-id": session_id,
            "Accept": "application/json, text/event-stream",
        },
        timeout=timeout,
    )
    assert response.status_code in (200, 206), (
        f"tools/call {name} failed: {response.status_code} {response.text[:300]}"
    )
    body = response.text.strip()
    if body.startswith("event:") or body.startswith("data:"):
        json_lines = [
            line[len("data:"):].strip()
            for line in body.splitlines()
            if line.startswith("data:")
        ]
        body = json_lines[-1] if json_lines else "{}"
    return json.loads(body)


def _tool_payload(envelope: dict) -> object:
    """Tool body returned as a JSON-encoded string in
    `result.content[0].text` per MCP spec."""
    contents = envelope["result"]["content"]
    return json.loads(contents[0]["text"])


# ---------------------------------------------------------------------------
# Canary fitness function (ADR-0027 §"Fitness function")
# ---------------------------------------------------------------------------


@pytest.mark.slow
def test_query_isolates_alpha_from_beta(
    daemon: DaemonHandle, tmp_path: Path
) -> None:
    """Two `query` calls with distinct `working_directory` parameters
    must each return chunks strictly from their own project. The
    canary tokens (`ALPHA-CANARY` / `BETA-CANARY`) make any leak
    self-evident."""
    alpha = _build_canary_project(tmp_path / "fixtures", "alpha", ALPHA_CANARY)
    beta = _build_canary_project(tmp_path / "fixtures", "beta", BETA_CANARY)

    session_id = _initialize(daemon)

    alpha_envelope = _call_tool(
        daemon,
        session_id,
        "query",
        {"working_directory": str(alpha), "query": "canary"},
    )
    alpha_chunks = _tool_payload(alpha_envelope)
    alpha_text = json.dumps(alpha_chunks)
    assert ALPHA_CANARY in alpha_text, (
        f"alpha query missing ALPHA-CANARY: {alpha_text}"
    )
    assert BETA_CANARY not in alpha_text, (
        f"alpha query leaked BETA-CANARY: {alpha_text}"
    )

    beta_envelope = _call_tool(
        daemon,
        session_id,
        "query",
        {"working_directory": str(beta), "query": "canary"},
    )
    beta_chunks = _tool_payload(beta_envelope)
    beta_text = json.dumps(beta_chunks)
    assert BETA_CANARY in beta_text, (
        f"beta query missing BETA-CANARY: {beta_text}"
    )
    assert ALPHA_CANARY not in beta_text, (
        f"beta query leaked ALPHA-CANARY: {beta_text}"
    )


@pytest.mark.slow
def test_find_decisions_isolates_alpha_from_beta(
    daemon: DaemonHandle, tmp_path: Path
) -> None:
    """`find_decisions` (kind = adr-madr) must respect the same
    isolation contract as `query`."""
    alpha = _build_canary_project(tmp_path / "fixtures", "alpha", ALPHA_CANARY)
    beta = _build_canary_project(tmp_path / "fixtures", "beta", BETA_CANARY)

    session_id = _initialize(daemon)

    alpha_envelope = _call_tool(
        daemon,
        session_id,
        "find_decisions",
        {"working_directory": str(alpha), "query": "canary"},
    )
    alpha_text = json.dumps(_tool_payload(alpha_envelope))
    assert ALPHA_CANARY in alpha_text and BETA_CANARY not in alpha_text

    beta_envelope = _call_tool(
        daemon,
        session_id,
        "find_decisions",
        {"working_directory": str(beta), "query": "canary"},
    )
    beta_text = json.dumps(_tool_payload(beta_envelope))
    assert BETA_CANARY in beta_text and ALPHA_CANARY not in beta_text


@pytest.mark.slow
def test_workspace_context_resolves_per_directory(
    daemon: DaemonHandle, tmp_path: Path
) -> None:
    """`workspace_context` returns project metadata resolved from
    `working_directory`. Two distinct directories must produce two
    distinct `project.id` values."""
    alpha = _build_canary_project(tmp_path / "fixtures", "alpha", ALPHA_CANARY)
    beta = _build_canary_project(tmp_path / "fixtures", "beta", BETA_CANARY)

    session_id = _initialize(daemon)

    alpha_payload = _tool_payload(
        _call_tool(
            daemon,
            session_id,
            "workspace_context",
            {"working_directory": str(alpha)},
        )
    )
    beta_payload = _tool_payload(
        _call_tool(
            daemon,
            session_id,
            "workspace_context",
            {"working_directory": str(beta)},
        )
    )

    assert isinstance(alpha_payload, dict) and isinstance(beta_payload, dict)
    assert alpha_payload["project"]["name"] == "alpha"
    assert beta_payload["project"]["name"] == "beta"
    assert alpha_payload["project"]["id"] != beta_payload["project"]["id"], (
        "alpha and beta resolved to the same project_id — ADR-0008 broken"
    )


@pytest.mark.slow
def test_walk_up_finds_schema_toml_from_subdirectory(
    daemon: DaemonHandle, tmp_path: Path
) -> None:
    """LLM passes a subdirectory; daemon walks up to the project's
    `schema.toml`. ADR-0027 §"Decision drivers" — friendliness with
    Claude Code's CWD."""
    alpha = _build_canary_project(tmp_path / "fixtures", "alpha", ALPHA_CANARY)
    nested = alpha / "docs" / "decisions"  # already exists, deep subdir

    session_id = _initialize(daemon)
    payload = _tool_payload(
        _call_tool(
            daemon,
            session_id,
            "workspace_context",
            {"working_directory": str(nested)},
        )
    )
    assert isinstance(payload, dict)
    assert payload["project"]["name"] == "alpha", (
        f"walk-up did not find alpha's schema.toml from {nested}"
    )


@pytest.mark.slow
def test_no_schema_toml_returns_error_envelope(
    daemon: DaemonHandle, tmp_path: Path
) -> None:
    """Calling a project-scoped tool against a directory with no
    `schema.toml` walking up must return a structured error envelope,
    not a 5xx and not a silent empty result."""
    bare = tmp_path / "bare"
    bare.mkdir()

    session_id = _initialize(daemon)
    payload = _tool_payload(
        _call_tool(
            daemon,
            session_id,
            "query",
            {"working_directory": str(bare), "query": "anything"},
            timeout=30.0,
        )
    )
    assert isinstance(payload, dict), f"expected error envelope, got {payload!r}"
    assert "error" in payload, f"missing error key: {payload}"
    assert "schema.toml" in payload["error"], (
        f"error message missing schema.toml hint: {payload['error']}"
    )


@pytest.mark.slow
def test_reset_alpha_does_not_touch_beta(
    daemon: DaemonHandle, tmp_path: Path
) -> None:
    """`reset_index` scopes to the project resolved from
    `working_directory`. Other projects' indexes are untouched —
    ADR-0008 isolation, ADR-0027 §"Decision drivers"."""
    alpha = _build_canary_project(tmp_path / "fixtures", "alpha", ALPHA_CANARY)
    beta = _build_canary_project(tmp_path / "fixtures", "beta", BETA_CANARY)

    session_id = _initialize(daemon)

    # Index both projects.
    for project_dir, canary in ((alpha, ALPHA_CANARY), (beta, BETA_CANARY)):
        text = json.dumps(
            _tool_payload(
                _call_tool(
                    daemon,
                    session_id,
                    "query",
                    {"working_directory": str(project_dir), "query": "canary"},
                )
            )
        )
        assert canary in text, f"{project_dir.name} initial index missing canary"

    # Reset alpha.
    reset_payload = _tool_payload(
        _call_tool(
            daemon,
            session_id,
            "reset_index",
            {"working_directory": str(alpha)},
        )
    )
    assert isinstance(reset_payload, dict)
    assert reset_payload.get("status") == "ok", (
        f"alpha reset_index did not return ok: {reset_payload}"
    )

    # Beta still resolves.
    beta_text = json.dumps(
        _tool_payload(
            _call_tool(
                daemon,
                session_id,
                "query",
                {"working_directory": str(beta), "query": "canary"},
            )
        )
    )
    assert BETA_CANARY in beta_text, (
        f"alpha reset wiped beta's index — ADR-0008 isolation broken: "
        f"{beta_text}"
    )
