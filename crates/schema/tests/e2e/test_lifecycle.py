"""Lifecycle of `schema serve` — spawn, endpoint.toml writing,
graceful SIGTERM, file cleanup (ADR-0019, ADR-0021)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import os
import signal
import time
from pathlib import Path

import pytest

from conftest import Project, ServerHandle, _terminate


def _endpoint_mode(path: Path) -> int:
    return path.stat().st_mode & 0o777


def test_endpoint_toml_appears_after_serve(server: ServerHandle) -> None:
    """ADR-0021 fitness — endpoint.toml exists with mode 0600 once
    the server has started."""
    path = server.project.endpoint_toml
    assert path.is_file(), "endpoint.toml must exist after spawn"
    assert _endpoint_mode(path) == 0o600, (
        f"endpoint.toml must be 0600, got {oct(_endpoint_mode(path))}"
    )


def test_endpoint_toml_carries_required_fields(server: ServerHandle) -> None:
    """ADR-0019 §"endpoint.toml schema" — version, url, token, pid,
    started_at all present."""
    expected = {"version", "url", "token", "pid", "started_at"}
    actual = set(server.endpoint.keys())
    missing = expected - actual
    assert not missing, f"endpoint.toml missing required fields: {missing}"


def test_endpoint_toml_url_targets_loopback(server: ServerHandle) -> None:
    """ADR-0019 §"localhost-only bind" — url must point at 127.0.0.1
    (no public IP, no DNS hostname leak)."""
    assert server.url.startswith("http://127.0.0.1:"), (
        f"url must bind to loopback, got {server.url!r}"
    )


def test_endpoint_toml_pid_matches_process(server: ServerHandle) -> None:
    """endpoint.toml pid is the actual schema serve PID."""
    pid_in_file = int(server.endpoint["pid"])
    assert pid_in_file == server.process.pid


def test_endpoint_toml_version_pinned(server: ServerHandle) -> None:
    """ADR-0021 §"endpoint.toml schema" — version=1 in FASE 1.0."""
    assert int(server.endpoint["version"]) == 1


def test_sigterm_removes_endpoint_toml(spawn_server, project: Project) -> None:
    """ADR-0019 graceful shutdown — SIGTERM unlinks endpoint.toml
    before exit."""
    server = spawn_server(project)
    endpoint_path = server.project.endpoint_toml
    assert endpoint_path.is_file()

    rc = _terminate(server, hard=False)
    assert rc == 0, f"graceful shutdown returned {rc}"
    assert not endpoint_path.exists(), (
        "endpoint.toml must be removed on graceful shutdown"
    )


def test_sigkill_leaves_stale_endpoint_toml(
    spawn_server, project: Project
) -> None:
    """Hard kill (SIGKILL) bypasses graceful shutdown; endpoint.toml
    stays. Documented as known behaviour — next start overwrites it."""
    server = spawn_server(project)
    endpoint_path = server.project.endpoint_toml
    assert endpoint_path.is_file()

    server.process.send_signal(signal.SIGKILL)
    server.process.wait(timeout=5)
    assert endpoint_path.exists(), (
        "SIGKILL leaves stale endpoint.toml — operator must clean it "
        "or rely on next-start overwrite"
    )


def test_restart_overwrites_endpoint_toml(
    spawn_server, project: Project
) -> None:
    """A second `schema serve` (after a clean stop) writes a fresh
    endpoint.toml with a new token and a new pid."""
    first = spawn_server(project)
    first_token = first.token
    first_pid = first.process.pid
    _terminate(first, hard=False)

    # endpoint.toml was unlinked; spawn again.
    second = spawn_server(project)
    assert second.token != first_token, "token must rotate per restart"
    assert int(second.endpoint["pid"]) != first_pid


def test_stderr_log_captured_when_spawned(server: ServerHandle) -> None:
    """conftest captures stderr to a file; verifies log lines exist."""
    # Give the server a beat to emit startup logs.
    time.sleep(0.5)
    assert server.stderr_log.exists()
    body = server.stderr_log.read_text()
    # Expect at least the source-of-truth log line from ADR-0023 +
    # the watcher / store init lines.
    assert "config: source path" in body, (
        f"stderr log missing config source line. content:\n{body[:1000]}"
    )
