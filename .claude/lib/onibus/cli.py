"""Argparse dispatch — grouped subparsers, `--schema` everywhere.

`main()` is the grouped interface: `onibus dag render`, `onibus merge lock`.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict
from datetime import datetime
from pathlib import Path

from pydantic import ValidationError

from onibus import (
    DAG_JSONL,
    INTEGRATION_BRANCH,
    KNOWN_FLAKES,
    REPO_ROOT,
    STATE_DIR,
    WORK_DIR,
)
from onibus import build as build_mod
from onibus import collisions as coll_mod
from onibus import git_ops, merge, reconcile, tick, tracey
from onibus.dag import Dag, gate_is_clear, set_priority, set_status
from onibus.jsonl import append_jsonl, read_header, read_jsonl, remove_jsonl, write_jsonl
from onibus.models import (
    FOLLOWUP_ORIGINS,
    AgentRow,
    DagValidation,
    Followup,
    KnownFlake,
    MergeQueueRow,
    PlanRow,
    canonical_plan_id,
)
from onibus.plan_doc import qa_mechanical_check, tracey_coverage

_P_NUM_RE = re.compile(r"^P(\d+)$")

_MODELS = {
    m.__name__: m for m in (
        AgentRow, MergeQueueRow, Followup, KnownFlake, PlanRow, DagValidation,
    )
} | {
    m: getattr(__import__("onibus.models", fromlist=[m]), m)
    for m in (
        "Gate", "MergerReport", "CoverageResult", "PlanFile", "CollisionRow",
        "BuildReport", "Preflight", "ConvcoResult", "RebaseResult", "FfResult",
        "BehindReport", "BehindCheck", "CadenceReport", "LockStatus",
        "ReconcileReport", "ExcusableVerdict", "AtomicityVerdict", "TraceyCoverage",
        "RenameReport", "CollisionReport", "TickReport", "StopSnapshot", "Worktree",
        "DagFlipResult", "FastPathVerdict",
    )
}


def _schema_exit(model_cls) -> None:
    print(json.dumps(model_cls.model_json_schema(), indent=2))
    sys.exit(0)


def _emit(obj) -> None:
    if hasattr(obj, "model_dump_json"):
        print(obj.model_dump_json(indent=2))
    else:
        print(json.dumps(obj, indent=2, default=str))


# ─── group handlers ──────────────────────────────────────────────────────────


def _cmd_dag(args: argparse.Namespace) -> int:
    if args.dag_cmd == "render":
        dag = Dag.load()
        print(dag.render())
        print(f"\n# {len(dag)} rows; frontier = {sorted(dag.frontier())}", file=sys.stderr)
        return 0
    if args.dag_cmd == "frontier":
        f = sorted(Dag.load().frontier())
        print(json.dumps(f) if args.json else "\n".join(f"P{n:04d}" for n in f))
        return 0
    if args.dag_cmd == "append":
        row = PlanRow.model_validate_json(args.json_row)
        dag = Dag.load()
        v = dag.try_append(row)
        if not v.ok:
            for e in v.errors:
                print(f"error: {e}", file=sys.stderr)
            return 2
        append_jsonl(DAG_JSONL, row)
        return 0
    if args.dag_cmd == "set-status":
        try:
            print(set_status(args.plan, args.status, args.note or "", force=args.force))
            return 0
        except KeyError as e:
            print(f"error: {e}", file=sys.stderr)
            return 1
        except ValidationError as e:
            print(f"error: {e.errors()[0]['msg']}", file=sys.stderr)
            return 2
    if args.dag_cmd == "deps":
        dag = Dag.load()
        if args.plan not in dag:
            print(f"no row for plan {args.plan}", file=sys.stderr)
            return 1
        r = dag[args.plan]
        _emit({
            "plan": r.plan, "status": r.status,
            "deps": [{"plan": d, "status": dag[d].status} for d in r.deps],
            "deps_raw": r.deps_raw,
            "all_deps_done": all(dag[d].status == "DONE" for d in r.deps),
        })
        return 0
    if args.dag_cmd == "blocks":
        _emit(Dag.load().blocks(args.plan, transitive=args.transitive))
        return 0
    if args.dag_cmd == "unblocked-by":
        _emit(Dag.load().unblocked_by(args.plan))
        return 0
    if args.dag_cmd == "impact":
        for n, count in Dag.load().impact():
            print(f"P{n:04d}  {count:3d}")
        return 0
    if args.dag_cmd == "hotpath":
        _emit(Dag.load().hotpath())
        return 0
    if args.dag_cmd == "launchable":
        dag = Dag.load()
        cx = coll_mod.CollisionIndex.load(only=dag.frontier())
        cands = dag.launchable(cx, args.parallel)
        if args.verbose:
            eff = dag.effective_priorities()
            for n in cands:
                own = dag[n].priority
                mark = f" (eff={eff[n]}←)" if eff[n] != own else ""
                print(f"P{n:04d}  prio={own}{mark}")
        else:
            _emit(cands)
        return 0
    if args.dag_cmd == "set-priority":
        try:
            old, bumped = set_priority(args.plan, args.priority)
        except KeyError as e:
            print(f"error: {e}", file=sys.stderr)
            return 1
        except ValidationError as e:
            print(f"error: {e.errors()[0]['msg']}", file=sys.stderr)
            return 2
        print(f"P{args.plan:04d} priority {old} → {args.priority}")
        if bumped:
            print(f"  propagates to frontier: {' '.join(f'P{n:04d}' for n in bumped)}")
        return 0
    if args.dag_cmd == "validate":
        v = Dag.load().validate()
        _emit(v)
        return 0 if v.ok else 1
    if args.dag_cmd == "markers":
        _dag_markers()
        return 0
    raise AssertionError(args.dag_cmd)


def _dag_markers() -> None:
    """Planning-gap detector: join UNIMPL plans' tracey refs with stdin uncovered."""
    from onibus.tracey import TRACEY_DOMAIN_ALT
    _DOC_RE = re.compile(rf"r\[((?:{TRACEY_DOMAIN_ALT})\.[a-z][a-z0-9-]*\.[a-z0-9.-]+)\]")
    _STDIN_RE = re.compile(rf"\b((?:{TRACEY_DOMAIN_ALT})\.[a-z][a-z0-9-]*\.[a-z0-9.-]+)\b")
    claims: defaultdict = defaultdict(list)
    dag = Dag.load()
    unimpl = {n for n in dag._by_num if dag[n].status in ("UNIMPL", "PARTIAL")}
    plan_re = re.compile(r"^plan-(\d{4})-")
    for doc in sorted(WORK_DIR.glob("plan-*.md")):
        m = plan_re.match(doc.name)
        if not m or int(m.group(1)) not in unimpl:
            continue
        n = int(m.group(1))
        for marker in set(_DOC_RE.findall(doc.read_text())):
            claims[marker].append(n)
    uncovered: set[str] = set()
    if not sys.stdin.isatty():
        uncovered = set(_STDIN_RE.findall(sys.stdin.read()))
    claimed = set(claims)
    _emit({
        "claimed_uncovered": {m: sorted(claims[m]) for m in sorted(claimed & uncovered)},
        "unclaimed_uncovered": sorted(uncovered - claimed),
        "claimed_covered": (
            {m: sorted(claims[m]) for m in sorted(claimed - uncovered)}
            if uncovered else "<no tracey input>"
        ),
        "_summary": {
            "unimpl_plans": len(unimpl), "markers_claimed": len(claimed),
            "tracey_uncovered": len(uncovered), "planning_gaps": len(uncovered - claimed),
        },
    })


