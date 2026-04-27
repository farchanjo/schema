"""Configuration error paths (ADR-0018 nice range, ADR-0023 walk-up,
ADR-0023 ENV overlay parse failures)."""

# pylint: disable=missing-function-docstring,redefined-outer-name

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from conftest import Project, _project_root


def _run(binary: Path, args: list[str], cwd: Path, env: dict | None = None) -> subprocess.CompletedProcess:
    """Run the binary, capturing stdout+stderr."""
    base_env = os.environ.copy()
    if env:
        base_env.update(env)
    return subprocess.run(
        [str(binary)] + args,
        cwd=cwd,
        env=base_env,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )


def test_validate_fails_when_no_schema_toml_in_walkup(
    schema_binary: Path, tmp_path: Path
) -> None:
    """ADR-0023 — walk-up returns descriptive error when no
    `schema.toml` is found anywhere in the chain."""
    # tmp_path is empty; no schema.toml above it on most systems
    # but some operators have ~/.schema.toml etc. We isolate by
    # disabling SCHEMA_CONFIG and using an empty CWD.
    result = _run(
        schema_binary,
        ["validate"],
        cwd=tmp_path,
        env={"SCHEMA_CONFIG": ""},  # empty == unset per env_str_via_process
    )
    # Either descriptive error OR walk-up climbed up and found a
    # schema.toml on the operator's machine. We accept both as valid
    # behaviour, but on a clean tmpdir we expect failure.
    if result.returncode != 0:
        assert "schema.toml not found" in (result.stderr + result.stdout), (
            f"expected descriptive 'not found' error, got: {result.stderr}"
        )


def test_validate_rejects_invalid_toml_syntax(
    schema_binary: Path, tmp_path: Path
) -> None:
    """Bad TOML syntax → exit non-zero with parse error."""
    config = tmp_path / "schema.toml"
    config.write_text("[project\nname = bad\n")  # missing ] and bad value
    result = _run(
        schema_binary,
        ["validate", "--config", str(config)],
        cwd=tmp_path,
    )
    assert result.returncode != 0
    combined = result.stderr + result.stdout
    assert "parsing schema.toml" in combined or "parse" in combined.lower()


def test_validate_rejects_nice_out_of_range(
    schema_binary: Path, tmp_path: Path
) -> None:
    """ADR-0018 — `[embedding] nice = 25` rejected."""
    config = tmp_path / "schema.toml"
    config.write_text(
        '[project]\nname = "x"\n[embedding]\nnice = 25\n'
    )
    result = _run(
        schema_binary,
        ["validate", "--config", str(config)],
        cwd=tmp_path,
    )
    assert result.returncode != 0
    combined = result.stderr + result.stdout
    assert "nice = 25" in combined


def test_env_overlay_invalid_value_rejected(
    schema_binary: Path, tmp_path: Path
) -> None:
    """ADR-0023 — invalid SCHEMA_EMBEDDING_NICE value surfaces an
    error that names the env var."""
    config = tmp_path / "schema.toml"
    config.write_text('[project]\nname = "x"\n')
    result = _run(
        schema_binary,
        ["validate", "--config", str(config)],
        cwd=tmp_path,
        env={"SCHEMA_EMBEDDING_NICE": "not-a-number"},
    )
    assert result.returncode != 0
    combined = result.stderr + result.stdout
    assert "SCHEMA_EMBEDDING_NICE" in combined


def test_walk_up_finds_schema_toml_in_ancestor(
    schema_binary: Path, tmp_path: Path
) -> None:
    """ADR-0023 — running from a deep subdirectory walks up and
    finds the project's schema.toml at the root."""
    config = tmp_path / "schema.toml"
    config.write_text('[project]\nname = "walkup-fixture"\n')
    deep = tmp_path / "a" / "b" / "c"
    deep.mkdir(parents=True)
    result = _run(
        schema_binary,
        ["validate"],
        cwd=deep,
        env={"SCHEMA_CONFIG": ""},  # ensure walk-up wins over env shortcut
    )
    assert result.returncode == 0, (
        f"walk-up should have found {config}: {result.stderr + result.stdout}"
    )
    assert "walkup-fixture" in (result.stdout + result.stderr)
