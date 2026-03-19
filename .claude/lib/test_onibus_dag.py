"""Dag class + CollisionIndex + merger-bash-ops + excusable. Net-new behavior,
no I/O in algorithm tests (synthetic PlanRow lists)."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from onibus.collisions import CollisionIndex
from onibus.dag import CycleError, Dag
from onibus.models import KnownFlake, PlanRow


def row(n: int, deps: list[int] | None = None, status: str = "UNIMPL", **kw) -> PlanRow:
    return PlanRow(plan=n, title=f"p{n}", deps=deps or [], status=status, **kw)  # type: ignore[arg-type]


# ─── Dag constructor invariants ──────────────────────────────────────────────


def test_dag_rejects_duplicate_plan():
    with pytest.raises(ValueError, match="duplicate plan 1"):
        Dag([row(1), row(1)])


def test_dag_rejects_unknown_dep():
    with pytest.raises(ValueError, match="P2 dep P99 not in DAG"):
        Dag([row(2, deps=[99])])


def test_dag_constructor_allows_cycle():
    # Cycle is a workflow bug, not corruption — constructor lets it through
    # so `onibus dag validate` can report it.
    dag = Dag([row(1, deps=[2]), row(2, deps=[1])])
    assert len(dag) == 2


def test_dag_empty():
    dag = Dag([])
    assert len(dag) == 0
    assert dag.frontier() == set()
    assert dag.topo == []
    assert dag.hotpath() == []


# ─── frontier ────────────────────────────────────────────────────────────────


def test_frontier_simple():
    dag = Dag([row(1, status="DONE"), row(2, deps=[1])])
    assert dag.frontier() == {2}


def test_frontier_excludes_done():
    dag = Dag([row(1, status="DONE"), row(2, status="DONE", deps=[1])])
    assert dag.frontier() == set()


def test_frontier_excludes_blocked():
    dag = Dag([row(1), row(2, deps=[1])])
    assert dag.frontier() == {1}  # 2 blocked by 1 (UNIMPL)


def test_frontier_excludes_deps_raw():
    # deps_raw escape-hatch → unknown dep state → conservatively excluded.
    dag = Dag([row(1, status="DONE"), row(2, deps_raw="P20b(deferred)")])
    assert dag.frontier() == set()


def test_frontier_includes_partial():
    dag = Dag([row(1, status="DONE"), row(2, deps=[1], status="PARTIAL")])
    assert dag.frontier() == {2}


# ─── blocks / unblocked_by / impact ──────────────────────────────────────────


def test_blocks_direct():
    dag = Dag([row(1), row(2, deps=[1]), row(3, deps=[1])])
    assert dag.blocks(1) == [2, 3]
    assert dag.blocks(2) == []


def test_blocks_transitive_diamond():
    #   1
    #  / \
    # 2   3
    #  \ /
    #   4
    dag = Dag([row(1), row(2, deps=[1]), row(3, deps=[1]), row(4, deps=[2, 3])])
    assert dag.blocks(1, transitive=True) == [2, 3, 4]  # 4 counted once
    assert dag.blocks(2, transitive=True) == [4]


def test_unblocked_by_chain():
    # 1(DONE) → 2 → 3. Finishing 2 unblocks 3.
    dag = Dag([row(1, status="DONE"), row(2, deps=[1]), row(3, deps=[2])])
    assert dag.frontier() == {2}
    assert dag.unblocked_by(2) == [3]
    assert dag.unblocked_by(1) == []  # 1 already DONE, no change


def test_unblocked_by_multi_dep():
    # 3 needs BOTH 1 and 2. Finishing only 1 doesn't unblock it.
    dag = Dag([row(1), row(2), row(3, deps=[1, 2])])
    assert dag.unblocked_by(1) == []
    assert dag.unblocked_by(2) == []


def test_impact_sorts_by_transitive_count():
    # 1 blocks {2,3,4}; 5 blocks {6}. Both on frontier → impact sorts 1 first.
    dag = Dag([
        row(1), row(2, deps=[1]), row(3, deps=[1]), row(4, deps=[2]),
        row(5), row(6, deps=[5]),
    ])
    imp = dag.impact()
    assert imp[0] == (1, 3)
    assert imp[1] == (5, 1)


# ─── topo + hotpath + cycle ──────────────────────────────────────────────────


def test_topo_linear():
    dag = Dag([row(3, deps=[2]), row(1), row(2, deps=[1])])
    assert dag.topo == [1, 2, 3]


def test_topo_raises_cycle():
    dag = Dag([row(1, deps=[2]), row(2, deps=[1])])
    with pytest.raises(CycleError) as exc:
        _ = dag.topo
    assert set(exc.value.cycle[:2]) == {1, 2}


def test_topo_raises_3cycle():
    dag = Dag([row(1, deps=[3]), row(2, deps=[1]), row(3, deps=[2])])
    with pytest.raises(CycleError) as exc:
        _ = dag.topo
    assert len(exc.value.cycle) == 4  # closes the loop


def test_hotpath_picks_longest_chain():
    # Chain A: 1→2→3 (length 3). Chain B: 4→5 (length 2). All UNIMPL.
    dag = Dag([row(1), row(2, deps=[1]), row(3, deps=[2]), row(4), row(5, deps=[4])])
    assert dag.hotpath() == [1, 2, 3]


def test_hotpath_skips_done():
    # 1(DONE)→2→3. Hotpath is [2,3] — the DONE prefix doesn't count.
    dag = Dag([row(1, status="DONE"), row(2, deps=[1]), row(3, deps=[2])])
    assert dag.hotpath() == [2, 3]


def test_hotpath_empty_when_all_done():
    dag = Dag([row(1, status="DONE"), row(2, deps=[1], status="DONE")])
    assert dag.hotpath() == []


# ─── try_append / try_transition / validate ──────────────────────────────────


def test_try_append_rejects_dup():
    dag = Dag([row(1)])
    v = dag.try_append(row(1))
    assert not v.ok and "P1 already exists" in v.errors[0]


def test_try_append_rejects_unknown_dep():
    dag = Dag([row(1)])
    v = dag.try_append(row(2, deps=[99]))
    assert not v.ok and "P99" in v.errors[0]


def test_try_append_ok():
    dag = Dag([row(1)])
    v = dag.try_append(row(2, deps=[1]))
    assert v.ok and not v.errors


def test_try_transition_warns_done_to_unimpl():
    dag = Dag([row(1, status="DONE")])
    warn = dag.try_transition(1, "UNIMPL")
    assert warn is not None and "revert" in warn


def test_try_transition_silent_on_forward():
    dag = Dag([row(1)])
    assert dag.try_transition(1, "DONE") is None
    assert dag.try_transition(1, "PARTIAL") is None


def test_validate_catches_cycle():
    dag = Dag([row(1, deps=[2]), row(2, deps=[1])])
    v = dag.validate()
    assert not v.ok
    assert any("cycle" in e for e in v.errors)


def test_validate_warns_deps_raw():
    dag = Dag([row(1), row(2, deps_raw="P20b")])
    v = dag.validate()
    assert v.ok  # not an error
    assert any("deps_raw" in w for w in v.warnings)


def test_validate_warns_done_with_unimpl_dep():
    # P2 DONE but its dep P1 is still UNIMPL — out-of-order merge.
    dag = Dag([row(1), row(2, deps=[1], status="DONE")])
    v = dag.validate()
    assert any("out-of-order" in w for w in v.warnings)


# ─── CollisionIndex ──────────────────────────────────────────────────────────


def test_collision_index_synthetic():
    cx = CollisionIndex({1: ["a.rs", "b.rs"], 2: ["b.rs", "c.rs"], 3: ["d.rs"]})
    assert cx.files_of(1) == frozenset({"a.rs", "b.rs"})
    assert cx.files_of(99) == frozenset()
    assert cx.check(1, {2}) == ["b.rs"]
    assert cx.check(1, {3}) == []
    assert cx.check(1, {2, 3}) == ["b.rs"]
    hot = cx.hot()
    assert len(hot) == 1 and hot[0].path == "b.rs" and hot[0].count == 2


def test_launchable_greedy_skips_collision():
    # 1 and 2 collide; 3 is independent. impact-sorted: 1 (blocks 4), 2 (blocks 5), 3.
    dag = Dag([row(1), row(2), row(3), row(4, deps=[1]), row(5, deps=[2])])
    cx = CollisionIndex({1: ["shared.rs"], 2: ["shared.rs"], 3: ["other.rs"]})
    picks = dag.launchable(cx, k=3)
    # 1 picked (impact=1), 2 skipped (collides with 1), 3 picked (no collision).
    assert picks == [1, 3]


def test_launchable_respects_k():
    dag = Dag([row(n) for n in range(1, 6)])
    cx = CollisionIndex({n: [f"{n}.rs"] for n in range(1, 6)})
    assert len(dag.launchable(cx, k=2)) == 2


# ─── priority + effective-priority propagation ───────────────────────────────


def test_priority_defaults_to_50():
    assert row(1).priority == 50


def test_priority_range_enforced():
    with pytest.raises(ValueError):
        row(1, priority=0)
    with pytest.raises(ValueError):
        row(1, priority=101)
    # Assignment also checked (validate_assignment=True on PlanRow)
    r = row(1)
    with pytest.raises(ValueError):
        r.priority = 200


def test_effective_priority_propagates_backward():
    """Chain 1→2→3, set 3=90. Both 1 and 2 inherit 90 because launching
    them is on the critical path to 3."""
    dag = Dag([row(1), row(2, deps=[1]), row(3, deps=[2], priority=90)])
    eff = dag.effective_priorities()
    assert eff[3] == 90
    assert eff[2] == 90  # blocks 3 → inherits
    assert eff[1] == 90  # blocks 2→3 transitively → inherits


def test_effective_priority_takes_max():
    """1 blocks both 2 (prio=70) and 3 (prio=90). eff(1) = max = 90."""
    dag = Dag([row(1), row(2, deps=[1], priority=70), row(3, deps=[1], priority=90)])
    eff = dag.effective_priorities()
    assert eff[1] == 90


def test_effective_priority_isolated_stays_own():
    """Plan 4 with no downstream keeps its own priority."""
    dag = Dag([row(1), row(2, deps=[1], priority=90), row(4)])
    eff = dag.effective_priorities()
    assert eff[4] == 50  # not on any high-prio path
    assert eff[1] == 90  # on 2's path


def test_effective_priority_short_circuit_all_default():
    """Nothing elevated → returns base dict, no closure walk."""
    dag = Dag([row(n) for n in range(1, 20)])
    eff = dag.effective_priorities()
    assert all(v == 50 for v in eff.values())


def test_effective_priority_demotion_does_not_propagate():
    """Priority<50 stays local — only elevation propagates (max semantics)."""
    dag = Dag([row(1), row(2, deps=[1], priority=20)])
    eff = dag.effective_priorities()
    assert eff[2] == 20
    assert eff[1] == 50  # NOT demoted — 1 might block other things too


def test_launchable_strict_priority_beats_impact():
    """Plan 1 blocks 3 (impact=1). Plan 2 blocks nothing (impact=0) but
    priority=90. Strict sort → 2 first despite lower impact."""
    dag = Dag([row(1), row(2, priority=90), row(3, deps=[1])])
    cx = CollisionIndex({1: ["a.rs"], 2: ["b.rs"]})
    picks = dag.launchable(cx, k=2)
    assert picks[0] == 2  # priority=90 beats impact=1
    assert picks[1] == 1


def test_launchable_propagated_priority_lifts_blocker():
    """Plan 5 is leaf priority=90, blocked on 3. Plan 3 is on frontier,
    inherits eff=90. Plan 1 has impact=2 but default prio. 3 wins."""
    dag = Dag([
        row(1), row(2, deps=[1]), row(4, deps=[1]),  # 1 blocks 2,4 → impact=2
        row(3), row(5, deps=[3], priority=90),        # 3 blocks only 5 → impact=1
    ])
    cx = CollisionIndex({n: [f"{n}.rs"] for n in [1, 3]})
    picks = dag.launchable(cx, k=2)
    assert picks[0] == 3  # eff_prio(3)=90 via propagation from 5
    assert picks[1] == 1  # eff_prio(1)=50, impact=2


def test_launchable_equal_priority_falls_to_impact():
    """Both at default 50 → impact decides."""
    dag = Dag([row(1), row(2, deps=[1]), row(3)])  # 1 impact=1, 3 impact=0
    cx = CollisionIndex({1: ["a.rs"], 3: ["c.rs"]})
    picks = dag.launchable(cx, k=2)
    assert picks == [1, 3]  # impact tiebreak


def test_set_priority_raises_keyerror_not_sysexit(tmp_path: Path, monkeypatch):
    """Library function raises — cli.py converts to exit code. Testable."""
    import onibus.dag
    dag_jsonl = tmp_path / "dag.jsonl"
    dag_jsonl.write_text(row(1).model_dump_json() + "\n")
    monkeypatch.setattr(onibus.dag, "DAG_JSONL", dag_jsonl)
    with pytest.raises(KeyError, match="plan 999"):
        onibus.dag.set_priority(999, 50)


def test_set_priority_raises_validation_on_range(tmp_path: Path, monkeypatch):
    """ge=1,le=100 enforced via validate_assignment — raises, not silently clamps."""
    import onibus.dag
    from pydantic import ValidationError
    dag_jsonl = tmp_path / "dag.jsonl"
    dag_jsonl.write_text(row(1).model_dump_json() + "\n")
    monkeypatch.setattr(onibus.dag, "DAG_JSONL", dag_jsonl)
    with pytest.raises(ValidationError):
        onibus.dag.set_priority(1, 150)


def test_set_priority_returns_old_and_propagation(tmp_path: Path, monkeypatch):
    """Returns (old, bumped_frontier) — cli.py formats, tests assert."""
    import onibus.dag
    dag_jsonl = tmp_path / "dag.jsonl"
    # 1 on frontier, blocks 2. Bump 2 → 1 should appear in propagation list.
    dag_jsonl.write_text(
        row(1).model_dump_json() + "\n" +
        row(2, deps=[1]).model_dump_json() + "\n"
    )
    monkeypatch.setattr(onibus.dag, "DAG_JSONL", dag_jsonl)
    old, bumped = onibus.dag.set_priority(2, 90)
    assert old == 50
    assert bumped == [1]  # plan 1 is on frontier and now has eff_prio=90 via propagation


# ─── atomic write_jsonl ──────────────────────────────────────────────────────


def test_write_jsonl_atomic_crash_leaves_original(tmp_path: Path, monkeypatch):
    """If os.replace crashes (power loss, kill -9), original file is intact."""
    import onibus.jsonl
    path = tmp_path / "dag.jsonl"
    path.write_text(row(1).model_dump_json() + "\n")  # original: 1 row

    def boom(*a, **kw):
        raise OSError("simulated crash")
    monkeypatch.setattr(onibus.jsonl.os, "replace", boom)

    with pytest.raises(OSError):
        onibus.jsonl.write_jsonl(path, [row(1), row(2), row(3)])

    # Original survives unchanged — no torn write.
    from onibus.jsonl import read_jsonl
    from onibus.models import PlanRow
    assert len(read_jsonl(path, PlanRow)) == 1
    # .tmp is the crash artifact (safe to clean).
    assert path.with_suffix(".jsonl.tmp").exists()


def test_write_jsonl_no_tmp_left_on_success(tmp_path: Path):
    import onibus.jsonl
    path = tmp_path / "x.jsonl"
    onibus.jsonl.write_jsonl(path, [row(1), row(2)])
    assert not path.with_suffix(".jsonl.tmp").exists()
    assert path.read_text().count("\n") == 2


def test_write_jsonl_preserves_header(tmp_path: Path):
    """remove_jsonl passes header through — known-flakes.jsonl has a # comment."""
    import onibus.jsonl
    path = tmp_path / "flakes.jsonl"
    onibus.jsonl.write_jsonl(path, [row(1)], header=["# bridge table"])
    lines = path.read_text().splitlines()
    assert lines[0] == "# bridge table"
    assert '"plan":1' in lines[1]