def _cmd_state(args: argparse.Namespace) -> int:
    if args.state_cmd == "agent-row":
        append_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow.model_validate_json(args.json_row))
        return 0
    if args.state_cmd == "agent-lookup":
        plan_c = canonical_plan_id(args.plan)
        for a in read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow):
            if a.plan == plan_c and a.role == args.role:
                print(a.model_dump_json())
                break
        return 0
    if args.state_cmd == "followup":
        _followup_write(args.source, args.json_row)
        return 0
    if args.state_cmd == "followups-render":
        _followups_render(args.inline)
        return 0
    if args.state_cmd == "open-batches":
        for r in Dag.load().rows():
            if "batch" in r.title.lower() and r.status != "DONE":
                print(f"P{r.plan:04d}: {r.status} — {r.title}")
        return 0
    if args.state_cmd == "reconcile":
        if args.schema:
            _schema_exit(_MODELS["ReconcileReport"])
        _emit(reconcile.reconcile())
        return 0
    if args.state_cmd == "agent-start":
        _emit(merge.agent_start(args.role, args.plan, agent_id=args.id, note=args.note or ""))
        return 0
    if args.state_cmd == "agent-mark":
        n = merge.agent_mark(args.plan, args.role, args.status)
        print(f"updated {n} row(s)")
        return 0 if n > 0 else 1
    if args.state_cmd == "archive-agents":
        n = merge.archive_agents()
        print(f"dropped {n} dead row(s)")
        return 0
    raise AssertionError(args.state_cmd)


