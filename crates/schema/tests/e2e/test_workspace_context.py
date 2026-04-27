"""`workspace_context` MCP tool (ADR-0009 amendment)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import json

import httpx

from conftest import ServerHandle


def _initialize(server: ServerHandle) -> str:
    """Run the MCP `initialize` handshake and return the session id."""
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "clientInfo": {"name": "e2e-pytest", "version": "0"},
            "capabilities": {},
        },
    }
    response = httpx.post(
        f"{server.url}/mcp",
        json=payload,
        headers={
            **server.auth_headers,
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


def _call_tool(server: ServerHandle, session_id: str, name: str, args: dict | None = None) -> dict:
    """Issue a `tools/call` JSON-RPC request and return the parsed envelope."""
    payload = {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": name, "arguments": args or {}},
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
    assert response.status_code in (200, 206), (
        f"tools/call {name} failed: {response.status_code} {response.text[:300]}"
    )
    # rmcp Streamable HTTP may return either `application/json` or
    # `text/event-stream` chunked; in both shapes the JSON-RPC envelope
    # is the last JSON object on the wire.
    body = response.text.strip()
    # Split SSE frames if present (`data: {...}` lines).
    if body.startswith("event:") or body.startswith("data:"):
        json_lines = [
            line[len("data:"):].strip()
            for line in body.splitlines()
            if line.startswith("data:")
        ]
        body = json_lines[-1] if json_lines else "{}"
    return json.loads(body)


def test_workspace_context_returns_project_metadata(
    server: ServerHandle,
) -> None:
    """ADR-0009 amendment — workspace_context surfaces project name,
    corpus, embedding model from the loaded schema.toml."""
    session_id = _initialize(server)
    envelope = _call_tool(server, session_id, "workspace_context")
    assert "result" in envelope, f"missing result: {envelope}"
    # Tool body returned as a JSON-encoded string in `result.content[0].text`
    # per MCP spec for tool responses.
    contents = envelope["result"].get("content", [])
    assert contents, f"empty content: {envelope}"
    payload = json.loads(contents[0]["text"])
    assert payload["project"]["name"] == "e2e-fixture"
    assert payload["project"]["version"] == "1"
    assert any(
        c["path"] == "docs" and c["kind"] == "Markdown"
        for c in payload["corpus"]
    ), f"corpus does not match fixture: {payload['corpus']}"
    assert payload["embedding"]["model"] == "bge-m3"
    assert payload["embedding"]["dims"] == 1024
