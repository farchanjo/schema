"""Two `schema serve` processes against the same project (ADR-0008 gap,
ADR-0019 §multi-instance discussion).

These tests **document** behaviour rather than enforce it. With ADR-0020
service mode, the operator never spawns two servers per project; if
they do, we record the resulting state so future debugging is informed.
"""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import time

import httpx
import pytest

from conftest import _terminate, ServerHandle


@pytest.mark.xfail(
    reason=(
        "ADR-0008 §Concurrency anticipates two simultaneous `schema serve`. "
        "ADR-0020 service mode (one launchd/systemd unit per project) is "
        "the operator-recommended path; the binary itself does not refuse "
        "concurrent spawns. This test documents the resulting race rather "
        "than enforcing a lock — if a future ADR adds fd-locking (FASE 1.1 "
        "follow-up of ADR-0008), promote this to a real assertion."
    ),
    strict=False,
)
def test_two_simultaneous_servers_lock_each_other_out(
    spawn_server, project
) -> None:
    """Spawn two servers simultaneously. Today: both bind (different
    ports because `127.0.0.1:0`); the later spawn's endpoint.toml
    overwrites the first. **Expected to xfail until lock lands.**"""
    first = spawn_server(project)
    # Try to spawn a second; expectation if the lock landed: this fails
    # to start (or blocks). Today: succeeds.
    second = spawn_server(project)
    assert first.process.pid != second.process.pid

    # If we got here, both are running. Force-fail the xfail expectation.
    raise AssertionError(
        "Two simultaneous schema serve processes booted on the same "
        "project. Expected behaviour with fd-lock: second spawn fails "
        "or blocks. Today's behaviour: both run; race on endpoint.toml."
    )


def test_two_servers_endpoint_toml_race_documented(
    spawn_server, project
) -> None:
    """Concrete behaviour: when two servers run, endpoint.toml reflects
    the most-recent writer. The earlier server is still listening on
    its own port but unreachable via endpoint.toml lookup."""
    first = spawn_server(project)
    first_url = first.url
    first_token = first.token

    # Brief delay so the second `schema serve` writes endpoint.toml
    # *after* the first.
    time.sleep(0.5)
    second = spawn_server(project)
    second_url = second.url
    second_token = second.token

    # Endpoints differ (different kernel-assigned ports).
    assert first_url != second_url
    assert first_token != second_token

    # endpoint.toml on disk reflects the last writer (second).
    on_disk = first.project.endpoint_toml
    assert on_disk.is_file()
    # Each fixture's `endpoint` snapshot was taken at that handle's
    # spawn time; the *current file* is whichever server wrote last.
    # The second server's token must match the on-disk file.
    import tomllib

    with open(on_disk, "rb") as fh:
        current = tomllib.load(fh)
    assert current["token"] == second_token, (
        "endpoint.toml race: expected second server's token on disk, "
        "but got first's. Investigate spawn ordering."
    )

    # First server is still alive and accepting requests (its own port,
    # its own token). This is the surprising bit — endpoint.toml lookup
    # would point any new client at server 2, but server 1 is still up.
    response = httpx.get(f"{first_url}/health", timeout=10.0)
    assert response.status_code == 200, (
        "first server should still be listening on its own port"
    )