def _followup_write(source_arg: str, row_json: str) -> None:
    data = json.loads(row_json)
    data["source_plan"] = source_arg
    data["timestamp"] = datetime.now().isoformat()
    m = _P_NUM_RE.match(source_arg)
    if m:
        data.setdefault("discovered_from", int(m.group(1)))
        data.setdefault("origin", "reviewer")
    elif source_arg in FOLLOWUP_ORIGINS:
        data.setdefault("discovered_from", None)
        data.setdefault("origin", source_arg)
    else:
        print(
            f"followup positional must be P<N> or one of {sorted(FOLLOWUP_ORIGINS)}, got {source_arg!r}",
            file=sys.stderr,
        )
        sys.exit(2)
    append_jsonl(STATE_DIR / "followups-pending.jsonl", Followup.model_validate(data))


def _followups_render(inline: str | None) -> None:
    if inline:
        rows = []
        for obj in json.loads(inline):
            obj.setdefault("source_plan", "inline")
            obj.setdefault("origin", "inline")
            obj.setdefault("timestamp", "-")
            rows.append(Followup.model_validate(obj))
    else:
        rows = read_jsonl(STATE_DIR / "followups-pending.jsonl", Followup)
    if not rows:
        print("(empty)")
        return
    print("| Severity | Description | File:line | Proposed plan | Deps | Source |")
    print("|---|---|---|---|---|---|")
    for f in rows:
        print(f"| {f.severity} | {f.description} | {f.file_line or '-'} "
              f"| {f.proposed_plan} | {f.deps or '-'} | {f.source_plan} |")


def _cmd_merge(args: argparse.Namespace) -> int:
    c = args.merge_cmd
    if c == "lock":
        merge.lock(args.plan, args.agent_id)  # exits internally
    if c == "unlock":
        merge.unlock()
        return 0
    if c == "lock-status":
        if args.schema:
            _schema_exit(_MODELS["LockStatus"])
        _emit(merge.lock_status())
        return 0
    if c == "cadence":
        if args.schema:
            _schema_exit(_MODELS["CadenceReport"])
        _emit(merge.cadence())
        return 0
    if c == "queue-consume":
        n = merge.queue_consume(args.plan)
        print(f"removed {n} row(s)")
        return 0
    if c == "behind-check":
        if args.schema:
            _schema_exit(_MODELS["BehindCheck"])
        _emit(git_ops.behind_check(Path(args.worktree)))
        return 0
    if c == "count-bump":
        print(merge.count_bump(args.set_to))
        return 0
    if c == "dag-flip":
        if args.schema:
            _schema_exit(_MODELS["DagFlipResult"])
        _emit(merge.dag_flip(args.plan))
        return 0
    if c == "queue":
        append_jsonl(STATE_DIR / "merge-queue.jsonl", MergeQueueRow.model_validate_json(args.json_row))
        return 0
    if c == "queue-gates":
        dag = Dag.load()
        for r in read_jsonl(STATE_DIR / "merge-queue.jsonl", MergeQueueRow):
            print(json.dumps({
                "plan": r.plan,
                "gate": r.gate.model_dump() if r.gate else None,
                "clear": gate_is_clear(r.gate, dag),
            }))
        return 0
    if c == "queue-halted":
        reason = merge.queue_halted()
        if reason:
            print(reason, end="")
            return 1
        return 0
    if c == "clear-halt":
        print("cleared" if merge.clear_halt() else "not-halted")
        return 0
    if c == "clause4-check":
        if args.schema:
            _schema_exit(_MODELS["FastPathVerdict"])
        try:
            v = merge.clause4_check(args.base)
        except Exception as e:
            # Fail-safe: crash in the optimizer degrades to full CI, never
            # skips it. The merger sees a normal RUN_FULL and proceeds to
            # step 5. reason carries the exception for the merge report.
            v = _MODELS["FastPathVerdict"](
                decision="RUN_FULL",
                reason=f"clause4-check crashed: {type(e).__name__}: {e}",
            )
        _emit(v)
        # Nonzero on HALT so the merger's `|| exit` catches it without
        # jq-parsing. halt_queue() has already written the sentinel.
        return 1 if v.decision == "HALT" else 0
    if c == "record-green":
        h = merge.record_green_ci_hash()
        print(h or "eval-failed")
        return 0
    if c == "atomicity-check":
        if args.schema:
            _schema_exit(_MODELS["AtomicityVerdict"])
        _emit(merge.atomicity_check(args.branch))
        return 0
    if c == "rename-unassigned":
        if args.schema:
            _schema_exit(_MODELS["RenameReport"])
        _emit(merge.rename_unassigned(args.branch))
        return 0
    if c == "pre-ff-rename":
        if args.schema:
            _schema_exit(_MODELS["RenameReport"])
        _emit(merge.pre_ff_rename(args.branch))
        return 0
    if c == "preflight":
        if args.schema:
            _schema_exit(_MODELS["Preflight"])
        _emit(git_ops.preflight(args.branch))
        return 0
    if c == "convco-check":
        if args.schema:
            _schema_exit(_MODELS["ConvcoResult"])
        _emit(git_ops.convco_check(args.range))
        return 0
    if c == "rebase-anchored":
        if args.schema:
            _schema_exit(_MODELS["RebaseResult"])
        _emit(git_ops.rebase_anchored(args.branch))
        return 0
    if c == "ff-try":
        if args.schema:
            _schema_exit(_MODELS["FfResult"])
        _emit(git_ops.ff_try(args.branch))
        return 0
    if c == "behind-report":
        if args.schema:
            _schema_exit(_MODELS["BehindReport"])
        _emit(git_ops.behind_report())
        return 0
    raise AssertionError(c)