def test_set_priority_uses_atomic_write(tmp_path: Path, monkeypatch):
    """No torn state: set_priority crash leaves old dag.jsonl intact."""
    import onibus.dag
    import onibus.jsonl
    dag_jsonl = tmp_path / "dag.jsonl"
    dag_jsonl.write_text(row(1, priority=50).model_dump_json() + "\n")
    monkeypatch.setattr(onibus.dag, "DAG_JSONL", dag_jsonl)
    monkeypatch.setattr(onibus.jsonl.os, "replace", lambda *a: (_ for _ in ()).throw(OSError("crash")))

    with pytest.raises(OSError):
        onibus.dag.set_priority(1, 90)

    # Reload — priority still 50, not partially-written garbage.
    fresh = onibus.dag.Dag.load()
    assert fresh[1].priority == 50


# ─── merger-bash ops (against tmp_repo fixture) ──────────────────────────────


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=True
    ).stdout.strip()


def test_convco_check(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "a").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): good", "--no-verify")
    (tmp_repo / "b").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "bad commit msg", "--no-verify")
    from onibus import INTEGRATION_BRANCH
    r = onibus.git_ops.convco_check(f"{INTEGRATION_BRANCH}..pX", cwd=tmp_repo)
    assert not r.clean
    assert r.violations == ["bad commit msg"]


