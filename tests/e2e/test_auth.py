"""Bearer auth wiring (ADR-0021)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import httpx

from conftest import ServerHandle


def test_health_returns_200_without_auth(server: ServerHandle) -> None:
    """ADR-0019 health probe — open endpoint."""
    response = httpx.get(f"{server.url}/health", timeout=10.0)
    assert response.status_code == 200
    assert response.text == "", "health body must not leak project state"


def test_mcp_rejects_request_without_authorization_header(
    server: ServerHandle,
) -> None:
    """ADR-0021 — POST /mcp without Authorization → 401."""
    response = httpx.post(f"{server.url}/mcp", json={}, timeout=10.0)
    assert response.status_code == 401


def test_mcp_rejects_wrong_token(server: ServerHandle) -> None:
    """ADR-0021 — POST /mcp with wrong bearer → 401."""
    response = httpx.post(
        f"{server.url}/mcp",
        json={},
        headers={"Authorization": "Bearer not-the-token"},
        timeout=10.0,
    )
    assert response.status_code == 401


def test_mcp_rejects_lowercase_bearer_scheme(server: ServerHandle) -> None:
    """ADR-0021 §"Threat model on byte-equality" — exact-match prefix."""
    response = httpx.post(
        f"{server.url}/mcp",
        json={},
        headers={"Authorization": f"bearer {server.token}"},
        timeout=10.0,
    )
    assert response.status_code == 401


def test_mcp_accepts_correct_token(server: ServerHandle) -> None:
    """ADR-0021 — valid bearer reaches the rmcp layer (status not 401)."""
    response = httpx.post(
        f"{server.url}/mcp",
        json={},  # rmcp will return a JSON-RPC parse error or similar
        headers=server.auth_headers,
        timeout=10.0,
    )
    assert response.status_code != 401, (
        f"valid bearer must not be rejected by auth; got status "
        f"{response.status_code}, body {response.text[:200]}"
    )


def test_token_rotates_per_restart(spawn_server, project) -> None:
    """ADR-0021 — token regenerated on every server start."""
    first = spawn_server(project)
    first_token = first.token
    response = httpx.get(
        f"{first.url}/health", timeout=10.0
    )  # ensure server is alive
    assert response.status_code == 200
    # Stop and restart.
    from conftest import _terminate  # type: ignore

    _terminate(first, hard=False)

    second = spawn_server(project)
    assert second.token != first_token, (
        "ADR-0021 §'Token rotates on every server restart' violated"
    )
    # Old token must not authenticate against the new server.
    rejected = httpx.post(
        f"{second.url}/mcp",
        json={},
        headers={"Authorization": f"Bearer {first_token}"},
        timeout=10.0,
    )
    assert rejected.status_code == 401


def test_authorization_header_redacted_in_stderr_log(
    server: ServerHandle,
) -> None:
    """ADR-0021 §"Why redact `Authorization`" — bearer must NOT appear
    in stderr log; SetSensitiveRequestHeadersLayer wraps before TraceLayer."""
    # Trigger at least one logged request.
    httpx.post(
        f"{server.url}/mcp",
        json={"jsonrpc": "2.0", "method": "initialize", "id": 1},
        headers=server.auth_headers,
        timeout=10.0,
    )
    # Give tracing a beat to flush.
    import time as _t

    _t.sleep(0.5)
    body = server.stderr_log.read_text()
    assert server.token not in body, (
        "bearer token leaked into stderr log — sensitive-headers layer "
        "must apply before trace layer (ADR-0021)"
    )
