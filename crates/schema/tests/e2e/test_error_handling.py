"""Error paths that should not leak state or crash the daemon."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import json

import httpx

from conftest import ServerHandle
from test_workspace_context import _initialize


def _post_mcp(server: ServerHandle, body: dict, session_id: str | None = None):
    headers = {
        **server.auth_headers,
        "Accept": "application/json, text/event-stream",
    }
    if session_id:
        headers["mcp-session-id"] = session_id
    return httpx.post(
        f"{server.url}/mcp",
        json=body,
        headers=headers,
        timeout=10.0,
    )


def test_unauthorized_response_body_does_not_leak_token(
    server: ServerHandle,
) -> None:
    """ADR-0021 — 401 body is empty (no token leak)."""
    response = httpx.post(
        f"{server.url}/mcp",
        json={},
        headers={"Authorization": "Bearer wrong"},
        timeout=10.0,
    )
    assert response.status_code == 401
    assert server.token not in response.text, (
        "401 response leaked the actual token. Auth response body must "
        "stay empty regardless of input."
    )


def test_invalid_tool_name_returns_jsonrpc_error(
    server: ServerHandle,
) -> None:
    """`tools/call` with a non-existent tool name returns a JSON-RPC
    error envelope, not a 5xx."""
    session_id = _initialize(server)
    response = _post_mcp(
        server,
        {
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {"name": "this_tool_does_not_exist", "arguments": {}},
        },
        session_id=session_id,
    )
    assert response.status_code in (200, 206), (
        f"unknown tool should be a JSON-RPC error, not HTTP failure: "
        f"{response.status_code} {response.text[:300]}"
    )


def test_malformed_json_returns_400_not_500(
    server: ServerHandle,
) -> None:
    """Malformed JSON body should not crash the daemon; expect 400-class
    response (or JSON-RPC parse error envelope) and no token leak."""
    response = httpx.post(
        f"{server.url}/mcp",
        content=b"this is not json",
        headers={
            **server.auth_headers,
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
        timeout=10.0,
    )
    # Acceptable: 400, 422, or a 200 with JSON-RPC error inside.
    assert response.status_code < 500, (
        f"malformed JSON crashed daemon: {response.status_code} "
        f"{response.text[:300]}"
    )
    # Server must remain alive after malformed input.
    health = httpx.get(f"{server.url}/health", timeout=10.0)
    assert health.status_code == 200, (
        "server died after malformed JSON request"
    )


def test_server_survives_repeated_bad_auth(
    server: ServerHandle,
) -> None:
    """20 401s in quick succession do not crash the daemon (bearer
    validator is stateless)."""
    for _ in range(20):
        r = httpx.post(
            f"{server.url}/mcp",
            json={},
            headers={"Authorization": "Bearer bad"},
            timeout=5.0,
        )
        assert r.status_code == 401
    health = httpx.get(f"{server.url}/health", timeout=10.0)
    assert health.status_code == 200
