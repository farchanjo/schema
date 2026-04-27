"""Shared pytest fixtures for the schema E2E suite (ADR-0024).

Lifecycle:

1. Locate the `schema` binary (prefer `target/release/schema`,
   fall back to `target/debug/schema`, override via env).
2. Per-test (or per-session, when explicit) build a tempdir
   project root with `schema.toml` + sample markdown files.
3. Spawn `schema serve --config <tmp>/schema.toml`.
4. Poll `<cache>/projects/<id>/endpoint.toml` until it appears
   or timeout (60 s — first-run includes model download).
5. Hand the test the parsed `Endpoint` (URL + token + PID).
6. Teardown: SIGTERM, wait, assert clean shutdown.

The model cache is shared across tests via `SCHEMA_E2E_CACHE_DIR`
(default: operator's `~/.cache/schema/`). Tempdir cache works but
re-downloads BGE-M3 (~2 GB) per session — slow but isolated.
"""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import dataclasses
import os
import shutil
import signal
import subprocess
import time
import tomllib
from collections.abc import Generator
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Binary + project root resolution
# ---------------------------------------------------------------------------


def _project_root() -> Path:
    """Walk up from this file to the cargo project root."""
    here = Path(__file__).resolve()
    for ancestor in here.parents:
        if (ancestor / "Cargo.toml").is_file():
            return ancestor
    raise RuntimeError("Could not locate Cargo.toml ancestor of conftest.py")


def _resolve_binary() -> Path:
    override = os.environ.get("SCHEMA_E2E_BINARY")
    if override:
        path = Path(override)
        if not path.is_file():
            raise RuntimeError(f"SCHEMA_E2E_BINARY={override} does not exist")
        return path
    root = _project_root()
    for candidate in (
        root / "target" / "release" / "schema",
        root / "target" / "debug" / "schema",
    ):
        if candidate.is_file():
            return candidate
    raise RuntimeError(
        "schema binary not found. Run `cargo build --release` first, "
        "or set SCHEMA_E2E_BINARY=/path/to/schema."
    )


@pytest.fixture(scope="session")
def schema_binary() -> Path:
    return _resolve_binary()


@pytest.fixture(scope="session")
def model_cache_dir(tmp_path_factory: pytest.TempPathFactory) -> Path:
    """Resolve the BGE-M3 model cache.

    `SCHEMA_E2E_CACHE_DIR` lets the operator point at the production
    `~/.cache/schema/` so tests reuse the already-downloaded model.
    Otherwise, fall back to a session tempdir (slow first run).
    """
    override = os.environ.get("SCHEMA_E2E_CACHE_DIR")
    if override:
        path = Path(override).expanduser()
        path.mkdir(parents=True, exist_ok=True)
        return path
    return tmp_path_factory.mktemp("schema_e2e_cache")


# ---------------------------------------------------------------------------
# Project tempdir builder
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Project:
    """A throwaway consumer project on disk for one test."""

    root: Path
    config: Path
    cache_dir: Path  # XDG_CACHE_HOME hint (Linux) — ignored on macOS
    project_id: str
    project_cache_dir: Path  # absolute, OS-specific (parsed from binary output)

    @property
    def endpoint_toml(self) -> Path:
        """Exact endpoint.toml path for this project, no glob."""
        return self.project_cache_dir / "endpoint.toml"


def _resolve_project_state(
    binary: Path, config: Path, cache_dir: Path
) -> tuple[str, Path]:
    """Run `schema validate` and parse the canonical `project id` and
    `cache dir` lines from its output.

    Why parse from the binary instead of computing in Python:

    - `project_id = sanitise(name) + "-" + blake3(canonical_path)[:16]`
      (ADR-0008) — re-implementing blake3 in Python would add a dep.
    - The cache dir is OS-specific. `dirs::cache_dir()` returns
      `~/Library/Caches/` on macOS regardless of `XDG_CACHE_HOME`,
      `$XDG_CACHE_HOME/` on Linux. The binary already prints the
      resolved value; conftest parses it instead of duplicating the
      per-OS rules.
    """
    env = os.environ.copy()
    env["XDG_CACHE_HOME"] = str(cache_dir)
    env["SCHEMA_CONFIG"] = ""
    result = subprocess.run(
        [str(binary), "validate", "--config", str(config)],
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
        check=False,
    )
    combined = result.stdout + result.stderr
    if result.returncode != 0:
        raise RuntimeError(
            f"`schema validate` failed during project state resolution.\n"
            f"stderr: {result.stderr}\n"
            f"stdout: {result.stdout}"
        )
    project_id: str | None = None
    project_cache_dir: Path | None = None
    for line in combined.splitlines():
        stripped = line.strip()
        if stripped.startswith("project id"):
            project_id = stripped.split(":", 1)[1].strip()
        elif stripped.startswith("cache dir"):
            project_cache_dir = Path(stripped.split(":", 1)[1].strip())
    if project_id is None or project_cache_dir is None:
        raise RuntimeError(
            f"`schema validate` did not print expected fields. "
            f"project_id={project_id!r}, cache_dir={project_cache_dir!r}\n"
            f"output:\n{combined}"
        )
    return project_id, project_cache_dir


