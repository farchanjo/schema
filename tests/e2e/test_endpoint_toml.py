"""endpoint.toml schema + permissions integrity (ADR-0019, ADR-0021)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import datetime as dt
import re

from conftest import ServerHandle


_ALLOWED_FIELDS = {"version", "url", "token", "pid", "started_at"}
_TOKEN_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
)
_URL_RE = re.compile(r"^http://127\.0\.0\.1:\d{1,5}$")


def test_no_unexpected_fields(server: ServerHandle) -> None:
    """ADR-0021 §"endpoint.toml schema" pinned. Reject unknown fields
    so a future field-add forces a code+test paired update."""
    actual = set(server.endpoint.keys())
    extra = actual - _ALLOWED_FIELDS
    assert not extra, (
        f"endpoint.toml has unexpected fields: {extra}. Add to "
        "_ALLOWED_FIELDS in this test only when the field is documented "
        "in ADR-0021."
    )


def test_token_is_uuidv4_shape(server: ServerHandle) -> None:
    """Token must look like a UUID v4 (ADR-0021 §"Token lifecycle")."""
    assert _TOKEN_RE.match(server.token), (
        f"token does not match UUIDv4 shape: {server.token!r}"
    )


def test_url_loopback_with_port(server: ServerHandle) -> None:
    """URL is `http://127.0.0.1:<port>`; no DNS, no public IP, no scheme drift."""
    assert _URL_RE.match(server.url), (
        f"url does not match loopback shape: {server.url!r}"
    )


def test_started_at_is_rfc3339(server: ServerHandle) -> None:
    """`started_at` parses as RFC 3339 (ISO 8601)."""
    raw = server.endpoint["started_at"]
    assert isinstance(raw, str)
    # Python 3.11+ fromisoformat handles full RFC 3339 incl. Z suffix.
    parsed = dt.datetime.fromisoformat(raw.replace("Z", "+00:00"))
    # Must be in the recent past (server started within this test run).
    now = dt.datetime.now(dt.timezone.utc)
    age = (now - parsed.astimezone(dt.timezone.utc)).total_seconds()
    assert -5 < age < 120, (
        f"started_at age {age}s out of range; got {parsed} now {now}"
    )


def test_pid_is_positive_integer(server: ServerHandle) -> None:
    pid = server.endpoint["pid"]
    assert isinstance(pid, int)
    assert pid > 0
