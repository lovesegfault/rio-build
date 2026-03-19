#!/usr/bin/env python3
"""Remote build via nix build --store ssh-ng://nxb-dev. Log to /tmp/rio-dev/.

Logs: /tmp/rio-dev/rio-{plan}-{role}-{iter}.log
  {plan}  from branch: p214 -> p0214; non-pNNN branches kept verbatim
  {role}  --role {impl,verify,merge,writer}, default impl
  {iter}  auto-increments per (plan, role) pair

Quiet by default: prints log path to stderr at start, BuildReport JSON to
stdout at end. No tee. Caller can `tail -f` the path if impatient.
--loud restores tee-to-stderr for interactive debugging.

Callers read the JSON (jq .rc, .log_tail, .store_path). Process exit is
always 0 -- rc is in the model, not the shell exit code.

Usage:
    nixbuild.py <target> [--copy] [--role R] [--loud]
    nixbuild.py --rio-coverage <branch> <merged_at>
    nixbuild.py --schema
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


class BuildReport(BaseModel):
    target: str
    branch: str
    rc: int = Field(description="0 green; nonzero build or copy failed")
    log_bytes: int = Field(description="Build-log size at proc.wait()")
    log_path: str = Field(
        description="Always kept. /tmp/rio-dev/rio-{plan}-{role}-{iter}.log"
    )
    store_path: str | None = Field(
        default=None,
        description="Set when --copy succeeded. No result/ symlink -- this is the handle.",
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
) -> BuildReport:
    LOG_DIR.mkdir(exist_ok=True)

    branch = git_try("rev-parse", "--abbrev-ref", "HEAD") or "no-git"
    toplevel = git_try("rev-parse", "--show-toplevel") or "."
    plan = _plan_from_branch(branch)
    iter_n = _next_iter(plan, role)
    log_path = LOG_DIR / f"rio-{plan}-{role}-{iter_n}.log"

    # Print log path upfront so caller can tail -f in another terminal.
    # stderr: this is progress, not data. stdout stays clean for JSON.
    print(f"\u2192 {log_path}", file=sys.stderr, flush=True)

    # --eval-store auto: flake source is on local disk.
    # --store ssh-ng://nxb-dev: build happens on the fleet; output lands remote.
    # --no-link: no result/ symlink (path wouldn't exist locally anyway).
    # --print-out-paths: outpath(s) to stdout — captured for --copy. No re-eval
    #   (re-evaluating the flake ref after the build can drift if git HEAD moves).
    # ssh_config Host nxb-dev + wildcard User root/Port 2222 resolve the fleet HA addr.
    #
    # Supersedes nix-build-remote wrapper (rix 79cc8f7). nix build --store is
    # atomic: either succeeds (output exists remote) or fails. No more "dispatch
    # died silently, rc=0 but invalid output" (the rix P172 failure mode).
    cmd = [
        "nix",
        "build",
        "--no-link",
        "--print-out-paths",
        "--eval-store",
        "auto",
        "--store",
        "ssh-ng://nxb-dev",
        "-L",
        target,
    ]

    # -L writes build logs to stderr; --print-out-paths writes to stdout.
    # Keep them separate — stream stderr to the log, capture stdout at the end.
    # stdout is tiny (one path per output); won't overflow the pipe buffer
    # while we're iterating stderr.
    with open(log_path, "w") as logf:
        proc = subprocess.Popen(
            cmd,
            cwd=toplevel,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        assert proc.stderr is not None
        for line in proc.stderr:
            if loud:
                sys.stderr.write(line)
            logf.write(line)
        rc = proc.wait()
        assert proc.stdout is not None
        out_paths = proc.stdout.read().strip().splitlines() if rc == 0 else []

    log_bytes = log_path.stat().st_size

    store_path = None
    if rc == 0 and copy and out_paths:
        cp = subprocess.run(
            [
                "nix",
                "copy",
                "--no-check-sigs",
                "--from",
                "ssh-ng://nxb-dev",
                *out_paths,
            ],
            cwd=toplevel,
            capture_output=True,
            text=True,
        )
        if cp.returncode != 0:
            rc = cp.returncode
            with open(log_path, "a") as f:
                f.write(f"\n[nixbuild] nix copy failed: {cp.stderr}\n")
        else:
            store_path = out_paths[0]

    tail = log_path.read_text().splitlines()[-80:] if rc != 0 else []
    return BuildReport(
        target=target,
        branch=branch,
        rc=rc,
        log_bytes=log_bytes,
        log_path=str(log_path),
        store_path=store_path,
        log_tail=tail,
        role=role,
        iter=iter_n,
    )


def rio_coverage(branch: str, merged_at: str, *, loud: bool = False) -> None:
    """Build .#coverage-full, copy it local, write a CoverageResult row.

    branch/merged_at are passed in because this runs from main/ AFTER the
    ff-merge -- cwd branch is "main" and HEAD may move while we're backgrounded.
    """
    # Lazy import: only this mode needs state.py.
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "lib"))
    from state import CoverageResult, STATE_DIR, append_jsonl

    r = run(".#coverage-full", role="merge", copy=True, loud=loud)
    cov = CoverageResult(
        branch=branch,
        exit_code=r.rc,
        log_path=r.log_path,
        merged_at=merged_at,
    )
    append_jsonl(STATE_DIR / "coverage-pending.jsonl", cov)
    print(cov.model_dump_json())


if __name__ == "__main__":
    # --schema shortcut before argparse (keeps it working with no other args).
    if "--schema" in sys.argv:
        print(json.dumps(BuildReport.model_json_schema(), indent=2))
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
        help="nix copy the output to local store; sets store_path in JSON",
    )
    p.add_argument(
        "--loud",
        action="store_true",
        help="tee build output to stderr (for debugging)",
    )
    p.add_argument(
        "--rio-coverage",
        nargs=2,
        metavar=("BRANCH", "MERGED_AT"),
        help="Build .#coverage-full, copy, write CoverageResult to coverage-pending.jsonl. "
        "Target/role/copy are fixed; only --loud is respected.",
    )
    args = p.parse_args()

    if args.rio_coverage:
        rio_coverage(*args.rio_coverage, loud=args.loud)
    else:
        report = run(args.target, role=args.role, copy=args.copy, loud=args.loud)
        # Compact single-line JSON: quiet mode's whole point is a 2-line footprint.
        # Loud mode gets indent=2 -- you're already in a wall of text, readability wins.
        if args.loud:
            print(report.model_dump_json(indent=2))
        else:
            print(report.model_dump_json())
