"""ADR-0025 — `synthesize` MCP tool fitness functions.

Two behaviours under test:

1. **Disabled mode (default in CI)** — no `ANTHROPIC_API_KEY` /
   `OPENAI_API_KEY` set. The tool is still registered (per ADR-0025
   evidence: rmcp `#[tool_router]` macro doesn't support per-instance
   filtering). Calling `synthesize` returns an error envelope; the
   `workspace_context.llm.active` field reads `false`.
2. **Provider-key-set mode** — `ANTHROPIC_API_KEY=fake-key` exposes
   the tool in *enabled* posture. We do **not** make the real HTTPS
   call (no live secret); instead we assert the tool returns a
   structured error from the provider HTTP layer (401 / DNS fail
   from the fake key), which proves the wiring is end-to-end and
   that `workspace_context.llm.active = true`.
"""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import json

from conftest import Project, ServerHandle
from test_workspace_context import _call_tool, _initialize


def test_synthesize_disabled_when_no_provider_key(server: ServerHandle) -> None:
    """ADR-0025 — without `*_API_KEY` the tool returns the disabled-error
    envelope (the LLM sees a clear hint to set the key)."""
    session_id = _initialize(server)
    envelope = _call_tool(
        server, session_id, "synthesize", {"query": "anything"}
    )
    assert "result" in envelope, f"missing result: {envelope}"
    contents = envelope["result"].get("content", [])
    assert contents, f"empty content: {envelope}"
    body = json.loads(contents[0]["text"])
    assert "error" in body, f"expected error envelope, got: {body}"
    assert "ANTHROPIC_API_KEY" in body["error"]
    assert "OPENAI_API_KEY" in body["error"]


def test_workspace_context_reports_llm_inactive_when_no_provider(
    server: ServerHandle,
) -> None:
    """ADR-0025 — `workspace_context.llm.active` is the canonical
    runtime signal the calling LLM checks before invoking
    `synthesize`."""
    session_id = _initialize(server)
    envelope = _call_tool(server, session_id, "workspace_context")
    assert "result" in envelope
    contents = envelope["result"].get("content", [])
    assert contents
    body = json.loads(contents[0]["text"])
    assert "llm" in body, f"workspace_context missing llm field: {body}"
    assert body["llm"]["active"] is False
    assert "provider" not in body["llm"] or body["llm"].get("provider") is None


def test_synthesize_with_anthropic_key_invokes_provider(
    spawn_server, project: Project
) -> None:
    """ADR-0025 — when `ANTHROPIC_API_KEY` is set, the provider is
    selected and the call is dispatched to the live API. With a
    deliberately-fake key, the request reaches Anthropic and is
    rejected at HTTP 401, which the adapter surfaces as
    `LlmError::Unauthorized`. We assert on either the unauthorized
    error or any provider HTTP error — both prove the
    selection-+-dispatch path is wired."""
    handle = spawn_server(
        project,
        extra_env={"ANTHROPIC_API_KEY": "sk-ant-fake-key-for-e2e"},
    )
    session_id = _initialize(handle)
    ctx_envelope = _call_tool(handle, session_id, "workspace_context")
    ctx = json.loads(ctx_envelope["result"]["content"][0]["text"])
    assert ctx["llm"]["active"] is True
    assert ctx["llm"]["provider"] == "anthropic"

    envelope = _call_tool(
        handle, session_id, "synthesize", {"query": "what is this corpus?"}
    )
    contents = envelope["result"]["content"]
    body = json.loads(contents[0]["text"])
    # The fake key never authenticates; the adapter produces an
    # error envelope. Specific status text varies (401 vs network);
    # the contract is "the call reached the provider layer" which
    # implies the disabled-mode error string is *not* present.
    assert "error" in body, f"expected provider-layer error, got: {body}"
    assert "ANTHROPIC_API_KEY" not in body["error"], (
        f"got the disabled-mode envelope; provider should have been wired: {body}"
    )