def test_preflight_clean(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    # tmp_repo is its own "worktree parent" sibling — mock the worktree check.
    wt = tmp_repo.parent / "pX"
    wt.mkdir()
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "x").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): a", "--no-verify")
    r = onibus.git_ops.preflight("pX", repo=tmp_repo)
    assert r.clean
    assert r.commits_ahead == 1


def test_preflight_rejects_missing_branch(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    r = onibus.git_ops.preflight("does-not-exist", repo=tmp_repo)
    assert not r.clean
    assert "branch" in r.reason or "worktree" in r.reason


def test_ff_try_advances(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    from onibus import INTEGRATION_BRANCH
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "x").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): a", "--no-verify")
    _git(tmp_repo, "checkout", INTEGRATION_BRANCH)
    r = onibus.git_ops.ff_try("pX", repo=tmp_repo)
    assert r.status == "ok"
    assert r.pre_merge != r.post_merge


def test_behind_check_trivial_when_no_overlap(tmp_repo: Path, monkeypatch):
    """Validator's compound query: behind but no file-collision → trivial_rebase."""
    import onibus.git_ops
    from onibus import INTEGRATION_BRANCH
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    # Branch pX touches a.rs; $TGT advances with b.rs. No overlap.
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "a.rs").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): a", "--no-verify")
    _git(tmp_repo, "checkout", INTEGRATION_BRANCH)
    (tmp_repo / "b.rs").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(y): b", "--no-verify")
    _git(tmp_repo, "checkout", "pX")
    r = onibus.git_ops.behind_check(tmp_repo)
    assert r.behind == 1
    assert r.file_collision == []
    assert r.trivial_rebase is True