def _build_project(binary: Path, tmp: Path, cache_dir: Path) -> Project:
    """Construct a minimal valid consumer project + resolve its id +
    OS-specific cache dir."""
    project_root = tmp / "project"
    project_root.mkdir()
    (project_root / "docs").mkdir()
    (project_root / "docs" / "intro.md").write_text(
        "# Intro\n\nE2E fixture markdown.\n"
    )
    fixture_src = (
        Path(__file__).resolve().parent / "fixtures" / "schema_minimal.toml"
    )
    config = project_root / "schema.toml"
    shutil.copy(fixture_src, config)
    project_id, project_cache_dir = _resolve_project_state(
        binary, config, cache_dir
    )
    return Project(
        root=project_root,
        config=config,
        cache_dir=cache_dir,
        project_id=project_id,
        project_cache_dir=project_cache_dir,
    )


@pytest.fixture
def project(
    schema_binary: Path, tmp_path: Path, model_cache_dir: Path
) -> Project:
    """Build a per-test project rooted in `tmp_path`.

    The HOME-mapped cache is shared (model_cache_dir) so the BGE-M3
    weights are not re-downloaded per test. Project-scoped state
    (`store.db`, `endpoint.toml`, `metadata.toml`) lands under the
    same shared cache because `XDG_CACHE_HOME` points at it; that
    mirrors the operator's real-world setup. The `project_id` is
    resolved by asking the binary (`schema validate`) so the
    `endpoint.toml` lookup is deterministic per test.
    """
    return _build_project(schema_binary, tmp_path, model_cache_dir)


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class ServerHandle:
    """One running `schema serve` process."""

    process: subprocess.Popen[bytes]
    project: Project
    endpoint: dict
    stderr_log: Path

    @property
    def url(self) -> str:
        return self.endpoint["url"]

    @property
    def token(self) -> str:
        return self.endpoint["token"]

    @property
    def auth_headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.token}"}


def _spawn(
    binary: Path,
    project: Project,
    extra_env: dict[str, str] | None = None,
    capture_stderr: bool = True,
) -> ServerHandle:
    env = os.environ.copy()
    env["XDG_CACHE_HOME"] = str(project.cache_dir)
    env["RUST_LOG"] = env.get("RUST_LOG", "info")
    if extra_env:
        env.update(extra_env)

    stderr_log = project.root / "stderr.log"
    stderr_target = (
        open(stderr_log, "wb") if capture_stderr else subprocess.DEVNULL
    )

    process = subprocess.Popen(
        [str(binary), "serve", "--config", str(project.config)],
        cwd=project.root,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=stderr_target,
    )

    # Poll for endpoint.toml authored by *this* process. A stale
    # endpoint.toml from a previous spawn (same project_id, hard-killed
    # last time, or a sibling spawn in the multi-instance test) would
    # otherwise leak into this fixture. Match by `pid` field.
    endpoint_path = project.endpoint_toml
    deadline = time.monotonic() + 90.0
    endpoint: dict | None = None
    while time.monotonic() < deadline:
        if endpoint_path.is_file():
            try:
                with open(endpoint_path, "rb") as fh:
                    candidate = tomllib.load(fh)
            except (OSError, tomllib.TOMLDecodeError):
                # Mid-write half-state; retry.
                time.sleep(0.1)
                continue
            if int(candidate.get("pid", -1)) == process.pid:
                endpoint = candidate
                break
        if process.poll() is not None:
            stderr_body = (
                stderr_log.read_text() if stderr_log.exists() else "(none)"
            )
            raise RuntimeError(
                f"schema server exited before writing endpoint.toml "
                f"(rc={process.returncode}). stderr:\n{stderr_body}"
            )
        time.sleep(0.25)
    if endpoint is None:
        process.kill()
        process.wait(timeout=5)
        stderr_body = (
            stderr_log.read_text() if stderr_log.exists() else "(none)"
        )
        raise TimeoutError(
            "schema server did not write a matching endpoint.toml "
            f"within 90 s. expected pid={process.pid} at "
            f"{endpoint_path}. stderr:\n{stderr_body}"
        )

    return ServerHandle(
        process=process,
        project=project,
        endpoint=endpoint,
        stderr_log=stderr_log,
    )


def _terminate(handle: ServerHandle, *, hard: bool = False) -> int:
    """Send SIGTERM (or SIGKILL on hard) and wait for the process."""
    if handle.process.poll() is not None:
        return handle.process.returncode
    sig = signal.SIGKILL if hard else signal.SIGTERM
    handle.process.send_signal(sig)
    try:
        return handle.process.wait(timeout=15.0)
    except subprocess.TimeoutExpired:
        handle.process.kill()
        return handle.process.wait(timeout=5.0)


@pytest.fixture
def server(
    schema_binary: Path, project: Project
) -> Generator[ServerHandle, None, None]:
    """Spawn one server, hand it to the test, terminate on teardown."""
    handle = _spawn(schema_binary, project)
    try:
        yield handle
    finally:
        _terminate(handle)


@pytest.fixture
def spawn_server(schema_binary: Path):
    """Factory for tests that need to spawn the server explicitly.

    Useful for `test_lifecycle.py` (asserts on first-spawn behaviour),
    `test_multi_instance.py` (spawns 2), `test_config_errors.py`
    (expects spawn to FAIL).
    """
    handles: list[ServerHandle] = []

    def factory(
        project: Project,
        extra_env: dict[str, str] | None = None,
    ) -> ServerHandle:
        handle = _spawn(schema_binary, project, extra_env=extra_env)
        handles.append(handle)
        return handle

    yield factory
    for handle in handles:
        _terminate(handle)
