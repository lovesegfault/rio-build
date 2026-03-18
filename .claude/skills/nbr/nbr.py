#!/usr/bin/env python3
"""Run nix-build-remote, log to /tmp/rio-dev/, return typed result.

Logs: /tmp/rio-dev/rio-{plan}-{role}-{iter}.log
  {plan}  from branch: p214 -> p0214; non-pNNN branches kept verbatim
  {role}  --role {impl,verify,merge,writer}, default impl
  {iter}  auto-increments per (plan, role) pair

Quiet by default: prints log path to stderr at start, NbrReport JSON to
stdout at end. No tee. Caller can `tail -f` the path if impatient.
--loud restores the old tee-to-stderr for interactive debugging.

Logs are always kept -- even on green. log_path is always set.

Python (not bash) because:
  - subprocess.wait() is the exit code -- no PIPESTATUS/pipefail portability
  - NbrReport is the contract -- downstream (merger step 5, impl gate) reads
    rc from JSON instead of grepping glyphs in shell output

Usage:
    nbr.py <target> [--copy] [--role R] [--loud]
    nbr.py --schema
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, Field

from _lib import git_try

LOG_DIR = Path("/tmp/rio-dev")
ROLES = ("impl", "verify", "merge", "writer")
Role = Literal["impl", "verify", "merge", "writer"]


class NbrReport(BaseModel):
    target: str
    branch: str
    rc: int = Field(
        description="0 green; 1 build fail; 3 rc=0 but nix path-info rejects target"
    )
    log_bytes: int = Field(
        description="Build-log size at proc.wait(). Real .#ci is ~100KB+; <5KB with rc=0 is the P172 smell."
    )
    log_path: str = Field(
        description="Always kept. /tmp/rio-dev/rio-{plan}-{role}-{iter}.log"
    )
    log_tail: list[str] = Field(description="Last 80 lines if red. Empty if green.")
    role: Role
    iter: int


def _plan_from_branch(branch: str) -> str:
    """p214 -> p0214 (zero-pad to match plan-doc 4-digit scheme).
    Non-pNNN branches kept verbatim: docs-171650 -> docs-171650."""
    m = re.fullmatch(r"p(\d+)", branch)
    return f"p{int(m.group(1)):04d}" if m else branch


def _next_iter(plan: str, role: str) -> int:
    """Find max existing iter for (plan, role), return +1. Starts at 1."""
    iters = []
    for p in LOG_DIR.glob(f"rio-{plan}-{role}-*.log"):
        tail = p.stem.rsplit("-", 1)[-1]
        if tail.isdigit():
            iters.append(int(tail))
    return max(iters, default=0) + 1


def run(
    target: str,
    *,
    role: Role = "impl",
    copy: bool = False,
    loud: bool = False,
) -> NbrReport:
    LOG_DIR.mkdir(exist_ok=True)

    branch = git_try("rev-parse", "--abbrev-ref", "HEAD") or "no-git"
    toplevel = git_try("rev-parse", "--show-toplevel") or "."
    plan = _plan_from_branch(branch)
    iter_n = _next_iter(plan, role)
    log_path = LOG_DIR / f"rio-{plan}-{role}-{iter_n}.log"

    # Print log path upfront so caller can tail -f in another terminal.
    # stderr: this is progress, not data. stdout stays clean for JSON.
    print(f"\u2192 {log_path}", file=sys.stderr, flush=True)

    cmd = ["nix-build-remote", "--dev", "--no-nom"]
    if copy:
        cmd.append("--copy")
    cmd += ["--", "-L", target]

    with open(log_path, "w") as logf:
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
            if loud:
                sys.stderr.write(line)
            logf.write(line)
        rc = proc.wait()

    # Capture build-log size before verification appends.
    # This is the smell detector -- dispatch-died logs are tiny.
    log_bytes = log_path.stat().st_size

    # Verify the build actually produced a valid output before trusting rc=0.
    # Builds run on nixbuildnet; outputs land remote, local store has nothing.
    # --eval-store auto (flake src is local) + --store ssh-ng://nxb-dev
    # (ssh_config Host nxb-dev + wildcard User root/Port 2222 resolves it).
    # (rix P172 incident: 998-byte log, rc=0, invalid output -- dispatch died silently.)
    if rc == 0:
        check = subprocess.run(
            [
                "nix",
                "path-info",
                "--eval-store",
                "auto",
                "--store",
                "ssh-ng://nxb-dev",
                target,
            ],
            cwd=toplevel,
            capture_output=True,
            text=True,
        )
        if check.returncode != 0:
            rc = 3
            with open(log_path, "a") as f:
                f.write(
                    f"\n[nbr] FAIL: rc=0 but nix path-info {target} invalid: {check.stderr}\n"
                )

    tail = log_path.read_text().splitlines()[-80:] if rc != 0 else []
    return NbrReport(
        target=target,
        branch=branch,
        rc=rc,
        log_bytes=log_bytes,
        log_path=str(log_path),
        log_tail=tail,
        role=role,
        iter=iter_n,
    )


if __name__ == "__main__":
    # --schema shortcut before argparse (keeps it working with no other args).
    if "--schema" in sys.argv:
        print(json.dumps(NbrReport.model_json_schema(), indent=2))
        sys.exit(0)

    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("target", nargs="?", default=".#ci")
    p.add_argument("--role", choices=ROLES, default="impl")
    p.add_argument(
        "--copy",
        action="store_true",
        help="pull outputs to local store (coverage lcov, etc.)",
    )
    p.add_argument(
        "--loud",
        action="store_true",
        help="tee build output to stderr (old behavior, for debugging)",
    )
    args = p.parse_args()

    report = run(args.target, role=args.role, copy=args.copy, loud=args.loud)
    # Compact single-line JSON: quiet mode's whole point is a 2-line footprint.
    # Loud mode gets indent=2 -- you're already in a wall of text, readability wins.
    if args.loud:
        print(report.model_dump_json(indent=2))
    else:
        print(report.model_dump_json())
