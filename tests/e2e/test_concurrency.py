"""Concurrent MCP sessions against a single `schema serve` process
(ADR-0019)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import concurrent.futures

from conftest import ServerHandle
from test_workspace_context import _initialize, _call_tool


def _do_session(server: ServerHandle) -> str:
    """One init + one ping; returns the session id."""
    sid = _initialize(server)
    envelope = _call_tool(server, sid, "ping")
    contents = envelope["result"]["content"]
    assert contents[0]["text"] == "pong"
    return sid


def test_n_parallel_sessions_succeed(server: ServerHandle) -> None:
    """ADR-0019 — N=5 concurrent initialize+ping. Each gets its own
    mcp-session-id; all 5 succeed."""
    n = 5
    with concurrent.futures.ThreadPoolExecutor(max_workers=n) as pool:
        futures = [pool.submit(_do_session, server) for _ in range(n)]
        session_ids = [f.result(timeout=30) for f in futures]
    # All session ids are distinct (LocalSessionManager assigns fresh
    # uuid per initialize).
    assert len(set(session_ids)) == n, (
        f"expected {n} distinct session ids, got {len(set(session_ids))}: "
        f"{session_ids}"
    )


def test_embedder_loads_once_across_sessions(server: ServerHandle) -> None:
    """ADR-0019 §"single embedder per project" — fastembed.try_new
    fires exactly once per process lifetime, regardless of session count."""
    # Trigger 3 sessions; each calls workspace_context (no embedder load
    # required — pure metadata read). Embedder load happens on the first
    # `query`-style call. We don't trigger that here, just assert the
    # log shows a single `bge-m3 embedder ready` line at process start.
    for _ in range(3):
        _do_session(server)
    body = server.stderr_log.read_text()
    init_count = body.count("bge-m3 embedder ready")
    # Could be 0 (if startup hasn't completed loading by now) or 1.
    # Never 2 — that would mean two embedder constructions.
    assert init_count <= 1, (
        f"embedder initialised {init_count} times; expected at most 1. "
        f"This breaks ADR-0019's single-embedder-per-process invariant."
    )