def test_behind_check_collision_when_same_file(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    from onibus import INTEGRATION_BRANCH
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "shared.rs").write_text("mine")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): a", "--no-verify")
    _git(tmp_repo, "checkout", INTEGRATION_BRANCH)
    (tmp_repo / "shared.rs").write_text("theirs")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(y): b", "--no-verify")
    _git(tmp_repo, "checkout", "pX")
    r = onibus.git_ops.behind_check(tmp_repo)
    assert r.behind == 1
    assert r.file_collision == ["shared.rs"]
    assert r.trivial_rebase is False


def test_behind_check_zero_when_current(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    r = onibus.git_ops.behind_check(tmp_repo)
    assert r.behind == 0
    assert r.trivial_rebase is False  # not behind → not a trivial REBASE, just current


def test_behind_check_no_phantom_self_collision(tmp_repo: Path, monkeypatch):
    """P0306 T1 regression: 2-dot `diff HEAD..TGT` is tree-vs-tree — includes
    OUR changes as 'undo', so `mine & theirs` always contains `mine`. 3-dot
    `diff HEAD...TGT` (merge-base→TGT) is what TGT actually changed.

    Distinct from test_behind_check_trivial_when_no_overlap: that test's
    identical file contents trigger git rename detection (a.rs→b.rs rename,
    --name-only shows only dest), which masks the 2-dot bug. Here file_a and
    file_b have DIFFERENT content — no rename heuristic — proves the 3-dot fix."""
    import onibus.git_ops
    from onibus import INTEGRATION_BRANCH
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    # pX touches file_a (unique content); TGT advances with file_b (different unique
    # content). No genuine overlap; no rename possibility.
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "file_a").write_text("content unique to branch pX\n" * 3)
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): a", "--no-verify")
    _git(tmp_repo, "checkout", INTEGRATION_BRANCH)
    (tmp_repo / "file_b").write_text("entirely different TGT content\n" * 5)
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(y): b", "--no-verify")
    _git(tmp_repo, "checkout", "pX")
    r = onibus.git_ops.behind_check(tmp_repo)
    assert r.behind == 1
    # Before fix: file_collision == ["file_a"] (2-dot tree diff shows file_a
    # as "deleted" going HEAD→TGT; intersected with mine={file_a} → phantom).
    assert r.file_collision == [], (
        f"phantom self-collision: {r.file_collision!r} — 2-dot theirs-side "
        "includes our own changes as undo direction"
    )
    assert r.trivial_rebase is True


