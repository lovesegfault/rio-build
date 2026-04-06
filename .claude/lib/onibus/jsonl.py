"""JSONL primitives — read/append/consume/remove.

Pydantic models ARE the contract: producer writes .model_dump_json(), consumer
reads .model_validate_json(). Same class both sides. Can't drift. Validation at
the boundary is the guarantee. (The COV_EXIT= bug: merger wrote COV_PID=,
dag_tick grepped COV_EXIT=, coverage regressions silently invisible. Root cause
is stringly-typed agent boundaries. This module is the fix.)
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Iterable, TypeVar

from pydantic import BaseModel

T = TypeVar("T", bound=BaseModel)


def atomic_write_text(path: Path, text: str) -> None:
    """tmp+rename — crash mid-write leaves the old file intact, no torn
    reads. os.replace is atomic on POSIX (same-filesystem rename). The
    .tmp sibling is what a crash leaves behind; safe to delete."""
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(text)
    os.replace(tmp, path)


def write_jsonl(path: Path, rows: Iterable[BaseModel], *, header: list[str] | None = None) -> None:
    """Atomic full-file jsonl rewrite."""
    lines = (header or []) + [r.model_dump_json() for r in rows]
    atomic_write_text(path, "\n".join(lines) + ("\n" if lines else ""))


def read_jsonl(path: Path, model: type[T]) -> list[T]:
    if not path.exists():
        return []
    return [
        model.model_validate_json(line)
        for line in path.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    ]
    # '#' skip lets us put a header comment in known-flakes.jsonl


def append_jsonl(path: Path, row: BaseModel) -> None:
    # Single-line append is atomic for < PIPE_BUF (4096). Rows are ~200 bytes.
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as f:
        f.write(row.model_dump_json() + "\n")


def consume_jsonl(path: Path, model: type[T]) -> list[T]:
    # Read-all-then-truncate. For coverage-pending (process-once semantics).
    rows = read_jsonl(path, model)
    if path.exists():
        path.write_text("")
    return rows


def remove_jsonl(path: Path, model: type[T], pred) -> int:
    # Read, filter, rewrite atomically. For known-flake deletion.
    rows = read_jsonl(path, model)
    keep = [r for r in rows if not pred(r)]
    header: list[str] = []
    if path.exists():
        header = [ln for ln in path.read_text().splitlines() if ln.startswith("#")]
    write_jsonl(path, keep, header=header)
    return len(rows) - len(keep)
