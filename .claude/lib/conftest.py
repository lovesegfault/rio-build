"""Pytest fixtures for onibus. No sys.path hacks — pytest adds the
conftest dir to sys.path automatically, and onibus/ is a sibling."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest


@pytest.fixture
def tmp_repo(tmp_path: Path) -> Path:
    """A minimal git repo with one commit on the integration branch.
    Includes .claude/integration-branch so modules that read it at import
    time don't crash when tests re-import onibus against this repo."""
    from onibus import INTEGRATION_BRANCH
    subprocess.run(["git", "init", "-b", INTEGRATION_BRANCH], cwd=tmp_path, check=True)
    subprocess.run(["git", "config", "user.email", "t@t"], cwd=tmp_path, check=True)
    subprocess.run(["git", "config", "user.name", "t"], cwd=tmp_path, check=True)
    (tmp_path / "README").write_text("x")
    (tmp_path / ".claude").mkdir()
    (tmp_path / ".claude" / "integration-branch").write_text(f"{INTEGRATION_BRANCH}\n")
    subprocess.run(["git", "add", "-A"], cwd=tmp_path, check=True)
    subprocess.run(
        ["git", "commit", "-m", "init", "--no-verify"], cwd=tmp_path, check=True
    )
    return tmp_path


@pytest.fixture
def plan_doc_full_paths(tmp_path: Path) -> Path:
    """Plan doc with rio-*/src/*.rs paths in T-item prose."""
    doc = tmp_path / "plan-0999-test.md"
    doc.write_text(
        "# Plan 999\n\n"
        "### T1 — `fix(scheduler):` Sort entries\n\n"
        "[`rio-scheduler/src/actor/completion.rs:429`](...): the walk.\n\n"
        "### T2 — `perf(store):` Prealloc\n\n"
        "See `rio-store/src/manifest.rs` line 230.\n\n"
        "## Files\n\n"
        "Tree style (bare names, won't match):\n"
        "```\n"
        "rio-scheduler/src/\n"
        "├── actor.rs\n"
        "```\n"
    )
    return doc


@pytest.fixture
def plan_doc_bare_names(tmp_path: Path) -> Path:
    """Plan doc with ONLY bare names — no full paths anywhere."""
    doc = tmp_path / "plan-0998-bare.md"
    doc.write_text(
        "# Plan 998\n\n"
        "### T1 — `fix(gateway):` Warn on URL\n\n"
        "Edit `handshake.rs` to emit the warning.\n\n"
        "## Files\n\n"
        "```\n"
        "opcodes/query.rs\n"
        "resolve.rs\n"
        "```\n"
    )
    return doc