# ─── cadence / lock-status / agent-start ─────────────────────────────────────


def test_cadence_due_at_multiples(tmp_path: Path, monkeypatch):
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "STATE_DIR", tmp_path)
    monkeypatch.setattr(onibus.merge, "_cadence_range", lambda w: f"fake..range{w}")
    (tmp_path / "merge-count.txt").write_text("35\n")  # 35 = 5×7 — both fire
    r = onibus.merge.cadence()
    assert r.count == 35
    assert r.consolidator.due and r.consolidator.range == "fake..range5"
    assert r.bughunter.due and r.bughunter.range == "fake..range7"


def test_cadence_not_due_off_multiple(tmp_path: Path, monkeypatch):
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "STATE_DIR", tmp_path)
    (tmp_path / "merge-count.txt").write_text("3\n")
    r = onibus.merge.cadence()
    assert not r.consolidator.due and r.consolidator.range is None
    assert not r.bughunter.due


def test_cadence_zero_count_not_due(tmp_path: Path, monkeypatch):
    """count=0 → 0%5==0 BUT 'due' should be False (nothing merged yet)."""
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "STATE_DIR", tmp_path)
    r = onibus.merge.cadence()  # no merge-count.txt → count=0
    assert r.count == 0
    assert not r.consolidator.due  # 0 is not a cadence trigger


