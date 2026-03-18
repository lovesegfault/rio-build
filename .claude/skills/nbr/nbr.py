#!/usr/bin/env python3
"""Run nix-build-remote, tee to log, return typed result.

Replaces the bash block in SKILL.md. Python because:
  - subprocess.wait() is the exit code — no PIPESTATUS/pipefail portability
  - NbrReport is the contract — downstream (merger step 5, impl gate) reads
    rc from JSON instead of grepping "✓"/"✗" in shell output

Usage:
    nbr.py <target> [--copy]
    nbr.py --schema
"""

from __future__ import annotations

import re
import subprocess
import sys
import tempfile
from pathlib import Path

from pydantic import BaseModel, Field

from _lib import cli_schema_or_run, git_try


class NbrReport(BaseModel):
    target: str
    branch: str
    rc: int = Field(description="nix-build-remote exit code")
    log_path: str | None = Field(description="Kept on red. None on green (deleted).")
    log_tail: list[str] = Field(description="Last 80 lines if red. Empty if green.")


def run(target: str, copy: bool = False) -> NbrReport:
    branch = git_try("rev-parse", "--abbrev-ref", "HEAD") or "no-git"
    toplevel = git_try("rev-parse", "--show-toplevel") or "."
    sanitized = re.sub(r"[^A-Za-z0-9]", "_", target)

    log_fd, log_path = tempfile.mkstemp(
        prefix=f"nbr-{branch}-{sanitized}-", suffix=".log", dir="/tmp"
    )

    cmd = ["nix-build-remote", "--dev", "--no-nom"]
    if copy:
        cmd.append("--copy")
    cmd += ["--", "-L", target]

    # tee: stream each line to stdout AND the log file. stderr merged (2>&1).
    with open(log_fd, "w") as logf:
        proc = subprocess.Popen(
            cmd,
            cwd=toplevel,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        assert proc.stdout is not None
        for line in proc.stdout:
            sys.stdout.write(line)
            logf.write(line)
        rc = proc.wait()

    if rc == 0:
        Path(log_path).unlink()
        return NbrReport(target=target, branch=branch, rc=0, log_path=None, log_tail=[])

    tail = Path(log_path).read_text().splitlines()[-80:]
    return NbrReport(
        target=target, branch=branch, rc=rc, log_path=log_path, log_tail=tail
    )


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if a != "--schema"]
    copy = "--copy" in args
    target = next((a for a in args if a != "--copy"), ".#ci")
    cli_schema_or_run(NbrReport, lambda: run(target, copy=copy))
