"""Local build + CI-log flake-excusability check.

nxb-dev retired 2026-03 — this host now handles x86_64+aarch64 KVM builds
directly. The ssh-ng:// remote-store path is gone; `--store` defaults to
the local daemon.

Lifted from nixbuild.py minus the sys.path hack — the coverage mode needed
to late-import state.py; now it's just `from onibus.models import CoverageResult`.
"""

from __future__ import annotations

import re
import subprocess
import sys
from collections import defaultdict
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

    # Local build — nxb-dev retired; this host does x86_64+aarch64 KVM directly.
    # --no-link: callers that want result/ use --link; store_path in the JSON
    #   report is the handle otherwise.
    # --print-out-paths: outpath(s) to stdout — captured for --copy/--link.
    cmd = [
        "nix", "build", "--no-link", "--print-out-paths", "-L", target,
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

    # Local build — outputs are already in the local store. --copy is now a
    # no-op kept for API compat; store_path is set whenever rc=0.
    store_path = out_paths[0] if (rc == 0 and out_paths) else None
    if link and store_path:
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
    by the merger — HEAD may move while this runs, so branch/merged_at are args.

    On infrastructure-class red (≥3 scenarios failed), ALSO writes
    queue-halted — the 118-commit undetected PSA break was
    all-scenarios-red triaged as individual test-gaps. halt_queue()
    blocks new /implement dispatch until the pipeline is fixed."""
    r = run(".#coverage-full", role="merge", copy=True, loud=loud)
    cov = CoverageResult(
        branch=branch, exit_code=r.rc, log_path=r.log_path, merged_at=merged_at,
    )
    append_jsonl(STATE_DIR / "coverage-pending.jsonl", cov)
    if r.rc != 0:
        # Late import: merge.py → (no build.py). build.py → merge.py
        # only here, function-local — no module-level cycle.
        from onibus.merge import coverage_maybe_halt
        coverage_maybe_halt(r.log_path)
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

# Non-VM checks.* constituent failures. Same ^error: anchor + Cannot build
# shape as _VM_FAIL_RE, but the drv name is rio-<check> directly after the
# hash (no vm-test-run- prefix). Captures the full rio-<name> for consistency
# with _VM_FAIL_RE's capture of the full vm-test-run-<scenario> suffix.
#
# Disjoint from _VM_FAIL_RE: [a-z0-9]+ does NOT include '-', so the greedy +
# halts at the first hyphen after the hash. A vm-test-run drv has -vm- there
# (not -rio-), so it cannot match here. And _VM_FAIL_RE's literal
# -vm-test-run- prefix excludes bare rio-<check> drvs. No double-count.
#
# Covers: tracey-validate, clippy, docs, coverage, fuzz, cov-smoke — any .#ci
# constituent that isn't a VM test or a nextest unit run. P0490 canary: first
# non-VM flake entry ever attempted, discovered this surface was missing.
#
# `rio-cov-vm-total` (aggregate) WOULD match here on failure, but excusable()
# is only consulted on .rc!=0 builds, extracts from ^error: Cannot-build lines
# only, and the len(failing)>1 gate below rejects cascades before by_drv lookup
# — no re-introduction of P0517's over-count.
_CHECK_FAIL_RE = re.compile(
    r"^error: Cannot build '/nix/store/[a-z0-9]+-(rio-[\w-]+)\.drv'",
    re.MULTILINE,
)

# nixbuild.net remote-builder infra errors. ^error: anchor is load-bearing
# (same rationale as _VM_FAIL_RE) — excludes `drv> ...` build-stdout relay
# where these tokens appear benignly in GREEN logs:
#   - vm-test-run-rio-chaos> ... postgres[1058]: ... Broken pipe  (client-disconnect noise)
#   - rio-test-run-rio-store> test ...pin_path_internal_error_is_scrubbed ... ok  (test NAME)
# Both observed in /tmp/rio-dev/ greens. A bare substring match would mark
# every CI run excusable.
#
# These signal the build never reached completion — ssh pipe broke mid-run,
# remote sandbox killed, server-side crash. Any co-occurring FAIL is a casualty
# of the pipe break, not a real test failure. Retry regardless of known-flakes.
#
# nxb-dev retired 2026-03 (module docstring) — still catches residual
# --store ssh-ng:// paths and anyone re-enabling remote builders.
#
# P0430 iter-1: vm-le-build-k3s was in known-flakes but excusable() returned
# false — log had a remote-builder crash signature, not a FAIL/Cannot-build
# line. Implementer blocked on a false-negative.
# 4 original patterns: nixbuild.net-era (P0447 plan-prescribed; zero corpus
# hits 2026-03). Retained — nixbuild.net may return.
# 2 corpus-derived (P0508, 289-file /tmp/rio-dev scan 2026-03-30):
#   - "failed to start SSH connection" — 8 hits at ^error: level across
#     p0450-impl-{2,3} + sprint-1-{impl-1,merge-1..5}.log. SSH connect
#     fails → no FAIL lines → tier-2 false-negative. Same class as P0430.
#     4 additional hits inside drv> relay (merge-58/59) are correctly
#     excluded by the ^error: anchor.
#   - "builder failed with exit code 137" — 2 hits (p501-t4-ci.log:18269,
#     rio-p0499-impl-1.log:50295). SIGKILL (128+9) on remote builder —
#     OOM under concurrent CI on keynes. Appears on the ^[ \t]+Reason:
#     continuation line, NOT ^error: — hence the unified-prefix alternation.
#     The plan prescribed `Killed\s*$` inside ^error:, but the corpus shows
#     the drv-relay Killed line ends with `$@` (bash setup-script output)
#     and the ^error: line carries `Cannot build` — no `Killed` at EOL
#     anywhere in 289 files. exit code 137 is the corpus-grounded marker.
#     `[ \t]+` (not `\s+`) so the anchor doesn't eat a \n.
_INFRA_ERROR_RE = re.compile(
    r"^(?:error: .*?|[ \t]+Reason: )(Broken pipe|internal_error|resource vanished"
    r"|Transient build error|failed to start SSH connection"
    r"|builder failed with exit code 137)",
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

# Retry restrictiveness: Never (no retry) > Once > Twice (most permissive).
# Lower index = more restrictive = wins when multiple entries share a drv_name.
_RETRY_RESTRICTIVENESS = ("Never", "Once", "Twice")


def _most_restrictive(entries: list[KnownFlake]) -> KnownFlake:
    """Pick the entry whose retry policy is most restrictive.

    Multiple known-flake rows legitimately share a drv_name (one VM drv,
    several subtest flakes). When the drv fails CI we can't tell WHICH subtest
    tripped, so we conservatively honor the strictest policy: if ANY entry
    says Never, treat the drv as Never. P0527.
    """
    return min(entries, key=lambda e: _RETRY_RESTRICTIVENESS.index(e.retry))


def excusable(log_path: Path) -> ExcusableVerdict:
    """If .#ci is red on exactly one test AND that test is a known-flake with
    retry != Never, retry is permitted. Typed replacement for the
    grep-then-compare prose at implementer.md:116 / ci-fixer.md:30."""
    text = log_path.read_text()
    nextest_fails = sorted(set(_NEXTEST_FAIL_RE.findall(text)))
    vm_fails = sorted(set(_VM_FAIL_RE.findall(text)))  # drv names
    check_fails = sorted(set(_CHECK_FAIL_RE.findall(text)))  # non-VM checks.* drv names
    failing = nextest_fails + vm_fails + check_fails  # order: nextest, VM, checks (for reason clarity)

    # Tier-1: nixbuild.net infra error → always excusable. Checked BEFORE
    # the failing-test gates — an ssh pipe break may produce spurious FAIL
    # lines (interrupted mid-run) or none at all (crashed before nextest
    # started). Either way the build never completed; known-flakes lookup
    # is moot. P0430's false-negative: "no FAIL lines" branch fired on
    # an infra crash.
    infra_hit = _INFRA_ERROR_RE.search(text)
    if infra_hit:
        return ExcusableVerdict(
            excusable=True, failing_tests=failing, matched_flakes=[],
            reason=f"nixbuild.net infra error {infra_hit.group(1)!r} — build never completed, retry",
        )

    # Tier-2: known-flakes lookup (existing single-failure discipline).
    flake_rows = read_jsonl(KNOWN_FLAKES, KnownFlake)
    # Three match surfaces: nextest fails match against `test` (crate::path form);
    # VM + non-VM check fails match against `drv_name` (rio-lifecycle-*, rio-tracey-validate form).
    # by_drv is list-valued: one VM drv CAN have multiple flake entries (distinct
    # subtests — e.g. rio-lifecycle-core has flannel-race + disruption-drain).
    # P0527: dict-comprehension here was last-entry-wins on dupe drv_name;
    # matched_row.retry below read a file-order-dependent entry.
    by_test = {f.test: f for f in flake_rows}
    by_drv: dict[str, list[KnownFlake]] = defaultdict(list)
    for f in flake_rows:
        if f.drv_name:
            by_drv[f.drv_name].append(f)

    matched = sorted(
        set(t for t in nextest_fails if t in by_test)
        | set(d for d in vm_fails if d in by_drv)
        | set(d for d in check_fails if d in by_drv)
    )
    # For reason-string: the KnownFlake object, not just the key.
    # drv-key lookups pick the MOST RESTRICTIVE retry policy across all
    # entries for that drv — if any entry says Never, the drv is Never.
    matched_row = None
    if matched:
        key = matched[0]
        if key in by_test:
            matched_row = by_test[key]
        elif key in by_drv:
            matched_row = _most_restrictive(by_drv[key])

    if not failing:
        reason, ok = "no FAIL lines (nextest) or Cannot-build lines (VM/check) in log", False
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