def test_lock_status_ff_landed_when_tgt_moved(tmp_repo: Path, monkeypatch):
    """stale lock + $TGT moved since acquire → ff_landed=True."""
    import json
    from datetime import datetime, timedelta, timezone
    import onibus.merge
    state = tmp_repo / ".claude" / "state"
    state.mkdir(parents=True)
    monkeypatch.setattr(onibus.merge, "_LOCK_FILE", state / "merger.lock")
    monkeypatch.setattr(onibus.merge, "INTEGRATION_BRANCH", "HEAD")
    # Aged past _LEASE_SECS + stale main_at_acquire
    old = (datetime.now(timezone.utc) - timedelta(minutes=31)).isoformat()
    (state / "merger.lock").write_text(json.dumps({
        "agent_id": "x", "plan": "P1",
        "main_at_acquire": "0000000", "acquired_at": old,
    }))
    r = onibus.merge.lock_status()
    assert r.held and r.stale
    assert r.ff_landed is True  # current HEAD != "0000000"


def test_lock_stale_after_lease(tmp_path: Path, monkeypatch):
    """P0306 T2: stale is time-lease (age > _LEASE_SECS), not PID-liveness.
    PID of the fire-and-forget `onibus merge lock` subprocess is always dead
    by the time anyone checks — false-positive stale on every merge."""
    import json
    from datetime import datetime, timedelta, timezone
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "_LOCK_FILE", tmp_path / "merger.lock")
    old = (datetime.now(timezone.utc) - timedelta(minutes=31)).isoformat()
    (tmp_path / "merger.lock").write_text(json.dumps({
        "agent_id": "x", "plan": "P1",
        "acquired_at": old, "main_at_acquire": "deadbee",
    }))
    r = onibus.merge.lock_status()
    assert r.held is True
    assert r.stale is True  # 31min > 30min lease


def test_lock_fresh(tmp_path: Path, monkeypatch):
    """P0306 T2: lock acquired 1min ago → NOT stale. Under the old PID check
    this would have been stale=True (subprocess already exited)."""
    import json
    from datetime import datetime, timedelta, timezone
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "_LOCK_FILE", tmp_path / "merger.lock")
    recent = (datetime.now(timezone.utc) - timedelta(minutes=1)).isoformat()
    (tmp_path / "merger.lock").write_text(json.dumps({
        "agent_id": "x", "plan": "P1",
        "acquired_at": recent, "main_at_acquire": "deadbee",
    }))
    r = onibus.merge.lock_status()
    assert r.held is True
    assert r.stale is False  # 1min << 30min lease
    assert r.ff_landed is None  # only computed when stale


def test_lock_status_tolerates_naive_timestamp(tmp_path: Path, monkeypatch):
    """Pre-T2 lock files stored naive timestamps. _lock_age_secs assumes UTC
    rather than crashing on offset-naive/offset-aware subtraction."""
    import json
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "_LOCK_FILE", tmp_path / "merger.lock")
    (tmp_path / "merger.lock").write_text(json.dumps({
        "agent_id": "x", "plan": "P1",
        "acquired_at": "2026-01-01T00:00:00",  # no +00:00 suffix
        "main_at_acquire": "deadbee",
    }))
    r = onibus.merge.lock_status()  # must not raise TypeError
    assert r.held is True
    assert r.stale is True  # months old