def _cmd_flake(args: argparse.Namespace) -> int:
    _warn_if_cwd_elsewhere()
    if args.flake_cmd == "add":
        new = KnownFlake.model_validate_json(args.json_row)
        existing = read_jsonl(KNOWN_FLAKES, KnownFlake)
        dups = [r for r in existing if r.test == new.test]
        if dups:
            print(
                f"known-flake with test={new.test!r} already exists. "
                f"Use `onibus flake remove {new.test}` first, "
                f"or pick a different test key (sentinel names like "
                f"`<tcg-builder-allocation>` are fine for infra-wide entries).",
                file=sys.stderr,
            )
            return 1
        append_jsonl(KNOWN_FLAKES, new)
        return 0
    if args.flake_cmd == "remove":
        n = remove_jsonl(KNOWN_FLAKES, KnownFlake, lambda f: f.test == args.test)
        print(f"removed {n} row(s) for test {args.test!r}")
        return 0
    if args.flake_cmd == "exists":
        rows = read_jsonl(KNOWN_FLAKES, KnownFlake)
        return 0 if any(f.test == args.test for f in rows) else 1
    if args.flake_cmd == "mitigation":
        from onibus.models import Mitigation
        rows = read_jsonl(KNOWN_FLAKES, KnownFlake)
        matches = [r for r in rows if r.test == args.test]
        if not matches:
            print(f"no known-flake with test={args.test!r}", file=sys.stderr)
            return 1
        if len(matches) > 1:
            # Defense in depth — T1's flake-add guard should prevent this,
            # but the file is hand-editable and pre-T1 history may have dups.
            # Silent first-match is worse than a loud error.
            print(
                f"AMBIGUOUS: {len(matches)} known-flakes with test={args.test!r}. "
                f"Cannot safely pick one. Fix known-flakes.jsonl by hand "
                f"(merge or rename duplicates).",
                file=sys.stderr,
            )
            return 2
        target = matches[0]
        target.mitigations.append(
            Mitigation(plan=args.plan, landed_sha=args.sha, note=args.note)
        )
        # Preserve header comments — atomic rewrite via write_jsonl.
        header = read_header(KNOWN_FLAKES)
        write_jsonl(KNOWN_FLAKES, rows, header=header)
        print(f"appended mitigation P{args.plan:04d} @ {args.sha} to {args.test!r}")
        return 0
    if args.flake_cmd == "excusable":
        if args.schema:
            _schema_exit(_MODELS["ExcusableVerdict"])
        v = build_mod.excusable(Path(args.log_path))
        _emit(v)
        return 0 if v.excusable else 1
    raise AssertionError(args.flake_cmd)


def _cmd_collisions(args: argparse.Namespace) -> int:
    if args.coll_cmd == "top":
        cx = coll_mod.CollisionIndex.load()
        for r in cx.hot(args.n):
            print(f"{r.count:3d} {r.path}")
        return 0
    if args.coll_cmd == "check":
        if args.schema:
            _schema_exit(_MODELS["CollisionReport"])
        _emit(coll_mod.check_vs_running(args.plan))
        return 0
    raise AssertionError(args.coll_cmd)


