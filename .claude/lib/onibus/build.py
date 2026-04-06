"""Remote build via ssh-ng://nxb-dev + CI-log flake-excusability check.

Lifted from nixbuild.py minus the sys.path hack — the coverage mode needed
to late-import state.py; now it's just `from onibus.models import CoverageResult`.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

from onibus import KNOWN_FLAKES, REPO_ROOT, STATE_DIR
from onibus.git_ops import git_try
from onibus.jsonl import append_jsonl, read_jsonl
from onibus.models import BuildReport, BuildRole, CoverageResult, ExcusableVerdict, KnownFlake

LOG_DIR = Path("/tmp/rio-dev")
ROLES = ("impl", "verify", "merge", "writer")


def _plan_from_branch(branch: str) -> str:
    """p214 → p0214 (zero-pad). Non-pNNN branches kept verbatim."""
    m = re.fullmatch(r"p(\d+)", branch)
    return f"p{int(m.group(1)):04d}" if m else branch


def _next_iter(plan: str, role: str) -> int:
    iters = []
    for p in LOG_DIR.glob(f"rio-{plan}-{role}-*.log"):
        tail = p.stem.rsplit("-", 1)[-1]
        if tail.isdigit():
            iters.append(int(tail))
    return max(iters, default=0) + 1


def run(
    target: str, *, role: BuildRole = "impl", copy: bool = False,
    link: bool = False, loud: bool = False,
) -> BuildReport:
    LOG_DIR.mkdir(exist_ok=True)
    # REPO_ROOT is derived from __file__ (onibus/__init__.py:24) — cwd-independent.
    # git_try already defaults to cwd=REPO_ROOT, so branch is correct even if
    # the process cwd is elsewhere. Using REPO_ROOT directly for toplevel avoids
    # a redundant git call and removes the cwd-sensitive "." fallback.
    toplevel = str(REPO_ROOT)
    branch = git_try("rev-parse", "--abbrev-ref", "HEAD") or "no-git"

    # Defensive: if cwd is in a DIFFERENT worktree, the caller probably invoked
    # the wrong onibus binary (e.g., agent in p209 ran ../main/.claude/bin/onibus).
    # This builds REPO_ROOT's flake, not cwd's — warn so the mistake is visible.
    cwd_toplevel = git_try("rev-parse", "--show-toplevel", cwd=Path.cwd())
    if cwd_toplevel and Path(cwd_toplevel).resolve() != REPO_ROOT:
        print(
            f"[onibus build] WARNING: cwd worktree ({cwd_toplevel}) != onibus REPO_ROOT ({REPO_ROOT}). "
            f"Building {REPO_ROOT}'s flake. Use {cwd_toplevel}/.claude/bin/onibus to build that worktree.",
            file=sys.stderr,
        )

    plan = _plan_from_branch(branch)
    iter_n = _next_iter(plan, role)
    log_path = LOG_DIR / f"rio-{plan}-{role}-{iter_n}.log"

    print(f"\u2192 {log_path}", file=sys.stderr, flush=True)

    # --eval-store auto: flake source is on local disk.
    # --store ssh-ng://nxb-dev: build happens on the fleet; output lands remote.
    # --no-link: no result/ symlink (path wouldn't exist locally anyway).
    # --print-out-paths: outpath(s) to stdout — captured for --copy.
    #   No re-eval (flake ref after build can drift if git HEAD moves).
    # ssh_config Host nxb-dev + wildcard User root/Port 2222 resolve the fleet HA addr.
    #
    # Supersedes the nix-build-remote wrapper. nix build --store is atomic:
    # either succeeds (output exists remote) or fails. No more "dispatch died
    # silently, rc=0 but invalid output".
    cmd = [
        "nix", "build", "--no-link", "--print-out-paths",
        "--eval-store", "auto", "--store", "ssh-ng://nxb-dev", "-L", target,
    ]

    with open(log_path, "w") as logf:
        proc = subprocess.Popen(
            cmd, cwd=toplevel, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
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
            ["nix", "copy", "--no-check-sigs", "--from", "ssh-ng://nxb-dev", *out_paths],
            cwd=toplevel, capture_output=True, text=True,
        )
        if cp.returncode != 0:
            rc = cp.returncode
            with open(log_path, "a") as f:
                f.write(f"\n[onibus build] nix copy failed: {cp.stderr}\n")
        else:
            store_path = out_paths[0]
            # --link: create ./result → /nix/store/<hash>-<name> after --copy
            # so the target exists locally. This is what plain `nix build`
            # does by default; we suppress it with --no-link because the
            # output is USUALLY remote-only. Needed for .#crds, .#coverage-html
            # — outputs the caller cp's or opens, not just checks for existence.
            if link:
                result = Path(toplevel) / "result"
                result.unlink(missing_ok=True)
                result.symlink_to(store_path)

    tail = log_path.read_text().splitlines()[-80:] if rc != 0 else []
    return BuildReport(
        target=target, branch=branch, rc=rc, log_bytes=log_bytes,
        log_path=str(log_path), store_path=store_path, log_tail=tail,
        role=role, iter=iter_n,
    )


def coverage(branch: str, merged_at: str, *, loud: bool = False) -> None:
    """Build .#coverage-full, copy local, write a CoverageResult row. Backgrounded
    by the merger — HEAD may move while this runs, so branch/merged_at are args."""
    r = run(".#coverage-full", role="merge", copy=True, loud=loud)
    cov = CoverageResult(
        branch=branch, exit_code=r.rc, log_path=r.log_path, merged_at=merged_at,
    )
    append_jsonl(STATE_DIR / "coverage-pending.jsonl", cov)
    print(cov.model_dump_json())


# ─── excusable (NEW — was implementer.md:116 prose) ──────────────────────────

# nextest FAIL line: "        FAIL [   1.234s] crate::module::test_name"
_NEXTEST_FAIL_RE = re.compile(r"^\s*FAIL\s+\[\s*[\d.]+s\]\s+(\S+)", re.MULTILINE)

# VM test drv failure: "error: Cannot build '/nix/store/<hash>-vm-test-run-<name>.drv'."
# <name> is the nixosTest `name` attr (e.g., rio-lifecycle-recovery), NOT the
# flake attr (vm-lifecycle-recovery-k3s). known-flakes.jsonl stores flake-attr
# names in `test`; drv names in `drv_name`. Match is against drv_name.
# Verified against /tmp/rio-dev/rio-p0209-impl-2.log:15780 — this is the
# top-level nix error line; the `Reason: builder failed with exit code N` is a
# SEPARATE line with no drv path.
#
# The `^error:` anchor is load-bearing: one line above in the same log is a
# debug dump `BuildResultV3 {..., errorMsg = "Cannot build '/nix/...'..."}`
# prefixed by `vm-test-run-...> [debug]` — the anchor excludes it. Without the
# anchor we'd double-count the same failure.
_VM_FAIL_RE = re.compile(
    r"^error: Cannot build '/nix/store/[a-z0-9]+-vm-test-run-([\w-]+)\.drv'",
    re.MULTILINE,
)

# 2026-03-20: HARD-STOP — KVM-denied is no longer excusable. The root cause
# was two-fold: (1) kvmOnly module's dual `-machine accel=` breaks qemu 10.2.1
# on multi-VM tests (FIXED @ 7bd70aba), (2) 7 of 13 kvm:y builders have
# /dev/kvm mode 0660 with empty snix-qemu group (nixbld can't access). (2) is
# an infra issue that must be fixed fleet-side. Until then, CI is EXPECTED to
# fail when a VM test lands on a 0660 builder — that's correct behavior.
# Retry-roulette let 180+ merges ship without VM coverage; never again.
_TCG_MARKERS = ()  # empty — no TCG excusability


def excusable(log_path: Path) -> ExcusableVerdict:
    """If .#ci is red on exactly one test AND that test is a known-flake with
    retry != Never, retry is permitted. Typed replacement for the
    grep-then-compare prose at implementer.md:116 / ci-fixer.md:30."""
    text = log_path.read_text()
    nextest_fails = sorted(set(_NEXTEST_FAIL_RE.findall(text)))
    vm_fails = sorted(set(_VM_FAIL_RE.findall(text)))  # drv names
    failing = nextest_fails + vm_fails  # order: nextest first, VM second (for reason clarity)

    flake_rows = read_jsonl(KNOWN_FLAKES, KnownFlake)
    # Two match surfaces: nextest fails match against `test` (crate::path form);
    # VM fails match against `drv_name` (rio-lifecycle-* form).
    by_test = {f.test: f for f in flake_rows}
    by_drv = {f.drv_name: f for f in flake_rows if f.drv_name}

    matched = sorted(
        set(t for t in nextest_fails if t in by_test)
        | set(d for d in vm_fails if d in by_drv)
    )
    # For reason-string: the KnownFlake object, not just the key
    matched_row = (by_test.get(matched[0]) or by_drv.get(matched[0])) if matched else None

    if not failing:
        reason, ok = "no FAIL lines (nextest) or Cannot-build lines (VM) in log", False
    elif len(failing) > 1:
        reason, ok = f"{len(failing)} failures — excusable requires exactly 1", False
    elif not matched:
        # P0304-T10 supplementary grant: VM-fail + TCG marker = infra-excusable
        # even if drv isn't in known-flakes. The 1-failure discipline above
        # already excludes co-occurring real failures (nextest FAIL + VM TCG).
        if failing[0] in vm_fails and any(m in text for m in _TCG_MARKERS):
            reason, ok = f"{failing[0]!r} TCG-marker present — builder-side KVM infra, retry", True
        else:
            reason, ok = f"{failing[0]!r} not in known-flakes.jsonl (neither test nor drv_name)", False
    elif matched_row.retry == "Never":
        reason, ok = f"{matched[0]!r} is known-flake but retry=Never — investigate, don't retry", False
    else:
        reason, ok = f"single failure {matched[0]!r} is known-flake retry={matched_row.retry}", True
    return ExcusableVerdict(
        excusable=ok, failing_tests=failing, matched_flakes=matched, reason=reason,
    )