def test_agent_start_derives_worktree(tmp_path: Path, monkeypatch):
    import onibus.merge
    monkeypatch.setattr(onibus.merge, "STATE_DIR", tmp_path)
    row = onibus.merge.agent_start("impl", "P0134", note="x")
    assert row.plan == "P0134"
    assert row.worktree == "/root/src/rio-build/p134"
    assert row.status == "running"
    # Also accepts bare number.
    row2 = onibus.merge.agent_start("verify", "245")
    assert row2.plan == "P245"
    assert row2.worktree == "/root/src/rio-build/p245"


def test_agent_mark_updates_matching(tmp_path: Path, monkeypatch):
    import onibus.merge
    from onibus.jsonl import append_jsonl, read_jsonl
    from onibus.models import AgentRow
    monkeypatch.setattr(onibus.merge, "STATE_DIR", tmp_path)
    f = tmp_path / "agents-running.jsonl"
    append_jsonl(f, AgentRow(plan="P1", role="impl", status="running"))
    append_jsonl(f, AgentRow(plan="P2", role="impl", status="running"))
    n = onibus.merge.agent_mark("P1", "impl", "consumed")
    assert n == 1
    rows = read_jsonl(f, AgentRow)
    assert rows[0].status == "consumed" and rows[1].status == "running"


def test_queue_consume_removes(tmp_path: Path, monkeypatch):
    import onibus.merge
    from onibus.jsonl import append_jsonl, read_jsonl
    from onibus.models import MergeQueueRow
    monkeypatch.setattr(onibus.merge, "STATE_DIR", tmp_path)
    f = tmp_path / "merge-queue.jsonl"
    append_jsonl(f, MergeQueueRow(plan="P1", worktree="/x", verdict="PASS", commit="a"))
    append_jsonl(f, MergeQueueRow(plan="P2", worktree="/y", verdict="PASS", commit="b"))
    n = onibus.merge.queue_consume("P1")
    assert n == 1
    assert [r.plan for r in read_jsonl(f, MergeQueueRow)] == ["P2"]


def test_merger_report_unblocked_field():
    """MergerReport.unblocked defaults empty, accepts int list."""
    from onibus.models import MergerReport
    r = MergerReport(status="merged", unblocked=[134, 135])
    assert r.unblocked == [134, 135]
    # Default still works (old agents not setting it).
    r2 = MergerReport(status="aborted")
    assert r2.unblocked == []