def _cmd_plan(args: argparse.Namespace) -> int:
    if args.plan_cmd == "qa-check":
        worktree = Path(args.worktree)
        dag_plans = set(Dag.load()._by_num)
        changed = subprocess.run(
            ["git", "diff", "--name-only", f"{INTEGRATION_BRANCH}..HEAD", "--",
             ".claude/work/plan-*.md"],
            cwd=worktree, capture_output=True, text=True, check=True,
        ).stdout.split()
        failed = False
        for doc in changed:
            issues = qa_mechanical_check(worktree / doc, dag_plans)
            for sev, msg in issues:
                if sev == "FAIL":
                    failed = True
                    print(f"FAIL: {doc}: {msg}")
                else:
                    print(f"{sev}: {doc}: {msg}", file=sys.stderr)
        return 1 if failed else 0
    if args.plan_cmd == "tracey-markers":
        for m in tracey.tracey_markers(Path(args.path)):
            print(m)
        return 0
    if args.plan_cmd == "tracey-coverage":
        if args.schema:
            _schema_exit(_MODELS["TraceyCoverage"])
        wt = Path(args.worktree) if args.worktree else None
        r = tracey_coverage(args.branch, Path(args.plan_doc), worktree=wt)
        _emit(r)
        return 0 if not r.unmatched else 1
    raise AssertionError(args.plan_cmd)


def _cmd_build(args: argparse.Namespace) -> int:
    if args.schema:
        _schema_exit(_MODELS["BuildReport"])
    if args.coverage:
        build_mod.coverage(args.coverage[0], args.coverage[1], loud=args.loud)
        return 0
    if args.link and not args.copy:
        # --link creates ./result → local store path; pointless without --copy
        # (output would be remote-only, symlink would dangle). Imply --copy.
        args.copy = True
    report = build_mod.run(
        args.target, role=args.role, copy=args.copy, link=args.link, loud=args.loud
    )
    print(report.model_dump_json(indent=2 if args.loud else None))
    return 0


def _warn_if_cwd_elsewhere() -> None:
    cwd = Path.cwd().resolve()
    if not cwd.is_relative_to(REPO_ROOT):
        sys.stderr.write(
            f"warning: editing {KNOWN_FLAKES} from cwd={cwd} — REPO_ROOT is {REPO_ROOT}. "
            f"If in a worktree, invoke via relative path so the edit lands there.\n"
        )


