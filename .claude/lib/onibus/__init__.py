"""onibus — consolidated CLI toolkit for the rio-build DAG harness.

Single package replacing the symlink-and-sys.path scatter (state.py + _lib.py +
6 per-skill scripts). All agent compound-actions, state-file I/O, and DAG graph
queries go through here. `.claude/bin/onibus <group> <cmd>` is the entrypoint.

REPO_ROOT resolves per-worktree via __file__ — same semantics as the old
relative-invocation discipline. Import this package via the bin shim, which
sets sys.path.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from pydantic import BaseModel

# ─── paths ───────────────────────────────────────────────────────────────────
# __file__ = .claude/lib/onibus/__init__.py → parents[3] = repo root.
# The bin shim (.claude/bin/onibus) adds .claude/lib to sys.path, so
# `import onibus` resolves to whichever worktree's package you invoked from.
REPO_ROOT = Path(__file__).resolve().parents[3]
CLAUDE_DIR = REPO_ROOT / ".claude"
STATE_DIR = CLAUDE_DIR / "state"
WORK_DIR = CLAUDE_DIR / "work"
DAG_JSONL = CLAUDE_DIR / "dag.jsonl"
KNOWN_FLAKES = CLAUDE_DIR / "known-flakes.jsonl"
PLAN_DOC_GLOB = ".claude/work/plan-{n:04d}-*.md"

# Merge target for the current sprint (sprint-1, sprint-2, …). Committed file
# — every worktree sees it. Mergers ff-advance this branch; impls rebase
# against it. main stays at the last stable cut.
INTEGRATION_BRANCH = (CLAUDE_DIR / "integration-branch").read_text().strip()


# ─── CLI glue ────────────────────────────────────────────────────────────────


def emit(model: BaseModel, schema_flag: bool = False) -> None:
    """Print model as JSON, or its schema if schema_flag. Always to stdout."""
    if schema_flag:
        print(json.dumps(type(model).model_json_schema(), indent=2))
    else:
        print(model.model_dump_json(indent=2))


def cli_schema_or_run(model_cls: type[BaseModel], run_fn):
    """Standard CLI pattern: --schema prints the output schema, otherwise run."""
    if "--schema" in sys.argv:
        print(json.dumps(model_cls.model_json_schema(), indent=2))
        sys.exit(0)
    result = run_fn()
    print(result.model_dump_json(indent=2))