def test_ff_try_rejects_diverged(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    from onibus import INTEGRATION_BRANCH
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    _git(tmp_repo, "checkout", "-b", "pX")
    (tmp_repo / "x").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(x): a", "--no-verify")
    _git(tmp_repo, "checkout", INTEGRATION_BRANCH)
    (tmp_repo / "y").write_text("1")
    _git(tmp_repo, "add", "-A"); _git(tmp_repo, "commit", "-m", "feat(y): b", "--no-verify")
    r = onibus.git_ops.ff_try("pX", repo=tmp_repo)
    assert r.status == "not-ff"
    assert r.pre_merge == r.post_merge


# ─── excusable ───────────────────────────────────────────────────────────────


def test_excusable_single_known_flake(tmp_path: Path, monkeypatch):
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text("stuff\n        FAIL [   1.234s] crate::mod::flaky_test\nmore\n")
    flakes = tmp_path / "known-flakes.jsonl"
    flakes.write_text(KnownFlake(
        test="crate::mod::flaky_test", symptom="s", root_cause="r",
        fix_owner="P0999", fix_description="d", retry="Once",
    ).model_dump_json() + "\n")
    monkeypatch.setattr(build, "KNOWN_FLAKES", flakes)
    v = build.excusable(log)
    assert v.excusable
    assert v.failing_tests == ["crate::mod::flaky_test"]
    assert v.matched_flakes == ["crate::mod::flaky_test"]


def test_excusable_rejects_multiple_fails(tmp_path: Path, monkeypatch):
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text(
        "        FAIL [  1.0s] crate::a::x\n"
        "        FAIL [  2.0s] crate::b::y\n"
    )
    monkeypatch.setattr(build, "KNOWN_FLAKES", tmp_path / "empty.jsonl")
    v = build.excusable(log)
    assert not v.excusable
    assert "2 failures" in v.reason


def test_excusable_rejects_unknown_test(tmp_path: Path, monkeypatch):
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text("        FAIL [  1.0s] crate::not_a_flake\n")
    monkeypatch.setattr(build, "KNOWN_FLAKES", tmp_path / "empty.jsonl")
    v = build.excusable(log)
    assert not v.excusable
    assert "not in known-flakes" in v.reason


def test_excusable_rejects_retry_never(tmp_path: Path, monkeypatch):
    """Panel #5: known-flake with retry=Never should NOT be excusable."""
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text("        FAIL [  1.0s] crate::mod::never_retry\n")
    flakes = tmp_path / "known-flakes.jsonl"
    flakes.write_text(KnownFlake(
        test="crate::mod::never_retry", symptom="s", root_cause="r",
        fix_owner="P0999", fix_description="d", retry="Never",
    ).model_dump_json() + "\n")
    monkeypatch.setattr(build, "KNOWN_FLAKES", flakes)
    v = build.excusable(log)
    assert not v.excusable
    assert "retry=Never" in v.reason
    # Still reports it matched (so coordinator knows WHICH flake), just doesn't excuse
    assert v.matched_flakes == ["crate::mod::never_retry"]


# ─── MergerReport cadence typing (panel #7) ──────────────────────────────────


def test_merger_report_cadence_typed():
    """Panel #7: cadence field accepts CadenceReport, not bare dict."""
    from onibus.models import CadenceReport, CadenceWindow, MergerReport
    cr = CadenceReport(
        count=5,
        consolidator=CadenceWindow(due=True, range="abc..def"),
        bughunter=CadenceWindow(due=False, range=None),
    )
    r = MergerReport(status="merged", cadence=cr)
    assert r.cadence.consolidator.due is True  # dot-access works
    assert r.cadence.bughunter.range is None


def test_merger_report_schema_has_cadence_ref():
    """Panel #7: schema emits $ref:CadenceReport not additionalProperties:true."""
    from onibus.models import MergerReport
    schema = MergerReport.model_json_schema()
    cad = schema["properties"]["cadence"]
    # anyOf: [CadenceReport ref, null] — NOT generic object
    assert "CadenceReport" in str(cad)
    assert "CadenceReport" in schema.get("$defs", {})


# ─── tracey-coverage (panel #6) ──────────────────────────────────────────────


def test_tracey_coverage_all_matched(tmp_path: Path, monkeypatch):
    """Plan claims 2 markers; diff has r[impl] for both → covered=2, unmatched=[]"""
    import onibus.plan_doc
    plan = tmp_path / "plan.md"
    plan.write_text("## Tracey\n\nr[gw.foo.bar] r[store.baz.qux]\n")
    diff = (
        "+++ b/rio-gateway/src/foo.rs\n"
        "@@ -0,0 +1,2 @@\n"
        "+// r[impl gw.foo.bar]\n"
        "+fn x() {}\n"
        "+++ b/rio-store/src/baz.rs\n"
        "@@ -0,0 +5,1 @@\n"
        "+// r[impl store.baz.qux]\n"
    )
    monkeypatch.setattr(onibus.plan_doc, "git_try", lambda *a, **kw: diff)
    r = onibus.plan_doc.tracey_coverage("branch", plan)
    assert r.total == 2
    assert r.covered == 2
    assert r.unmatched == []
    assert r.markers[0].id == "gw.foo.bar"
    assert r.markers[0].impl_loc == "rio-gateway/src/foo.rs:1"


def test_tracey_coverage_unmatched(tmp_path: Path, monkeypatch):
    """Plan claims marker but diff doesn't have it → unmatched → exit 1"""
    import onibus.plan_doc
    plan = tmp_path / "plan.md"
    plan.write_text("r[gw.foo.bar] r[gw.missing.one]\n")
    diff = "+++ b/x.rs\n@@ -0,0 +1,1 @@\n+// r[impl gw.foo.bar]\n"
    monkeypatch.setattr(onibus.plan_doc, "git_try", lambda *a, **kw: diff)
    r = onibus.plan_doc.tracey_coverage("branch", plan)
    assert r.total == 2
    assert r.covered == 1
    assert r.unmatched == ["gw.missing.one"]


def test_tracey_coverage_verify_also_counts(tmp_path: Path, monkeypatch):
    """r[verify ...] without r[impl ...] still counts as covered (test-only plan)."""
    import onibus.plan_doc
    plan = tmp_path / "plan.md"
    plan.write_text("r[sched.retry.once]\n")
    diff = "+++ b/x.rs\n@@ -0,0 +1,1 @@\n+// r[verify sched.retry.once]\n"
    monkeypatch.setattr(onibus.plan_doc, "git_try", lambda *a, **kw: diff)
    r = onibus.plan_doc.tracey_coverage("branch", plan)
    assert r.unmatched == []
    assert r.markers[0].verify_loc == "x.rs:1"
    assert r.markers[0].impl_loc is None