# ─── main (grouped) ──────────────────────────────────────────────────────────


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="onibus", description="rio-build DAG harness toolkit")
    sub = p.add_subparsers(dest="group", required=True)

    # top-level singletons
    sub.add_parser("integration-branch")
    sp = sub.add_parser("schema"); sp.add_argument("model", choices=sorted(_MODELS))
    sp = sub.add_parser("tick"); sp.add_argument("--schema", action="store_true")
    sp = sub.add_parser("stop"); sp.add_argument("--schema", action="store_true")

    # dag
    g = sub.add_parser("dag").add_subparsers(dest="dag_cmd", required=True)
    g.add_parser("render")
    sp = g.add_parser("frontier"); sp.add_argument("--json", action="store_true")
    sp = g.add_parser("append"); sp.add_argument("json_row")
    sp = g.add_parser("set-status"); sp.add_argument("plan", type=int); sp.add_argument("status"); sp.add_argument("--note"); sp.add_argument("--force", action="store_true")
    sp = g.add_parser("deps"); sp.add_argument("plan", type=int)
    sp = g.add_parser("blocks"); sp.add_argument("plan", type=int); sp.add_argument("--transitive", action="store_true")
    sp = g.add_parser("unblocked-by"); sp.add_argument("plan", type=int)
    g.add_parser("impact")
    g.add_parser("hotpath")
    sp = g.add_parser("launchable"); sp.add_argument("--parallel", type=int, default=10); sp.add_argument("--verbose", "-v", action="store_true")
    sp = g.add_parser("set-priority"); sp.add_argument("plan", type=int); sp.add_argument("priority", type=int)
    g.add_parser("validate")
    g.add_parser("markers")

    # state
    g = sub.add_parser("state").add_subparsers(dest="state_cmd", required=True)
    sp = g.add_parser("agent-row"); sp.add_argument("json_row")
    sp = g.add_parser("agent-lookup"); sp.add_argument("plan"); sp.add_argument("role")
    sp = g.add_parser("followup"); sp.add_argument("source"); sp.add_argument("json_row")
    sp = g.add_parser("followups-render"); sp.add_argument("--inline")
    g.add_parser("open-batches")
    sp = g.add_parser("reconcile"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("agent-start"); sp.add_argument("role"); sp.add_argument("plan"); sp.add_argument("--id"); sp.add_argument("--note")
    sp = g.add_parser("agent-mark"); sp.add_argument("plan"); sp.add_argument("role"); sp.add_argument("status", choices=["running", "done", "consumed"])
    g.add_parser("archive-agents")

    # merge
    g = sub.add_parser("merge").add_subparsers(dest="merge_cmd", required=True)
    sp = g.add_parser("lock"); sp.add_argument("plan"); sp.add_argument("agent_id")
    g.add_parser("unlock")
    sp = g.add_parser("lock-status"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("cadence"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("queue-consume"); sp.add_argument("plan")
    sp = g.add_parser("behind-check"); sp.add_argument("worktree"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("count-bump"); sp.add_argument("--set-to", dest="set_to", type=int, default=None)
    sp = g.add_parser("dag-flip"); sp.add_argument("plan", type=int); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("queue"); sp.add_argument("json_row")
    g.add_parser("queue-gates")
    g.add_parser("queue-halted")
    g.add_parser("clear-halt")
    sp = g.add_parser("clause4-check"); sp.add_argument("base"); sp.add_argument("--schema", action="store_true")
    g.add_parser("record-green")
    for name in ("atomicity-check", "rename-unassigned", "pre-ff-rename", "preflight", "rebase-anchored", "ff-try"):
        sp = g.add_parser(name); sp.add_argument("branch"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("convco-check"); sp.add_argument("range"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("behind-report"); sp.add_argument("--schema", action="store_true")

    # flake
    g = sub.add_parser("flake").add_subparsers(dest="flake_cmd", required=True)
    sp = g.add_parser("add"); sp.add_argument("json_row")
    sp = g.add_parser("remove"); sp.add_argument("test")
    sp = g.add_parser("exists"); sp.add_argument("test")
    sp = g.add_parser("excusable"); sp.add_argument("log_path"); sp.add_argument("--schema", action="store_true")
    sp = g.add_parser("mitigation"); sp.add_argument("test"); sp.add_argument("plan", type=int); sp.add_argument("sha"); sp.add_argument("note")

    # collisions
    g = sub.add_parser("collisions").add_subparsers(dest="coll_cmd", required=True)
    sp = g.add_parser("top"); sp.add_argument("n", type=int, nargs="?", default=20)
    sp = g.add_parser("check"); sp.add_argument("plan", type=int); sp.add_argument("--schema", action="store_true")

    # plan
    g = sub.add_parser("plan").add_subparsers(dest="plan_cmd", required=True)
    sp = g.add_parser("qa-check"); sp.add_argument("worktree")
    sp = g.add_parser("tracey-markers"); sp.add_argument("path")
    sp = g.add_parser("tracey-coverage"); sp.add_argument("branch"); sp.add_argument("plan_doc"); sp.add_argument("--worktree"); sp.add_argument("--schema", action="store_true")

    # build
    sp = sub.add_parser("build")
    sp.add_argument("target", nargs="?", default=".#ci")
    sp.add_argument("--role", choices=build_mod.ROLES, default="impl")
    sp.add_argument("--copy", action="store_true")
    sp.add_argument("--link", action="store_true", help="create ./result symlink (implies --copy)")
    sp.add_argument("--loud", action="store_true")
    sp.add_argument("--coverage", nargs=2, metavar=("BRANCH", "MERGED_AT"))
    sp.add_argument("--schema", action="store_true")

    args = p.parse_args(argv)

    match args.group:
        case "integration-branch": print(INTEGRATION_BRANCH); return 0
        case "schema": _schema_exit(_MODELS[args.model])
        case "tick":
            if args.schema: _schema_exit(_MODELS["TickReport"])
            _emit(tick.tick()); return 0
        case "stop":
            if args.schema: _schema_exit(_MODELS["StopSnapshot"])
            _emit(tick.stop()); return 0
        case "dag": return _cmd_dag(args)
        case "state": return _cmd_state(args)
        case "merge": return _cmd_merge(args)
        case "flake": return _cmd_flake(args)
        case "collisions": return _cmd_collisions(args)
        case "plan": return _cmd_plan(args)
        case "build": return _cmd_build(args)
    return 0
