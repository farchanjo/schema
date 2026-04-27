"""MCP wire protocol — initialize handshake, tools/list, basic tools/call
(ADR-0019 fitness function 1)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import json

import httpx

from conftest import ServerHandle
from test_workspace_context import _initialize, _call_tool


_EXPECTED_TOOLS = {
    "ping",
    "workspace_context",
    "query",
    "find_decisions",
    "glossary_lookup",
    "cross_reference",
    "list_corpus",
    "reset_index",
    "forget_source",
    "synthesize",
}


def _tools_list(server: ServerHandle, session_id: str) -> dict:
    payload = {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/list",
        "params": {},
    }
    response = httpx.post(
        f"{server.url}/mcp",
        json=payload,
        headers={
            **server.auth_headers,
            "mcp-session-id": session_id,
            "Accept": "application/json, text/event-stream",
        },
        timeout=30.0,
    )
    assert response.status_code in (200, 206)
    body = response.text.strip()
    if body.startswith("event:") or body.startswith("data:"):
        json_lines = [
            line[len("data:"):].strip()
            for line in body.splitlines()
            if line.startswith("data:")
        ]
        body = json_lines[-1] if json_lines else "{}"
    return json.loads(body)


def test_initialize_returns_server_info(server: ServerHandle) -> None:
    """ADR-0019 fitness function 1 — initialize → serverInfo + session-id."""
    session_id = _initialize(server)
    assert session_id, "session id must be non-empty"


def test_tools_list_carries_all_ten_tools(server: ServerHandle) -> None:
    """ADR-0009 amendment + ADR-0025 — tool catalogue is 10 tools post-
    2026-04-26 (`synthesize` always registered; runtime-disabled when no
    LLM provider key is configured per ADR-0025 §"silent-degrade evidence")."""
    session_id = _initialize(server)
    envelope = _tools_list(server, session_id)
    assert "result" in envelope, f"missing result: {envelope}"
    tools = envelope["result"].get("tools", [])
    names = {t["name"] for t in tools}
    missing = _EXPECTED_TOOLS - names
    extra = names - _EXPECTED_TOOLS
    assert not missing, f"missing tools: {missing}"
    assert not extra, (
        f"unexpected tools: {extra}. Update _EXPECTED_TOOLS only when the "
        "addition is documented in ADR-0009 / ADR-amendment."
    )


def test_ping_tool_returns_pong(server: ServerHandle) -> None:
    """`ping` smoke test — should always return literal `"pong"`."""
    session_id = _initialize(server)
    envelope = _call_tool(server, session_id, "ping")
    assert "result" in envelope
    contents = envelope["result"].get("content", [])
    assert contents, f"empty content: {envelope}"
    assert contents[0]["text"] == "pong"


def test_list_corpus_returns_project_paths(server: ServerHandle) -> None:
    """`list_corpus` returns the indexed source paths."""
    session_id = _initialize(server)
    envelope = _call_tool(server, session_id, "list_corpus")
    assert "result" in envelope
    contents = envelope["result"].get("content", [])
    assert contents, f"empty content: {envelope}"
    payload = json.loads(contents[0]["text"])
    assert payload["project"] == "e2e-fixture"
    # Fixture has docs/intro.md; delta-sync should index it.
    assert any(
        p.endswith("intro.md") for p in payload["source_paths"]
    ), f"intro.md not in source_paths: {payload['source_paths']}"
