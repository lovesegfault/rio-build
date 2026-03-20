"""Tests for DAG-orchestration scripts.

Run: pytest .claude/lib/

rio-build deltas:
  - fixture paths: crates/rix-*/ → rio-*/ (real crate paths)
  - qa_mechanical_check: SEMANTIC INVERSION. rix defines r[plan.*] markers
    per plan; rio-build tracey is domain-indexed. r[plan.*] in a plan doc
    is POLLUTION → FAIL. Zero r[domain.*] refs → WARN (not FAIL — refactor
    plans legitimately cite zero).
  - PlanFile validator: +migrations/ +infra/ +scripts/ +codecov.yml,
    -systemd/ -tests/ -benches/ -deny.toml
  - rename_unassigned fixtures: no r[plan.*] markers (use plain P<N> refs);
    the string-replace logic is format-agnostic
  - agent-def tests: @pytest.mark.skip pending separate agent port
"""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from pydantic import ValidationError

import onibus
from onibus import REPO_ROOT
from onibus.dag import Dag, gate_is_clear
from onibus.jsonl import append_jsonl, consume_jsonl, read_jsonl, remove_jsonl
from onibus.models import (
    AgentRow,
    CollisionRow,
    CoverageResult,
    Followup,
    FollowupOrigin,
    Gate,
    KnownFlake,
    MergeQueueRow,
    MergerReport,
    MergeSha,
    PlanFile,
    PlanRow,
    Worktree,
)
from onibus.plan_doc import (
    plan_doc_deps,
    plan_doc_files,
    plan_doc_src_files,
    plan_doc_t_count,
    qa_mechanical_check,
)
from onibus.tracey import TRACEY_DOMAINS


# ─── phase doc parsing ───────────────────────────────────────────────────────


def test_plan_doc_src_files_finds_full_paths(plan_doc_full_paths: Path):
    files = plan_doc_src_files(plan_doc_full_paths)
    assert "rio-scheduler/src/actor/completion.rs" in files
    assert "rio-store/src/manifest.rs" in files
    # Tree-style bare name does NOT match (no rio-*/ prefix on that line)
    assert "actor.rs" not in files
    assert not any("actor.rs" in f for f in files)


def test_plan_doc_src_files_empty_for_bare_names(plan_doc_bare_names: Path):
    """The wave-1 bug: docs with only bare names yield empty — the check is
    empty, not clean. CollisionReport.source == 'none' flags this."""
    assert plan_doc_src_files(plan_doc_bare_names) == []


def test_plan_doc_t_count(plan_doc_full_paths: Path, plan_doc_bare_names: Path):
    assert plan_doc_t_count(plan_doc_full_paths) == 2
    assert plan_doc_t_count(plan_doc_bare_names) == 1


# ─── plan_doc_deps (fenced + prose fallback) ─────────────────────────────────


def test_plan_doc_deps_reads_fence(tmp_path: Path):
    doc = tmp_path / "plan-0999-x.md"
    doc.write_text(
        "# Phase 999\n\n## Dependencies\n\n"
        '```json deps\n{"deps": [79, 115], "soft_deps": [12], "note": "ship-either-order"}\n```\n\n'
        "**Depends on:** [P0079](plan-0079-store-gc.md) — prose below the fence.\n"
    )
    got = plan_doc_deps(doc)
    assert got["source"] == "fence"
    assert got["deps"] == [79, 115]
    assert got["soft_deps"] == [12]
    assert got["note"] == "ship-either-order"


def test_plan_doc_deps_falls_back_to_prose(tmp_path: Path):
    doc = tmp_path / "plan-0998-x.md"
    doc.write_text(
        "# Phase 998\n\n## Dependencies\n\n"
        "**Depends on:** [P0068](plan-0068-fod-pasta.md), [P0094](plan-0094-atomic-file.md),\n"
        "[P0099](plan-0099-no-insecure-transport.md) — multi-line prose.\n\n"
        "**Conflicts with:** the P0022 sandbox chain — NOT a dep, next paragraph.\n"
    )
    got = plan_doc_deps(doc)
    assert got["source"] == "prose"
    assert got["deps"] == [68, 94, 99]  # P0022 from Conflicts line NOT included
    assert got["soft_deps"] == []


def test_plan_doc_deps_none_when_absent(tmp_path: Path):
    doc = tmp_path / "plan-0997-x.md"
    doc.write_text("# Phase 997\n\n## Tasks\n\nNo dep section at all.\n")
    got = plan_doc_deps(doc)
    assert got["source"] == "none"
    assert got["deps"] == []


def test_plan_doc_deps_prose_none_keyword(tmp_path: Path):
    # P0170-style: **Depends on:** none — architectural.
    doc = tmp_path / "plan-0996-x.md"
    doc.write_text("## Dependencies\n\n**Depends on:** none — architectural.\n")
    got = plan_doc_deps(doc)
    assert got["source"] == "prose"
    assert got["deps"] == []  # no P<int> tokens → empty list


# ─── state JSONL ─────────────────────────────────────────────────────────────


def test_jsonl_roundtrip(tmp_path: Path):
    p = tmp_path / "agents.jsonl"
    rows = [
        AgentRow(plan="P0120", role="impl", status="done", worktree="/x/p120"),
        AgentRow(plan="P0109", role="impl", status="done", worktree="/x/p109"),
        AgentRow(plan="P0120", role="verify", status="running", agent_id="abc123"),
    ]
    for r in rows:
        append_jsonl(p, r)
    got = read_jsonl(p, AgentRow)
    assert len(got) == 3
    assert got[0].plan == "P0120"
    assert got[0].agent_id is None
    assert got[2].role == "verify"
    assert got[2].agent_id == "abc123"


def test_mergesha_roundtrip(tmp_path: Path):
    """MergeSha writes via append_jsonl and reads via read_jsonl. The
    ts datetime roundtrips through ISO string. Last-row-per-mc-wins
    is a dict-comp property, not a model property — tested separately
    in test_cadence_range_last_mc_wins if that exists."""
    from datetime import datetime, timezone

    p = tmp_path / "merge-shas.jsonl"
    row = MergeSha(mc=42, sha="deadbeef" * 5, ts=datetime.now(timezone.utc))
    append_jsonl(p, row)
    got = read_jsonl(p, MergeSha)
    assert len(got) == 1
    assert got[0].mc == 42
    assert got[0].sha == "deadbeef" * 5
    # ts roundtrips (pydantic serializes datetime → ISO, parses back)
    assert abs((got[0].ts - row.ts).total_seconds()) < 1


def test_mergesha_sha_pattern_rejects():
    """The sha Field pattern ^[0-9a-f]{8,40}$ — same as Mitigation.landed_sha.
    Catches: 7-char git-short (too ambiguous), uppercase (git is lowercase),
    non-hex, empty. Raw json.dumps didn't validate any of this; a typo'd
    sha would surface as 'fatal: bad object' in _cadence_range's git diff
    much later."""
    from datetime import datetime, timezone

    ts = datetime.now(timezone.utc)
    # 7-char: rejected (min 8)
    with pytest.raises(ValidationError):
        MergeSha(mc=1, sha="abc1234", ts=ts)
    # uppercase: rejected
    with pytest.raises(ValidationError):
        MergeSha(mc=1, sha="DEADBEEF", ts=ts)
    # non-hex: rejected
    with pytest.raises(ValidationError):
        MergeSha(mc=1, sha="ghijklmn", ts=ts)
    # 8-char lowercase hex: accepted
    MergeSha(mc=1, sha="deadbeef", ts=ts)
    # 40-char full: accepted
    MergeSha(mc=1, sha="a" * 40, ts=ts)
    # negative mc: rejected (ge=0)
    with pytest.raises(ValidationError):
        MergeSha(mc=-1, sha="deadbeef", ts=ts)


def test_read_jsonl_missing_file(tmp_path: Path):
    assert read_jsonl(tmp_path / "nope.jsonl", AgentRow) == []


def test_read_jsonl_skips_comments(tmp_path: Path):
    p = tmp_path / "flakes.jsonl"
    p.write_text(
        "# header comment — bridge while fix pending\n"
        "# second comment line\n"
        + KnownFlake(
            test="x",
            symptom="s",
            root_cause="rc",
            fix_owner="P0143",
            fix_description="d",
            retry="Once",
        ).model_dump_json()
        + "\n"
    )
    rows = read_jsonl(p, KnownFlake)
    assert len(rows) == 1
    assert rows[0].test == "x"


def test_consume_truncates(tmp_path: Path):
    p = tmp_path / "cov.jsonl"
    append_jsonl(
        p, CoverageResult(branch="p99", exit_code=1, log_path="/tmp/x", merged_at="abc")
    )
    append_jsonl(
        p, CoverageResult(branch="p98", exit_code=0, log_path="/tmp/y", merged_at="def")
    )
    got = consume_jsonl(p, CoverageResult)
    assert len(got) == 2
    assert got[0].branch == "p99"
    assert got[0].exit_code == 1
    assert p.read_text() == ""
    assert consume_jsonl(p, CoverageResult) == []


def test_remove_jsonl_preserves_header(tmp_path: Path):
    p = tmp_path / "flakes.jsonl"
    p.write_text("# Known flaky tests — bridge while fix pending\n")
    for test_name in ("a", "b", "c"):
        append_jsonl(
            p,
            KnownFlake(
                test=test_name,
                symptom="s",
                root_cause="rc",
                fix_owner="P0143",
                fix_description="d",
                retry="Once",
            ),
        )
    n = remove_jsonl(p, KnownFlake, lambda f: f.test == "b")
    assert n == 1
    remaining = read_jsonl(p, KnownFlake)
    assert [r.test for r in remaining] == ["a", "c"]
    assert p.read_text().startswith("# Known flaky tests")


def test_agent_row_literal_rejects():
    with pytest.raises(ValidationError):
        AgentRow(plan="P0001", role="foo", status="running")
    # Old status name is rejected — migration maps done-unverified → done
    with pytest.raises(ValidationError):
        AgentRow(plan="P0001", role="impl", status="done-unverified")


def test_followup_severity_rejects():
    with pytest.raises(ValidationError) as exc:
        Followup(
            severity="bug",
            description="x",
            proposed_plan="P-new",
            source_plan="P0099",
            timestamp="2026-01-01",
        )
    assert "bug" in str(exc.value)


def test_merge_queue_verdict_rejects():
    with pytest.raises(ValidationError):
        MergeQueueRow(plan="P0001", worktree="/x", verdict="MAYBE", commit="abc")


def test_followup_discovered_from_optional():
    # None is valid — tooling/meta followups have no originating phase.
    f = Followup(
        severity="trivial",
        description="fix flag",
        proposed_plan="P-batch-tooling",
        source_plan="tooling",
        origin="coordinator",
        timestamp="2026-01-01",
    )
    assert f.discovered_from is None
    # Explicit int is valid — the structured twin of source_plan.
    f2 = Followup(
        severity="correctness",
        description="auto-gc yield_now",
        proposed_plan="P-new",
        source_plan="P0109",
        origin="reviewer",
        discovered_from=109,
        timestamp="2026-01-01",
    )
    assert f2.discovered_from == 109
    # Roundtrips through JSONL.
    assert Followup.model_validate_json(f2.model_dump_json()).discovered_from == 109


def test_followup_origin_validates():
    """FollowupOrigin is a Literal — rejects anything not in the enum, case-
    sensitively. Required (CLI always sets it from the positional)."""
    # Valid origins.
    for o in (
        "reviewer",
        "consolidator",
        "bughunter",
        "coverage",
        "inline",
        "coordinator",
    ):
        f = Followup(
            severity="trivial",
            description="x",
            proposed_plan="P-new",
            source_plan=o,
            origin=o,
            timestamp="t",
        )
        assert f.origin == o
    # Missing → ValidationError (required).
    with pytest.raises(ValidationError):
        Followup(
            severity="trivial", description="x", proposed_plan="P-new",
            source_plan="P0001", timestamp="t",
        )
    # Literal rejects case variants and typos.
    for bad in ("Consolidator", "BUGHUNTER", "review", "unknown"):
        with pytest.raises(ValidationError):
            Followup(
                severity="trivial",
                description="x",
                proposed_plan="P-new",
                source_plan="x",
                origin=bad,
                timestamp="t",
            )


def test_proposed_plan_validator_rejects():
    """proposed_plan must match P-new | P-batch-<kind> | P<NNNN> exactly.
    Same pattern as KnownFlake.fix_owner — prose rule → validator."""
    # Valid shapes.
    for ok in (
        "P-new",
        "P-batch-trivial",
        "P-batch-tests",
        "P-batch-tooling",
        "P-batch-a-b-c",
        "P0143",
        "P9999",
    ):
        Followup(
            severity="trivial",
            description="x",
            proposed_plan=ok,
            source_plan="P0001",
            origin="reviewer",
            timestamp="t",
        )
    # Invalid shapes — caught at write time.
    for bad in (
        "p-new",
        "P-New",
        "P-123",
        "P123",
        "P-batch",
        "P-batch-",
        "P-batch-Foo",
        "new",
        "P01234",
        "",
    ):
        with pytest.raises(ValidationError) as exc:
            Followup(
                severity="trivial",
                description="x",
                proposed_plan=bad,
                source_plan="P0001",
                origin="reviewer",
                timestamp="t",
            )
        assert "proposed_plan" in str(exc.value)


def test_merger_report_roundtrip():
    """MergerReport — all status/abort_reason combos validate and roundtrip
    through JSON. This is the fence the merger emits; merge-impl/SKILL and
    dag-run match on the typed fields."""
    # Merged — full shape.
    m = MergerReport(
        status="merged",
        hash="abc1234",
        commits_merged=2,
        stale_verify_commits_moved=0,
        dag_delta_commit="def5678",
        cov_log="/tmp/merge-cov-p134.log",
        behind_worktrees=["/root/src/rio-build/p135@p135:behind=3"],
        cleanup="ok",
    )
    assert m.abort_reason is None
    m2 = MergerReport.model_validate_json(m.model_dump_json())
    assert m2.status == "merged"
    assert m2.hash == "abc1234"
    assert m2.behind_worktrees == ["/root/src/rio-build/p135@p135:behind=3"]
    # Merged with stale-verify signal.
    stale = MergerReport(status="merged", hash="abc", stale_verify_commits_moved=7)
    assert stale.stale_verify_commits_moved > 3  # the soft-signal threshold
    # Aborted — every abort_reason validates.
    for reason in (
        "rebase-conflict",
        "ff-rejected",
        "ci-failed",
        "non-convco-commits",
        "worktree-missing",
        "already-merged",
    ):
        a = MergerReport(
            status="aborted",
            abort_reason=reason,
            failure_detail="<payload>",
        )
        assert a.abort_reason == reason
        assert (
            MergerReport.model_validate_json(a.model_dump_json()).abort_reason == reason
        )
    # dag_delta_commit can be "already-done" (row was already DONE).
    MergerReport(status="merged", hash="abc", dag_delta_commit="already-done")
    # Literal rejects typos.
    with pytest.raises(ValidationError):
        MergerReport(status="MERGED")
    with pytest.raises(ValidationError):
        MergerReport(status="aborted", abort_reason="ci-fail")  # missing -ed
    # Defaults: everything optional except status.
    minimal = MergerReport(status="aborted")
    assert minimal.abort_reason is None
    assert minimal.behind_worktrees == []
    assert minimal.cleanup == "ok"


def test_consolidator_followup_format_is_valid():
    """The rio-impl-consolidator agent def tells the agent to emit followups
    with the positional "consolidator" and severity="feature"|"trivial". Check
    both shapes validate against the Followup model, that "consolidator"
    is a valid FollowupOrigin (producer/contract agreement), and that it
    yields discovered_from=None."""
    from typing import get_args

    # Producer-side: the positional the agent emits is a valid FollowupOrigin.
    assert "consolidator" in get_args(FollowupOrigin)
    # Shape from agent-def step 5: a real finding.
    proposal = Followup.model_validate(
        {
            "severity": "feature",
            "description": "CONSOLIDATION: match-arm pattern across plans 170,132,127 "
            "— extract to trait dispatch. Evidence: main.rs:89,142,201. "
            "Worth it if: P01xx adds a 4th arm.",
            "file_line": "rio-cli/src/main.rs",
            "proposed_plan": "P-new",
            "source_plan": "consolidator",
            "origin": "consolidator",  # state.py CLI sets this: FollowupOrigin match
            "discovered_from": None,  # state.py CLI: not P<N> → None
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    assert proposal.severity == "feature"
    assert proposal.origin == "consolidator"
    assert proposal.discovered_from is None
    # Shape from agent-def step 6: no-pattern marker.
    marker = Followup.model_validate(
        {
            "severity": "trivial",
            "description": "CONSOLIDATION: reviewed merges abc..def (N=5), "
            "no duplication pattern above noise floor.",
            "proposed_plan": "P-batch-trivial",
            "source_plan": "consolidator",
            "origin": "consolidator",
            "discovered_from": None,
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    assert marker.file_line is None  # optional — no primary file for no-pattern


def test_bughunter_followup_format_is_valid():
    """rio-impl-bughunter emits followups with the positional "bughunter",
    severity="correctness"|"test-gap"|"trivial". All three shapes must
    validate; "bughunter" is a valid FollowupOrigin (producer/contract
    agreement) and yields discovered_from=None."""
    from typing import get_args

    # Producer-side: the positional the agent emits is a valid FollowupOrigin.
    assert "bughunter" in get_args(FollowupOrigin)
    # Step 5: correctness finding (smell accumulation)
    Followup.model_validate(
        {
            "severity": "correctness",
            "description": "BUGHUNT: 7 .unwrap() added in store/cas.rs across "
            "plans 170,174,187. Risk: network blip panics daemon.",
            "file_line": "rio-store/src/cas.rs",
            "proposed_plan": "P-new",
            "source_plan": "bughunter",
            "origin": "bughunter",
            "discovered_from": None,
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    # Step 5: test-gap finding (error-path coverage)
    Followup.model_validate(
        {
            "severity": "test-gap",
            "description": "BUGHUNT: fn fetch_narinfo returns Result, no test "
            "exercises Err arm.",
            "file_line": "rio-store/src/cas.rs:142",
            "proposed_plan": "P-batch-tests",
            "source_plan": "bughunter",
            "origin": "bughunter",
            "discovered_from": None,
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    # Step 6: null result with under-threshold counts
    null = Followup.model_validate(
        {
            "severity": "trivial",
            "description": "BUGHUNT: reviewed merges abc..def (N=7), no "
            "cross-plan pattern above threshold. Smell counts: unwrap=3, "
            "swallow=0, orphan-todo=1, allow=2.",
            "proposed_plan": "P-batch-trivial",
            "source_plan": "bughunter",
            "origin": "bughunter",
            "discovered_from": None,
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    assert null.origin == "bughunter"
    assert null.discovered_from is None


_AGENTS_DIR = Path(__file__).resolve().parents[1] / "agents"


@pytest.mark.skip(reason="rio-adapt: agent defs differ, manual review")
@pytest.mark.parametrize(
    "name,source",
    [("rio-impl-consolidator", "consolidator"), ("rio-impl-bughunter", "bughunter")],
)
def test_cadence_agent_def_parses(name: str, source: str):
    """Cadence agent defs have YAML frontmatter, read-only tool list,
    reference the real state.py CLI entrypoint with a positional that
    validates as a FollowupOrigin literal — producer-side contract check.
    Skipped in .#ci sandbox (agents/ isn't in the source fileset — this
    is a local-dev sanity check)."""
    from typing import get_args

    body = (_AGENTS_DIR / f"{name}.md").read_text()
    # Frontmatter fence
    assert body.startswith("---\n")
    fm_end = body.index("\n---\n", 4)
    fm = body[4:fm_end]
    assert f"name: {name}" in fm
    assert "tools:" in fm
    # Read-only by construction: no Edit/Write in tool list
    tools_line = next(ln for ln in fm.splitlines() if ln.startswith("tools:"))
    assert "Edit" not in tools_line
    assert "Write" not in tools_line
    assert "Bash" in tools_line and "Read" in tools_line
    # Protocol references the real CLI entrypoint with the agent's source name
    assert f"onibus state followup {source}" in body
    # Producer-side: the positional IS a valid FollowupOrigin — CLI validates
    # it at write time, so a typo here would fail the agent's first sink write.
    assert source in get_args(FollowupOrigin)
    # And the agent def mentions FollowupOrigin (knows it's typed, not free-text)
    assert "FollowupOrigin" in body


# ─── reviewer + qa (verifier split) ───────────────────────────────────────────


def test_reviewer_followup_format_valid():
    """rio-impl-reviewer emits followups with source_plan="P<N>" (the plan
    being reviewed). All severities from the reviewer's §5 catalog validate;
    P<N> → discovered_from parsed as int (closes the loop: finding during
    P0109's review → new plan deps include 109)."""
    # Smell catalog hit: .unwrap() in prod
    smell = Followup.model_validate(
        {
            "severity": "correctness",
            "description": ".unwrap() on network result — panics on blip",
            "file_line": "rio-store/src/cas.rs:520",
            "proposed_plan": "P-new",
            "deps": "P0109",
            "source_plan": "P0109",
            "origin": "reviewer",  # state.py CLI: P<N> → origin=reviewer (inferred)
            "discovered_from": 109,  # state.py CLI: _P_NUM_RE match → int
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    assert smell.origin == "reviewer"
    assert smell.discovered_from == 109
    # Test-gap: pub fn with zero callers in tests
    gap = Followup.model_validate(
        {
            "severity": "test-gap",
            "description": "pub fn fetch_narinfo — no test calls it",
            "file_line": "rio-store/src/cas.rs:142",
            "proposed_plan": "P-batch-tests",
            "deps": "P0109",
            "source_plan": "P0109",
            "origin": "reviewer",
            "discovered_from": 109,
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    assert gap.severity == "test-gap"
    # Trivial: orphan TODO (no plan number)
    Followup.model_validate(
        {
            "severity": "trivial",
            "description": "TODO at line 89 has no P-ref",
            "file_line": "rio-scheduler/src/assignment.rs:89",
            "proposed_plan": "P-batch-trivial",
            "deps": "P0109",
            "source_plan": "P0109",
            "origin": "reviewer",
            "discovered_from": 109,
            "timestamp": "2026-03-18T00:00:00",
        }
    )
    # AgentRole accepts "review" (verifier→reviewer split added it)
    AgentRow(plan="P0109", role="review", status="running")


@pytest.mark.skip(reason="rio-adapt: agent defs differ, manual review")
def test_reviewer_agent_def_parses():
    """rio-impl-reviewer agent def: read-only tools, references state.py
    followup with P<N> positional (not a named source like cadence agents)."""
    body = (_AGENTS_DIR / "rio-impl-reviewer.md").read_text()
    assert body.startswith("---\n")
    fm_end = body.index("\n---\n", 4)
    fm = body[4:fm_end]
    assert "name: rio-impl-reviewer" in fm
    tools_line = next(ln for ln in fm.splitlines() if ln.startswith("tools:"))
    assert "Edit" not in tools_line
    assert "Write" not in tools_line
    assert "Bash" in tools_line and "Read" in tools_line
    # Reviewer uses P<N> as source (not "reviewer" — discovered_from parses to int)
    assert "onibus state followup P<N>" in body
    # Severity table present (moved from verifier §7)
    assert "`trivial`" in body and "`correctness`" in body and "`test-gap`" in body


# ─── qa_mechanical_check (rio-build semantic inversion) ───────────────────────
#
# rix: plan docs DEFINE r[plan.pNNNN.*] markers → zero markers = FAIL.
# rio-build: plan docs REFERENCE r[domain.*] markers → r[plan.*] = FAIL
# (pollution), zero domain refs = WARN (refactor plans cite zero).


def test_qa_passes_valid_doc(tmp_path: Path):
    """Clean synthetic doc: valid fences, deps exist, domain markers referenced → no issues."""
    doc = tmp_path / "plan-0999-valid.md"
    doc.write_text(
        "# Plan 999\n\n"
        "## Dependencies\n\n"
        '```json deps\n{"deps": [79, 115]}\n```\n\n'
        "## Files\n\n"
        '```json files\n[{"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY"}]\n```\n\n'
        "## Tracey\n\n"
        "Implements r[sched.actor.dispatch] — assignment loop.\n"
        "Verifies r[gw.opcode.build-paths] — wire encoding.\n"
    )
    dag_plans = {79, 115, 1, 2}
    issues = qa_mechanical_check(doc, dag_plans)
    assert [sev for sev, _ in issues if sev == "FAIL"] == []
    # No WARN either — both fences present, deps declared, domain markers present
    assert issues == []


def test_qa_rejects_invalid_fence(tmp_path: Path):
    """Malformed json files entry (bad path prefix) → PlanFile ValidationError → FAIL."""
    doc = tmp_path / "plan-0998-badfence.md"
    doc.write_text(
        "# Plan 998\n\n"
        '```json files\n[{"path": "src/foo.rs", "action": "MODIFY"}]\n```\n\n'
        "Implements r[sched.actor.foo].\n"
    )
    issues = qa_mechanical_check(doc, set())
    fails = [msg for sev, msg in issues if sev == "FAIL"]
    assert len(fails) == 1
    # Pydantic error mentions the bad path (pattern mismatch)
    assert "json files[0]" in fails[0]
    assert "src/foo.rs" in fails[0] or "pattern" in fails[0]


def test_qa_rejects_missing_dep(tmp_path: Path):
    """Dep in json deps fence not in dag.jsonl → FAIL. 9-digit placeholder skipped."""
    doc = tmp_path / "plan-0997-baddep.md"
    doc.write_text(
        "# Plan 997\n\n"
        '```json deps\n{"deps": [79, 287, 924999902]}\n```\n\n'
        '```json files\n[{"path": "rio-scheduler/src/assignment.rs"}]\n```\n\n'
        "Implements r[sched.actor.foo].\n"
    )
    dag_plans = {79, 115}  # 287 missing; 924999902 is placeholder
    issues = qa_mechanical_check(doc, dag_plans)
    fails = [msg for sev, msg in issues if sev == "FAIL"]
    assert fails == ["dep P0287 not in dag.jsonl"]
    # 79 present, 924999902 skipped (placeholder ≥ 9e8) — only 287 fails


def test_qa_rejects_plan_markers(tmp_path: Path):
    """r[plan.*] markers → FAIL (rio-build tracey is domain-indexed; plan
    docs don't DEFINE markers, they REFERENCE domain markers). This is the
    semantic inversion from rix — presence is pollution, not absence."""
    doc = tmp_path / "plan-0996-pollution.md"
    doc.write_text(
        "# Plan 996\n\n"
        '```json files\n[{"path": "rio-scheduler/src/assignment.rs"}]\n```\n\n'
        "## Exit criteria\n\n"
        "r[plan.p0996.foo-bar] — test_foo asserts X.\n"
        "r[plan.p0996.baz] — benchmark crosses threshold.\n"
    )
    issues = qa_mechanical_check(doc, set())
    fails = [msg for sev, msg in issues if sev == "FAIL"]
    assert len(fails) == 1
    assert "r[plan.*]" in fails[0]
    assert "domain-indexed" in fails[0]
    # 2 markers found, message says count
    assert "2 " in fails[0]


def test_qa_warns_zero_domain_markers(tmp_path: Path):
    """Zero r[domain.*] refs → WARN (not FAIL). Refactor/tooling plans
    legitimately cite zero; tracey-validate in .#ci catches dangling refs."""
    doc = tmp_path / "plan-0995-nomarker.md"
    doc.write_text(
        "# Plan 995\n\n"
        '```json files\n[{"path": "rio-scheduler/src/assignment.rs"}]\n```\n\n'
        "## Tasks\n\nRefactor: extract helper. No spec change.\n"
    )
    issues = qa_mechanical_check(doc, set())
    fails = [msg for sev, msg in issues if sev == "FAIL"]
    warns = [msg for sev, msg in issues if sev == "WARN"]
    assert fails == []  # ← NOT a FAIL (inversion from rix)
    assert any("zero r[domain.*]" in w for w in warns)
    # AgentRole accepts "qa" (plan-doc gate added it)
    AgentRow(plan="docs-249999", role="qa", status="running")


def test_tracey_domains_matches_spec():
    """TRACEY_DOMAINS must match the set of r[domain.*] prefixes in docs/src.

    Hardcoding the alternation at 8 sites previously missed `common` + `dash`:
    P0280 (UNIMPL, uses r[dash.*]) would have false-FAILed validation. This
    test catches drift in either direction — spec adds a domain, or the
    constant has a phantom domain that no spec uses.
    """
    docs = REPO_ROOT / "docs" / "src"
    if not docs.exists():
        pytest.skip("docs/src not present (not a full checkout)")
    spec_domains: set[str] = set()
    for md in docs.rglob("*.md"):
        for m in re.finditer(r"^r\[([a-z]+)\.", md.read_text(), re.MULTILINE):
            spec_domains.add(m.group(1))
    assert spec_domains == set(TRACEY_DOMAINS), (
        f"drift: spec has {spec_domains - set(TRACEY_DOMAINS)}, "
        f"constant has {set(TRACEY_DOMAINS) - spec_domains}"
    )


# rio-adapt: dropped test_qa_rejects_bad_marker_slug — rio-build doesn't
# slug-check domain markers (tracey-validate in .#ci does that independently).


@pytest.mark.skip(reason="rio-adapt: agent defs differ, manual review")
def test_verifier_narrowed_no_followup_sink():
    """Regression guard: the narrowed verifier protocol must NOT reference
    state.py followup. Followup sink writes moved to rio-impl-reviewer.
    Skipped in .#ci sandbox (agents/ not in fileset)."""
    verifier = _AGENTS_DIR / "rio-impl-validator.md"
    if not verifier.exists():
        pytest.skip(".claude/agents/ not in scripts-pytest fileset")
    body = verifier.read_text()
    # No sink-write CLI invocations (mentioning the file in "you don't write
    # to this; reviewer does" prose is fine — it's the bash pattern we guard)
    assert "onibus state followup" not in body
    onibus_lines = [ln for ln in body.splitlines() if ".claude/bin/onibus" in ln]
    assert not any("followup" in ln for ln in onibus_lines)
    # PARTIAL preserved in the verdict table
    assert "PARTIAL" in body
    assert "tracey coverage incomplete" in body.lower()
    # BEHIND preserved
    assert "BEHIND" in body


def test_gate_plan_merged_clears_when_done(tmp_path: Path):
    # Mechanical check: dag.jsonl[132].status == "DONE" → gate clears.
    gate = Gate(kind="plan_merged", plan=132)
    # P0132 UNIMPL → gate blocks.
    dag_unimpl = Dag([_mk_row(127, status="UNIMPL"), _mk_row(132, status="UNIMPL")])
    assert gate_is_clear(gate, dag_unimpl) is False
    # P0132 DONE → gate clears. (Dag is immutable — rebuild.)
    dag_done = Dag([_mk_row(127, status="UNIMPL"), _mk_row(132, status="DONE")])
    assert gate_is_clear(gate, dag_done) is True
    # None gate always clears (ready to merge).
    assert gate_is_clear(None, dag_done) is True
    # ci_green: S3-403-aware check — greps for "status = Built".
    log = tmp_path / "ci.log"
    ci_gate = Gate(kind="ci_green", log_path=str(log))
    empty = Dag([])
    assert gate_is_clear(ci_gate, empty) is False  # missing file
    log.write_text("...\nerror: 403 PutObject\n...\nstatus = Built\n")
    assert gate_is_clear(ci_gate, empty) is True
    log.write_text("...\nbuild failed\n")
    assert gate_is_clear(ci_gate, empty) is False


def test_gate_manual_never_auto_clears():
    gate = Gate(kind="manual", reason="waiting on upstream nixpkgs bump")
    # Regardless of DAG state — manual gates need coordinator to remove the row.
    assert gate_is_clear(gate, Dag([])) is False
    assert gate_is_clear(gate, Dag([_mk_row(n, status="DONE") for n in range(200)])) is False


def test_dag_set_status_rejects_invalid_status():
    # PlanRow has model_config = ConfigDict(validate_assignment=True) —
    # without it, `r.status = "BOGUS"` silently succeeds (pydantic v2
    # doesn't validate on attribute assignment by default). The
    # dag-set-status CLI relies on this.
    r = PlanRow(plan=1, title="t", status="UNIMPL")
    with pytest.raises(ValidationError):
        r.status = "BOGUS"  # type: ignore[assignment]
    # Valid assignment still works.
    r.status = "DONE"
    assert r.status == "DONE"


# ─── PlanRow + dag-render ───────────────────────────────────────────────────


def _mk_row(n, status="DONE", deps=(), **kw):
    return PlanRow(
        plan=n,
        title=f"t{n}",
        deps=list(deps),
        status=status,
        tracey_total=1,
        tracey_covered=1,
        crate="x",
        **kw,
    )


def test_phaserow_status_rejects():
    with pytest.raises(ValidationError):
        PlanRow(plan=1, title="x", status="BOGUS")
    # Valid statuses (including RESERVED for P0122)
    for s in ("UNIMPL", "PARTIAL", "DONE", "RESERVED"):
        PlanRow(plan=1, title="x", status=s)


def test_dag_set_status_roundtrips(tmp_path: Path, monkeypatch):
    import onibus.dag

    jl = tmp_path / "dag.jsonl"
    monkeypatch.setattr(onibus.dag, "DAG_JSONL", jl)
    for r in (_mk_row(1), _mk_row(2, status="UNIMPL", deps=[1])):
        append_jsonl(jl, r)
    # Simulate the CLI body: read, edit one row, rewrite
    rows = read_jsonl(jl, PlanRow)
    for r in rows:
        if r.plan == 2:
            r.status = "DONE"
            r.note = "(landed @ abc)"
    jl.write_text("".join(r.model_dump_json() + "\n" for r in rows))
    # Read back
    got = read_jsonl(jl, PlanRow)
    assert len(got) == 2
    r2 = next(r for r in got if r.plan == 2)
    assert r2.status == "DONE"
    assert r2.note == "(landed @ abc)"
    assert got[0].status == "DONE"  # row 1 unchanged


def test_dag_render_idempotent(tmp_path: Path):
    rows = [
        _mk_row(1),
        _mk_row(2, status="UNIMPL", deps=[1]),  # frontier (dep 1 DONE)
        _mk_row(3, status="UNIMPL", deps=[2]),  # blocked (dep 2 UNIMPL)
    ]
    t1 = Dag(rows).render()
    t2 = Dag(rows).render()
    assert t1 == t2
    # Frontier bold: P0002 yes (dep DONE), P0003 no (dep UNIMPL)
    assert "**UNIMPL**" in t1.splitlines()[3]  # P0002 row
    assert "**UNIMPL**" not in t1.splitlines()[4]  # P0003 row
    assert "| UNIMPL |" in t1.splitlines()[4]


def test_dag_render_emits_to_stdout():
    """dag-render is stdout-only now — no file splice. DAG.md is gone;
    dag.jsonl IS the DAG. Render produces a markdown table string."""
    rows = [_mk_row(1), _mk_row(2, status="UNIMPL")]
    table = Dag(rows).render()
    lines = table.splitlines()
    # Header + separator + 2 data rows
    assert len(lines) == 4
    assert lines[0].startswith("| P# | Title |")
    assert lines[1].startswith("|---|")
    assert "| P0001 | t1 |" in lines[2]
    assert "| P0002 | t2 |" in lines[3]
    # Pure string — no file IO, no splice markers
    assert "BEGIN GENERATED" not in table
    assert "END GENERATED" not in table


def test_plan_file_validates_path_prefix():
    # rio-build valid prefixes (state.py PlanFile pattern)
    PlanFile(path="rio-scheduler/src/assignment.rs")
    PlanFile(path="rio-store/src/manifest.rs", action="NEW")
    PlanFile(path="nix/vm-tests/foo.nix", action="NEW")
    PlanFile(path="flake.nix")
    PlanFile(path=".claude/lib/onibus/cli.py")
    PlanFile(path="Cargo.toml")
    PlanFile(path="docs/src/components/gateway.md")
    PlanFile(path="justfile")
    PlanFile(path=".config/tracey/config.styx")
    PlanFile(path="codecov.yml")
    # rio-build additions: migrations/ + infra/ + scripts/
    PlanFile(path="migrations/009_tenants.sql")
    PlanFile(path="infra/helm/rio-build/values.yaml")
    PlanFile(path="scripts/split-crds.sh")
    # rio-build removals (vs rix): -systemd/ -tests/ -benches/ -deny.toml
    with pytest.raises(ValidationError):
        PlanFile(path="src/foo.rs")  # missing rio-*/ prefix
    with pytest.raises(ValidationError):
        PlanFile(path="/abs/path.rs")
    with pytest.raises(ValidationError):
        PlanFile(path="rio-scheduler/foo.rs", action="INVALID")
    with pytest.raises(ValidationError):
        PlanFile(path="deny.toml")  # not in rio pattern (lives in .config/ now)
    with pytest.raises(ValidationError):
        PlanFile(path="systemd/rio-daemon.service")  # rio has no systemd/


def test_collision_row_roundtrip():
    r = CollisionRow(path="rio-scheduler/src/assignment.rs", plans=[109, 192, 193], count=3)
    j = r.model_dump_json()
    r2 = CollisionRow.model_validate_json(j)
    assert r2.plans == [109, 192, 193]
    assert r2.count == 3


def test_plan_doc_files_reads_fence(tmp_path: Path):
    doc = tmp_path / "plan-0999-x.md"
    doc.write_text(
        "# Plan 999\n\n## Files\n\n"
        "```json files\n"
        '[{"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T1"},'
        ' {"path": "rio-store/src/cas.rs", "action": "NEW", "note": "T2"}]\n'
        "```\n\n"
        "```\nrio-scheduler/src/\n└── assignment.rs\n```\n"
    )
    got = plan_doc_files(doc)
    assert got is not None
    assert len(got) == 2
    assert got[0]["path"] == "rio-scheduler/src/assignment.rs"
    assert got[0]["action"] == "MODIFY"
    assert got[1]["path"] == "rio-store/src/cas.rs"
    assert got[1]["action"] == "NEW"
    # Validates against PlanFile
    for f in got:
        PlanFile.model_validate(f)


def test_plan_doc_files_returns_none_without_fence(tmp_path: Path):
    """Old-format doc (box-drawing tree only) → None. Caller falls back
    to plan_doc_src_files() grep. This is the transition-period contract."""
    doc = tmp_path / "plan-0998-x.md"
    doc.write_text(
        "# Plan 998\n\n## Files\n\n```\nrio-scheduler/src/\n└── assignment.rs\n```\n"
    )
    assert plan_doc_files(doc) is None
    # But grep fallback still works
    assert plan_doc_src_files(doc) == []  # bare name, no full path


def test_dag_render_deps_raw_and_note():
    # P0152's escape hatch: renderer uses deps_raw verbatim
    r = _mk_row(
        152,
        status="UNIMPL",
        deps=[111],
        deps_raw="P20b(deferred-nodoc),P0111",
    )
    line = r.render(frontier=False)
    assert "| P20b(deferred-nodoc),P0111 |" in line
    # P0143's note: appended after status
    r2 = _mk_row(143, status="PARTIAL", note="(2/37 — T26/T28)")
    line2 = r2.render(frontier=False)
    assert "| PARTIAL (2/37 — T26/T28) |" in line2
    # P0122's complexity=None → '-'
    r3 = PlanRow(plan=122, title="(reserved)", status="RESERVED", complexity=None)
    assert r3.render(frontier=False).endswith("| - |")


def test_dag_frontier_excludes_deps_raw():
    # Conservative: deps_raw set → unknown dep state → not frontier.
    # P0152 has P20b which can't be an int dep; don't claim it's ready.
    rows = [
        _mk_row(111),  # DONE
        _mk_row(
            152, status="UNIMPL", deps=[111], deps_raw="P20b(deferred-nodoc),P0111"
        ),
    ]
    table = Dag(rows).render()
    # P0152 should NOT be bold despite dep 111 being DONE
    assert "**UNIMPL**" not in table.splitlines()[-1]


# ─── known-flakes validator ─────────────────────────────────────────────────


def test_known_flake_owner_plan():
    f = KnownFlake(
        test="x",
        symptom="s",
        root_cause="rc",
        fix_owner="P0143 T26",
        fix_description="widen gate",
        retry="Once",
    )
    assert f.owner_plan == 143
    f2 = KnownFlake(
        test="x",
        symptom="s",
        root_cause="rc",
        fix_owner="P0143",
        fix_description="d",
        retry="Once",
    )
    assert f2.owner_plan == 143


def test_known_flake_validator_rejects_placeholder():
    # The "bridge not parking lot" enforcement — placeholder owners rejected.
    with pytest.raises(ValidationError) as exc:
        KnownFlake(
            test="x",
            symptom="s",
            root_cause="rc",
            fix_owner="P-batch-tests",
            fix_description="d",
            retry="Once",
        )
    assert "/plan" in str(exc.value)
    with pytest.raises(ValidationError):
        KnownFlake(
            test="x",
            symptom="s",
            root_cause="rc",
            fix_owner="TODO",
            fix_description="d",
            retry="Once",
        )


def test_known_flake_crud_edits_worktree_not_main(tmp_repo: Path):
    """Absolute-path bug: `python3 /root/src/rio-build/main/.claude/lib/state.py`
    from a worktree → REPO_ROOT resolves to main (parents[2] of __file__),
    edit lands in main uncommitted. Relative invocation resolves REPO_ROOT
    to the worktree — both add and remove land there, git-trackable."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    flakes = tmp_repo / ".claude" / "known-flakes.jsonl"
    flakes.write_text("# header\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "seed", "--no-verify")

    entry = KnownFlake(
        test="test_foo",
        symptom="s",
        root_cause="rc",
        fix_owner="P0999",
        fix_description="d",
        retry="Once",
    ).model_dump_json()

    def _onibus(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [".claude/bin/onibus", *args],
            cwd=tmp_repo,
            check=True,
            capture_output=True,
            text=True,
        )

    # Add: lands in tmp_repo's copy; git sees it (step-0 seed commit shape)
    r = _onibus("flake", "add", entry)
    assert "warning:" not in r.stderr  # cwd == REPO_ROOT → guardrail silent
    assert "test_foo" in flakes.read_text()
    assert ".claude/known-flakes.jsonl" in _git(tmp_repo, "status", "--porcelain")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "chore(flakes): add", "--no-verify")

    # Remove: same mechanism, same trackability (step-6 shape)
    r = _onibus("flake", "remove", "test_foo")
    assert "warning:" not in r.stderr
    assert "test_foo" not in flakes.read_text()
    assert ".claude/known-flakes.jsonl" in _git(tmp_repo, "status", "--porcelain")


def test_flake_add_rejects_duplicate_test_key(tmp_repo: Path):
    """T1 guard: flake-add with an existing test key → rc=1, stderr
    message, file unchanged. Without the guard, append_jsonl silently
    creates a duplicate and flake-mitigation picks the first."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    flakes = tmp_repo / ".claude" / "known-flakes.jsonl"
    first = KnownFlake(
        test="dupkey::test_foo", symptom="s", root_cause="rc",
        fix_owner="P0999", fix_description="d", retry="Once",
    )
    flakes.write_text("# header\n" + first.model_dump_json() + "\n")

    # Second add with SAME test key → rejected.
    r = subprocess.run(
        [".claude/bin/onibus", "flake", "add", first.model_dump_json()],
        cwd=tmp_repo, capture_output=True, text=True,
    )
    assert r.returncode == 1
    assert "already exists" in r.stderr
    # File unchanged: still one JSON line, header preserved.
    lines = [ln for ln in flakes.read_text().splitlines() if not ln.startswith("#")]
    assert len(lines) == 1


def test_flake_mitigation_errors_on_multiple_matches(tmp_repo: Path):
    """T2 guard: if known-flakes.jsonl has two rows with the same test
    key (hand-edit or pre-guard history), flake-mitigation refuses
    rather than silently picking the first. rc=2 (distinct from
    rc=1 not-found)."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    flakes = tmp_repo / ".claude" / "known-flakes.jsonl"
    row = KnownFlake(
        test="dupkey::test_ambig", symptom="s", root_cause="rc",
        fix_owner="P0999", fix_description="d", retry="Once",
    ).model_dump_json()
    # Two rows, same test key — bypasses T1's guard by writing directly.
    flakes.write_text(f"# header\n{row}\n{row}\n")

    r = subprocess.run(
        [
            ".claude/bin/onibus", "flake", "mitigation",
            "dupkey::test_ambig", "999", "deadbeef", "n",
        ],
        cwd=tmp_repo, capture_output=True, text=True,
    )
    assert r.returncode == 2
    assert "AMBIGUOUS" in r.stderr
    # File untouched — no mitigation appended to either row.
    assert "deadbeef" not in flakes.read_text()


# ─── unassigned placeholder rename ───────────────────────────────────────────


def test_placeholder_re():
    from onibus.merge import _PLACEHOLDER_RE, _REAL_RE

    assert _PLACEHOLDER_RE.match("plan-924999901-foo.md").groups() == (
        "924999901",
        "foo",
    )
    assert _PLACEHOLDER_RE.match("plan-900000001-multi-word-slug.md").group(2) == (
        "multi-word-slug"
    )
    assert _PLACEHOLDER_RE.match("plan-0187-real.md") is None
    assert _PLACEHOLDER_RE.match("plan-92499990-eight-digits.md") is None
    # Real-phase regex excludes 9-digit placeholders (≤4 digit cap)
    assert _REAL_RE.match("plan-0187-bar.md").group(1) == "0187"
    assert _REAL_RE.match("plan-924999901-foo.md") is None


def _git(tmp_path: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=tmp_path, check=True, capture_output=True, text=True
    ).stdout.strip()


@pytest.fixture
def docs_branch(tmp_repo: Path, monkeypatch):
    """A docs-style branch with two 9-digit placeholder phase docs.

    rio-adapt: plan docs don't carry r[plan.*] markers (domain-indexed tracey).
    The rename_unassigned string-replace is format-agnostic — it replaces the
    literal placeholder number everywhere. Use plain P<N> prose refs instead."""
    from onibus import merge as rename_unassigned

    impl = tmp_repo / ".claude" / "work"
    impl.mkdir(parents=True)
    (impl / "plan-0100-existing.md").write_text("# real\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "seed", "--no-verify")

    _git(tmp_repo, "checkout", "-b", "docs-249999")
    # rio-adapt: no r[plan.*] markers — use plain P<N> prose. String-replace
    # hits P924999901 (doc body) and plan-924999902-*.md (cross-ref link).
    (impl / "plan-924999901-alpha.md").write_text(
        "# Phase 924999901\n\nSee P924999901 exit criteria.\n\n"
        "- [P924999902](plan-924999902-beta.md) dep\n"
    )
    (impl / "plan-924999902-beta.md").write_text("P924999902 exit criteria.\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: add placeholders", "--no-verify")

    monkeypatch.setattr(rename_unassigned, "_worktree_for", lambda _: tmp_repo)
    monkeypatch.setattr(rename_unassigned, "DOCS_DIR", impl)
    return tmp_repo, impl


def test_rename_rewrites_and_commits(docs_branch):
    from onibus.merge import rename_unassigned as run

    tmp_repo, impl = docs_branch
    report = run("docs-249999")

    assert len(report.mapping) == 2
    assert report.mapping[0].placeholder == "924999901"
    assert report.mapping[0].assigned == 101
    assert report.mapping[0].slug == "alpha"
    assert report.mapping[1].assigned == 102
    assert report.commit is not None

    assert not (impl / "plan-924999901-alpha.md").exists()
    alpha = (impl / "plan-0101-alpha.md").read_text()
    assert "924999901" not in alpha
    # rio-adapt: plain-text P<N> ref rewritten (.md uses :04d → P0101)
    assert "P0101" in alpha
    # Cross-ref to sibling placeholder also rewritten in the same pass
    assert "[P0102](plan-0102-beta.md)" in alpha

    msg = _git(tmp_repo, "log", "-1", "--format=%s")
    assert msg == "docs: assign plan numbers P0101-P0102"


def test_rename_unassigned_rewrites_dag_jsonl(docs_branch):
    """Proves the text-replace hits {"plan":924999901} in JSONL — placeholder
    is a literal integer in JSON, str.replace still finds it."""
    from onibus.merge import rename_unassigned as run

    tmp_repo, impl = docs_branch
    # Add a .claude/dag.jsonl with placeholder rows (writer-appended shape)
    claude = tmp_repo / ".claude"
    claude.mkdir(exist_ok=True)
    (claude / "dag.jsonl").write_text(
        '{"plan":100,"title":"real","deps":[],"status":"DONE"}\n'
        '{"plan":924999901,"title":"alpha","deps":[924999902],"status":"UNIMPL"}\n'
        '{"plan":924999902,"title":"beta","deps":[],"status":"UNIMPL"}\n'
    )
    _git(tmp_repo, "add", ".claude/dag.jsonl")
    _git(tmp_repo, "commit", "-m", "docs: add dag.jsonl", "--no-verify")

    report = run("docs-249999")
    assert report.mapping[0].assigned == 101
    jl = (claude / "dag.jsonl").read_text()
    # 9-digit placeholders gone
    assert "924999901" not in jl
    assert "924999902" not in jl
    # Rewritten as unpadded ints (JSON forbids leading zeros — .jsonl gets
    # str(assigned), .md gets :04d). Still valid JSON; parsed int is the same.
    assert '"plan":101' in jl
    assert '"deps":[102]' in jl
    assert '"plan":102' in jl
    # Row 100 untouched
    assert '"plan":100' in jl
    # JSONL is still parseable
    for line in jl.strip().splitlines():
        json.loads(line)


def test_rename_scans_main_not_worktree(docs_branch, tmp_path, monkeypatch):
    # The bug f32dccc shipped: _next_real scanned the worktree. If main received
    # P0105 after the branch forked (branch only sees plan-0100), buggy code
    # allocates 101; correct code scans main and allocates 106.
    from onibus import merge as rename_unassigned
    from onibus.merge import rename_unassigned as run

    tmp_repo, _ = docs_branch
    mains_docs = tmp_path / "mains-docs"
    mains_docs.mkdir()
    (mains_docs / "plan-0100-existing.md").touch()
    (mains_docs / "plan-0105-merged-after-fork.md").touch()
    monkeypatch.setattr(rename_unassigned, "DOCS_DIR", mains_docs)

    report = run("docs-249999")
    assert report.mapping[0].assigned == 106
    assert report.mapping[1].assigned == 107
    assert (
        _git(tmp_repo, "log", "-1", "--format=%s")
        == "docs: assign plan numbers P0106-P0107"
    )


def test_rename_noop_on_impl_branch(tmp_repo: Path, monkeypatch):
    from onibus.merge import rename_unassigned as run
    from onibus import merge as rename_unassigned

    impl = tmp_repo / ".claude" / "work"
    impl.mkdir(parents=True)
    (impl / "plan-0100-x.md").write_text("real\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "seed", "--no-verify")
    _git(tmp_repo, "checkout", "-b", "p100")
    # Impl branch must be ahead of INTEGRATION_BRANCH — rename_unassigned
    # now rejects already-merged branches (is-ancestor guard, P0325 T2).
    # Realistic anyway: an impl branch with no commits isn't an impl branch.
    (tmp_repo / "src.rs").write_text("impl work\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "feat(x): impl work", "--no-verify")
    monkeypatch.setattr(rename_unassigned, "_worktree_for", lambda _: tmp_repo)

    report = run("p100")
    assert report.mapping == []
    assert report.commit is None
    # No rename commit created — tip is still the impl commit.
    assert _git(tmp_repo, "log", "-1", "--format=%s") == "feat(x): impl work"


def test_rename_idempotent(docs_branch):
    from onibus.merge import rename_unassigned as run

    tmp_repo, _ = docs_branch
    first = run("docs-249999")
    assert len(first.mapping) == 2
    second = run("docs-249999")
    assert second.mapping == []
    assert second.commit is None


# ─── T-placeholder rewrite (lazy T-number assignment, P0401) ──────────────────


def _seed_batch_doc(work: Path, plan: str, max_t: int) -> Path:
    """A batch doc with T1..T<max_t> headers. Filename: plan-0<plan>-batch.md."""
    p = work / f"plan-{plan}-batch.md"
    body = f"# Plan {plan}\n\n## Tasks\n\n"
    for i in range(1, max_t + 1):
        body += f"### T{i} — `fix(x):` item {i}\n\ndesc {i}\n\n"
    p.write_text(body)
    return p


@pytest.fixture
def t_placeholder_branch(tmp_repo: Path, monkeypatch):
    """Two batch docs on TGT (P0304: T1-T3, P0311: T1-T2). docs-branch
    appends T-placeholders (runid 594354) with a cross-ref."""
    from onibus import merge as merge_mod

    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True)
    _seed_batch_doc(work, "0304", 3)  # TGT: P0304 has T1-T3
    _seed_batch_doc(work, "0311", 2)  # TGT: P0311 has T1-T2
    (work / "plan-0100-existing.md").write_text("# real\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "seed batch docs", "--no-verify")

    _git(tmp_repo, "checkout", "-b", "docs-594354")
    # Writer appends two T-placeholders to P0304 (with a cross-ref) and one
    # to P0311. Per-doc sequences both start at 01.
    p0304 = work / "plan-0304-batch.md"
    p0304.write_text(
        p0304.read_text()
        + "### T959435401 — `fix(x):` new item A\n\n"
        "Body A — see T959435402 below for sibling.\n\n"
        "### T959435402 — `fix(x):` new item B\n\n"
        "Body B — back-ref to T959435401.\n\n"
    )
    p0311 = work / "plan-0311-batch.md"
    p0311.write_text(
        p0311.read_text()
        + "### T959435401 — `fix(y):` new item C\n\n"
        "Body C. Refs pre-existing T2 (real number, not rewritten).\n\n"
    )
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: batch append", "--no-verify")

    monkeypatch.setattr(merge_mod, "_worktree_for", lambda _: tmp_repo)
    monkeypatch.setattr(merge_mod, "DOCS_DIR", work)
    return tmp_repo, work


def test_rename_unassigned_t_placeholders_per_doc(t_placeholder_branch):
    """Per-doc T-sequence: P0304 (max T3) → T4,T5; P0311 (max T2) → T3.
    Cross-refs rewritten in the same pass. Pre-existing T-refs untouched."""
    from onibus.merge import rename_unassigned as run

    tmp_repo, work = t_placeholder_branch
    report = run("docs-594354")
    assert report.commit is not None
    assert report.mapping == []  # no P-placeholders on this branch

    p0304_text = (work / "plan-0304-batch.md").read_text()
    # P0304 max-T on TGT was 3 → placeholders become T4, T5
    assert "### T4 —" in p0304_text
    assert "### T5 —" in p0304_text
    assert "T959435401" not in p0304_text
    assert "T959435402" not in p0304_text
    # Cross-ref body text rewritten: T959435402 → T5, T959435401 → T4
    assert "see T5 below" in p0304_text
    assert "back-ref to T4" in p0304_text

    p0311_text = (work / "plan-0311-batch.md").read_text()
    # P0311 max-T on TGT was 2 → placeholder becomes T3 (per-doc, NOT T6)
    assert "### T3 —" in p0311_text
    assert "T959435401" not in p0311_text
    # Pre-existing real T-ref ("Refs pre-existing T2") NOT rewritten
    assert "Refs pre-existing T2" in p0311_text

    msg = _git(tmp_repo, "log", "-1", "--format=%s")
    assert "T-number" in msg
    assert "batch doc" in msg


def test_rename_unassigned_t_placeholders_concurrent_writers(tmp_repo: Path, monkeypatch):
    """Two docs-branches append to the same batch doc with distinct runids.
    A merges first → its placeholders get T4,T5. B rebases, then merges →
    B's placeholder gets T6 (max-T on TGT is now 5 after A merged)."""
    from onibus import merge as merge_mod
    from onibus.merge import rename_unassigned as run
    from onibus import INTEGRATION_BRANCH

    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True)
    p0304 = _seed_batch_doc(work, "0304", 3)  # TGT: T1-T3
    (work / "plan-0100-existing.md").write_text("# real\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "seed", "--no-verify")

    monkeypatch.setattr(merge_mod, "DOCS_DIR", work)
    monkeypatch.setattr(merge_mod, "_worktree_for", lambda _: tmp_repo)

    # --- Branch A (runid 111111): append 2 placeholders, rename, ff-merge ---
    _git(tmp_repo, "checkout", "-b", "docs-111111")
    p0304.write_text(
        p0304.read_text()
        + "### T911111101 — `fix(x):` A-one\n\n"
        "### T911111102 — `fix(x):` A-two\n\n"
    )
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: A append", "--no-verify")
    run("docs-111111")
    text_a = p0304.read_text()
    assert "### T4 —" in text_a
    assert "### T5 —" in text_a
    assert "T911111101" not in text_a
    _git(tmp_repo, "checkout", INTEGRATION_BRANCH)
    _git(tmp_repo, "merge", "--ff-only", "docs-111111")

    # --- Branch B (runid 222222): concurrent with A, now behind TGT ---
    # In the real workflow B was created from the SAME base as A. After A
    # merged, B's rebase onto TGT conflicts (both appended at file-end).
    # The merger resolves via keep-both (appends are additive). We construct
    # that post-resolution state directly: TGT's content + B's placeholder.
    _git(tmp_repo, "checkout", "-b", "docs-222222")
    p0304.write_text(
        p0304.read_text()
        + "### T922222201 — `fix(x):` B-one\n\n"
    )
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: B append (rebased)", "--no-verify")
    run("docs-222222")
    text_b = p0304.read_text()
    # B's placeholder → T6 (_max_existing_t reads TGT which now shows max=5
    # after A's ff — THIS is the load-bearing assertion: had B computed
    # from its own base it would have gotten T4, colliding with A).
    assert "### T6 —" in text_b
    assert "T922222201" not in text_b
    # A's assignments still present (from TGT)
    assert "### T4 —" in text_b
    assert "### T5 —" in text_b


def test_rename_unassigned_batch_doc_p_crossref(tmp_repo: Path, monkeypatch):
    """P0304-T30: batch-target docs cross-reference P-placeholders ("see
    [P994957501](plan-994957501-...md)"). Before this fix, the P-rewrite
    pass only iterated the new plan-9ddddddNN-*.md files + dag.jsonl, so
    the batch-doc cross-ref stayed stale. 7/12 recent docs-merges needed
    coordinator manual-sed. Now _touched_batch_docs feeds into the
    P-rewrite `touched` set."""
    from onibus import merge as merge_mod
    from onibus.merge import rename_unassigned as run

    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True)
    _seed_batch_doc(work, "0304", 3)
    (work / "plan-0100-existing.md").write_text("# real\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "seed", "--no-verify")

    _git(tmp_repo, "checkout", "-b", "docs-594354")
    # Writer creates a new standalone plan (P-placeholder) AND appends
    # a cross-ref to it in the batch doc P0304.
    (work / "plan-959435401-newfeat.md").write_text(
        "# Plan 959435401\n\nStandalone. See P959435401 self-ref.\n"
    )
    p0304 = work / "plan-0304-batch.md"
    p0304.write_text(
        p0304.read_text()
        + "### T959435401 — `fix(x):` batch task\n\n"
        "Blocked on [P959435401](plan-959435401-newfeat.md) landing.\n\n"
    )
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: new plan + batch append", "--no-verify")

    monkeypatch.setattr(merge_mod, "_worktree_for", lambda _: tmp_repo)
    monkeypatch.setattr(merge_mod, "DOCS_DIR", work)

    report = run("docs-594354")
    assert len(report.mapping) == 1
    # _next_real scans DOCS_DIR: max(100, 304) + 1 = 305
    assert report.mapping[0].assigned == 305

    # The batch doc's cross-ref is rewritten (P0304-T30 fix)
    p0304_text = p0304.read_text()
    assert "959435401" not in p0304_text
    assert "[P0305](plan-0305-newfeat.md)" in p0304_text
    # AND the T-placeholder in the same doc got assigned (T1-T3 existed → T4)
    assert "### T4 —" in p0304_text
    # The new standalone plan doc was renamed + rewritten
    assert not (work / "plan-959435401-newfeat.md").exists()
    assert (work / "plan-0305-newfeat.md").exists()
    assert "959435401" not in (work / "plan-0305-newfeat.md").read_text()


def test_t_placeholder_re():
    from onibus.merge import _T_PLACEHOLDER_RE, _T_HEADER_RE

    # 9-digit with T prefix matches; captures the digits
    assert _T_PLACEHOLDER_RE.search("see T959435401 above").group(1) == "959435401"
    assert _T_PLACEHOLDER_RE.findall("T911111101 and T911111102") == [
        "911111101", "911111102",
    ]
    # 8-digit doesn't match (runid is 6, NN is 2 → 9 total required)
    assert _T_PLACEHOLDER_RE.search("T91111111 eight") is None
    # Real T-number (≤4 digits) not a placeholder
    assert _T_PLACEHOLDER_RE.search("T163") is None
    # Word boundary — T959435401x (no boundary) shouldn't match
    assert _T_PLACEHOLDER_RE.search("T959435401x") is None

    # Header regex caps at 4 digits — excludes placeholders by construction
    assert _T_HEADER_RE.findall("### T163 — title\n### T959435401 — ph\n") == ["163"]


# ─── dag-tick cadence-filter ───────────────────────────────────────────────────


def _mk_followup(**kw):
    defaults = {
        "severity": "trivial",
        "description": "x",
        "proposed_plan": "P-batch-trivial",
        "source_plan": "P0099",
        "origin": "reviewer",
        "timestamp": "t",
    }
    return Followup(**{**defaults, **kw})


def test_tick_flush_excludes_cadence_proposals(tmp_path: Path, monkeypatch):
    """Cadence-agent rows (origin in {consolidator,bughunter}) accumulate in
    the sink but don't count toward the >15 flush threshold and don't fire
    the P-new trigger. A consolidator P-new would otherwise insta-flush the
    sink the moment it lands — bypassing coordinator review."""
    from onibus import tick as dag_tick

    monkeypatch.setattr(dag_tick, "STATE_DIR", tmp_path)
    sink = tmp_path / "followups-pending.jsonl"

    # 3 reviewer rows (actionable) + 2 cadence rows = 5 total, 0 P-new actionable
    for f in [
        _mk_followup(severity="trivial", description="sort entries", origin="reviewer"),
        _mk_followup(
            severity="correctness", description="recurse refs", origin="reviewer"
        ),
        _mk_followup(severity="test-gap", description="spill path", origin="reviewer"),
        _mk_followup(
            severity="feature",
            description="CONSOLIDATION: extract trait across 170,132,127",
            proposed_plan="P-new",  # ← P-new but cadence: must NOT fire
            source_plan="consolidator",
            origin="consolidator",
        ),
        _mk_followup(
            severity="correctness",
            description="BUGHUNT: 7 unwraps in cas.rs",
            proposed_plan="P-new",  # ← same: cadence P-new, filtered
            source_plan="bughunter",
            origin="bughunter",
        ),
    ]:
        append_jsonl(sink, f)

    total, cadence, should_flush = dag_tick._followups_state()
    assert total == 5
    assert cadence == 2
    assert should_flush is False  # 3 actionable < 15, no actionable P-new

    # Now add a REAL reviewer P-new — flush fires
    append_jsonl(
        sink,
        _mk_followup(
            severity="correctness",
            description="query_missing doesn't recurse",
            proposed_plan="P-new",
            origin="reviewer",
        ),
    )
    total, cadence, should_flush = dag_tick._followups_state()
    assert total == 6
    assert cadence == 2
    assert should_flush is True  # reviewer P-new fires


def test_tick_flush_threshold_excludes_cadence_from_count(tmp_path: Path, monkeypatch):
    """>15 threshold counts actionable only. 20 cadence rows don't flush."""
    from onibus import tick as dag_tick

    monkeypatch.setattr(dag_tick, "STATE_DIR", tmp_path)
    sink = tmp_path / "followups-pending.jsonl"
    # 20 cadence rows, 10 actionable → 10 < 15, no flush
    for i in range(20):
        append_jsonl(
            sink,
            _mk_followup(
                description=f"CONSOLIDATION: pattern {i}",
                source_plan="consolidator",
                origin="consolidator",
            ),
        )
    for i in range(10):
        append_jsonl(
            sink, _mk_followup(description=f"reviewer finding {i}", origin="reviewer")
        )
    total, cadence, should_flush = dag_tick._followups_state()
    assert total == 30
    assert cadence == 20
    assert should_flush is False
    # 6 more actionable → 16 > 15, flushes
    for i in range(6):
        append_jsonl(sink, _mk_followup(description=f"more {i}", origin="reviewer"))
    _, _, should_flush = dag_tick._followups_state()
    assert should_flush is True


def test_cadence_filter_uses_origin_not_prefix(tmp_path: Path, monkeypatch):
    """The CONSOLIDATION:/BUGHUNT: description prefix is cosmetic. The filter
    matches on the typed `origin` field. A reviewer row with a CONSOLIDATION:
    prefix (accidental, or a reviewer flagging duplication) is still actionable;
    a cadence row with no prefix is still filtered. Proves CADENCE_PREFIXES
    was deleted (replaced by origin check)."""
    from onibus import tick as dag_tick

    # CADENCE_PREFIXES must be gone — origin is load-bearing now.
    assert not hasattr(dag_tick, "CADENCE_PREFIXES")

    monkeypatch.setattr(dag_tick, "STATE_DIR", tmp_path)
    sink = tmp_path / "followups-pending.jsonl"
    # Reviewer row with CONSOLIDATION: prefix — still actionable (P-new fires).
    append_jsonl(
        sink,
        _mk_followup(
            description="CONSOLIDATION: this prefix is cosmetic",
            proposed_plan="P-new",
            origin="reviewer",
        ),
    )
    # Cadence row WITHOUT prefix — still filtered (P-new does NOT fire).
    append_jsonl(
        sink,
        _mk_followup(
            description="no prefix here",
            proposed_plan="P-new",
            source_plan="consolidator",
            origin="consolidator",
        ),
    )
    total, cadence, should_flush = dag_tick._followups_state()
    assert total == 2
    assert cadence == 1  # only the origin=consolidator row
    assert should_flush is True  # reviewer P-new fires despite the prefix

    # Legacy row (origin=None, pre-this-change) counts as actionable.
    sink.write_text("")
    append_jsonl(sink, _mk_followup(description="legacy", proposed_plan="P-new"))
    _, cadence, should_flush = dag_tick._followups_state()
    assert cadence == 0
    assert should_flush is True  # None not in {consolidator,bughunter} → actionable


# ─── dag-stop snapshot ───────────────────────────────────────────────────────


def test_stop_snapshot_filters_running(tmp_path: Path, monkeypatch):
    from onibus import tick as dag_stop

    monkeypatch.setattr(dag_stop, "STATE_DIR", tmp_path)
    monkeypatch.setattr(dag_stop, "git", lambda *_: "abc1234")
    append_jsonl(
        tmp_path / "agents-running.jsonl",
        AgentRow(
            plan="P0183",
            role="impl",
            status="running",
            worktree="/x/p183",
            agent_id="xyz789",
        ),
    )
    append_jsonl(
        tmp_path / "agents-running.jsonl",
        AgentRow(plan="P0120", role="verify", status="done"),
    )
    append_jsonl(
        tmp_path / "merge-queue.jsonl",
        MergeQueueRow(
            plan="P0022", worktree="/x/p22", verdict="PASS", commit="deadbee"
        ),
    )
    append_jsonl(tmp_path / "followups-pending.jsonl", _mk_followup())
    append_jsonl(
        tmp_path / "followups-pending.jsonl",
        _mk_followup(severity="correctness", description="y", proposed_plan="P-new"),
    )

    snap = dag_stop.stop()
    assert snap.main_sha == "abc1234"
    assert len(snap.in_flight) == 1
    assert snap.in_flight[0].plan == "P0183"
    assert snap.in_flight[0].role == "impl"
    assert snap.in_flight[0].agent_id == "xyz789"  # hard-stop needs this
    assert len(snap.merge_queue) == 1
    assert snap.merge_queue[0].plan == "P0022"
    assert snap.followups_total == 2
    assert snap.followups_pnew == 1


# ─── worktree parsing ────────────────────────────────────────────────────────


def test_worktree_phase_num():
    assert Worktree(path=Path("/x"), branch="p142", head="abc").plan_num == 142
    assert Worktree(path=Path("/x"), branch="p22", head="abc").plan_num == 22
    assert Worktree(path=Path("/x"), branch="main", head="abc").plan_num is None
    assert Worktree(path=Path("/x"), branch=None, head="abc").plan_num is None
    # Reject non-pNNN shapes
    assert Worktree(path=Path("/x"), branch="p142x", head="abc").plan_num is None
    assert Worktree(path=Path("/x"), branch="xp142", head="abc").plan_num is None


# ─── behind_check phantom-amend detection ────────────────────────────────────


def test_behind_check_phantom_amend_detection(tmp_repo: Path):
    """phantom_amend fires when the worktree's base is the merger's
    orphaned pre-amend SHA.

    Scenario (merger step-7.5): integration branch gets a feature commit
    ff'd, then `git commit --amend --no-edit` to flip dag.jsonl. Any
    worktree that rebased onto the pre-amend SHA during the ff→amend
    window sees behind=1 with a collision on the feature files (NOT
    dag.jsonl — the collision is the ff'd commit's tree, which appears
    on both sides of the merge-base). `git rebase` auto-drops the
    patch-already-upstream commit; the phantom_amend bit makes that
    mechanical instead of a ~30s hand-diagnosis."""
    from onibus import INTEGRATION_BRANCH
    from onibus.git_ops import behind_check

    repo = tmp_repo
    # tmp_repo starts on INTEGRATION_BRANCH with an init commit.
    # Seed dag.jsonl so the amend has a target.
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"UNIMPL"}\n')
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "chore: seed dag", "--no-verify")

    # Feature commit — simulates a plan's ff'd work.
    (repo / "feature.txt").write_text("prev plan work\n")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: prev plan change", "--no-verify")
    pre_amend = _git(repo, "rev-parse", "HEAD")
    # The amend — flips dag status, rewrites HEAD SHA, keeps message.
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"DONE"}\n')
    _git(repo, "add", ".claude/dag.jsonl")
    _git(repo, "commit", "--amend", "--no-edit", "--no-verify")
    post_amend = _git(repo, "rev-parse", "HEAD")
    assert pre_amend != post_amend, "amend must rewrite SHA"
    assert _git(repo, "log", "-1", "--format=%s") == "feat: prev plan change"

    # Worktree branched from pre_amend (orphaned). Simulates an impl that
    # rebased onto sprint-1 during the ff→amend window.
    wt = repo.parent / "wt-p999"
    _git(repo, "worktree", "add", "-b", "p999", str(wt), pre_amend)
    (wt / "own-work.txt").write_text("p999 work\n")
    _git(wt, "add", "own-work.txt")
    _git(wt, "commit", "-m", "feat: p999 work", "--no-verify")

    bc = behind_check(wt)
    assert bc.behind == 1
    # Collision includes the ff'd commit's files — both sides have
    # feature.txt since both pre_amend and post_amend share that tree.
    # (This is WHY trivial_rebase reads false despite the rebase being
    # mechanically safe — phantom_amend is the signal that cuts through.)
    assert "feature.txt" in bc.file_collision
    assert bc.trivial_rebase is False
    assert bc.phantom_amend is True, (
        f"expected phantom_amend=True: pre_amend={pre_amend[:8]} in our "
        f"ancestry differs from {INTEGRATION_BRANCH}@{post_amend[:8]} only "
        f"in dag.jsonl; collision={bc.file_collision}"
    )

    # Negative A: worktree from post_amend → behind=0, no phantom.
    wt2 = repo.parent / "wt-p998"
    _git(repo, "worktree", "add", "-b", "p998", str(wt2), post_amend)
    bc2 = behind_check(wt2)
    assert bc2.behind == 0
    assert bc2.phantom_amend is False

    # Negative B: forked from BEFORE the feature commit (did NOT rebase
    # onto pre-amend). behind=1, collision may include dag.jsonl if the
    # worktree also touches it — but there's no pre-amend SHA in its
    # ancestry, so phantom_amend must NOT fire. Guards against a naive
    # `collision ⊆ {dag.jsonl}` check false-positiving here.
    seed_sha = _git(repo, "rev-parse", f"{INTEGRATION_BRANCH}~1")
    wt3 = repo.parent / "wt-p997"
    _git(repo, "worktree", "add", "-b", "p997", str(wt3), seed_sha)
    (wt3 / ".claude" / "dag.jsonl").write_text(
        '{"plan":1,"status":"UNIMPL"}\n{"plan":2,"status":"UNIMPL"}\n'
    )
    _git(wt3, "add", ".claude/dag.jsonl")
    _git(wt3, "commit", "-m", "docs: add plan-2 row", "--no-verify")
    bc3 = behind_check(wt3)
    assert bc3.behind == 1
    assert ".claude/dag.jsonl" in bc3.file_collision
    # The oldest ours-only commit is our OWN docs commit (not pre_amend);
    # diff vs sprint-1 tip = {feature.txt, dag.jsonl} ⊄ amend-files.
    assert bc3.phantom_amend is False, (
        "forked-from-earlier should NOT be phantom — no pre-amend SHA "
        "in ancestry; a naive collision⊆{dag.jsonl} check would have "
        "false-positived here"
    )


def test_behind_check_phantom_amend_stacked(tmp_repo: Path):
    """phantom_amend fires for behind>=2 (stacked amends).

    Scenario: worktree rebases onto sprint-1 at the pre-amend SHA of
    feature A. Merger amends A → A'. A second merger ff's feature B on
    top of A', then amends B → B'. Worktree now sees behind=2: both A'
    and B' are new reachable-from-sprint-1. P0346's behind==1 gate would
    miss this; the generalized check finds A' (message-match for the
    pre-amend candidate) within the last-N commits and diffs against it.
    """
    from onibus import INTEGRATION_BRANCH
    from onibus.git_ops import behind_check

    repo = tmp_repo
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"UNIMPL"}\n')
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "chore: seed dag", "--no-verify")

    # Merge 1: feature A → ff → amend (dag flip 1).
    (repo / "a.txt").write_text("feature a\n")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: plan-a change", "--no-verify")
    pre_amend_a = _git(repo, "rev-parse", "HEAD")
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"DONE"}\n')
    _git(repo, "add", ".claude/dag.jsonl")
    _git(repo, "commit", "--amend", "--no-edit", "--no-verify")
    post_amend_a = _git(repo, "rev-parse", "HEAD")
    assert pre_amend_a != post_amend_a

    # Merge 2: feature B on top of A' → ff → amend (dag flip 2).
    (repo / "b.txt").write_text("feature b\n")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: plan-b change", "--no-verify")
    (repo / ".claude" / "dag.jsonl").write_text(
        '{"plan":1,"status":"DONE"}\n{"plan":2,"status":"DONE"}\n'
    )
    _git(repo, "add", ".claude/dag.jsonl")
    _git(repo, "commit", "--amend", "--no-edit", "--no-verify")

    # Worktree: branched from pre_amend_a (before FIRST amend). Missed
    # both merge windows → behind=2.
    wt = repo.parent / "wt-p996"
    _git(repo, "worktree", "add", "-b", "p996", str(wt), pre_amend_a)
    (wt / "own-work.txt").write_text("p996 work\n")
    _git(wt, "add", "own-work.txt")
    _git(wt, "commit", "-m", "feat: p996 work", "--no-verify")

    bc = behind_check(wt)
    assert bc.behind == 2, f"expected behind=2 (stacked), got {bc.behind}"
    # Collision includes the ff'd commit's files (both pre_amend_a and
    # post_amend_a share a.txt in their trees) — phantom_amend cuts
    # through the false collision signal.
    assert "a.txt" in bc.file_collision
    assert bc.trivial_rebase is False
    assert bc.phantom_amend is True, (
        f"stacked amend: expected phantom_amend=True (pre_amend_a "
        f"{pre_amend_a[:8]} should message-match A' in last-2 of "
        f"sprint-1), got {bc!r}"
    )

    # Verify the cure: `git rebase $TGT` auto-drops the orphaned commit.
    # Post-rebase, behind=0 and own-work.txt is preserved.
    _git(wt, "rebase", INTEGRATION_BRANCH)
    bc_post = behind_check(wt)
    assert bc_post.behind == 0
    assert (wt / "own-work.txt").read_text() == "p996 work\n"
    assert (wt / "b.txt").exists()  # picked up from sprint-1


def test_behind_check_not_phantom_real_two_behind(tmp_repo: Path):
    """Negative: behind=2 with REAL commits (not amends) → phantom=False.

    Distinguishes stacked-amend (message match within last-N + dag-only
    diff) from genuine behind-by-2 (no message match — integration branch
    has different commits entirely).
    """
    from onibus.git_ops import behind_check

    repo = tmp_repo
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1}\n')
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "chore: seed dag", "--no-verify")

    (repo / "a.txt").write_text("a")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: a", "--no-verify")
    base = _git(repo, "rev-parse", "HEAD")

    # Two real commits on sprint-1 past `base` — NOT amends.
    (repo / "b.txt").write_text("b")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: b", "--no-verify")
    (repo / "c.txt").write_text("c")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: c", "--no-verify")

    # Worktree forked from `base`, now behind by 2 real commits.
    wt = repo.parent / "wt-p995"
    _git(repo, "worktree", "add", "-b", "p995", str(wt), base)
    (wt / ".claude" / "dag.jsonl").write_text('{"plan":1}\n{"plan":9}\n')
    _git(wt, "add", ".claude/dag.jsonl")
    _git(wt, "commit", "-m", "docs: plan-9", "--no-verify")

    bc = behind_check(wt)
    assert bc.behind == 2
    # `base` is reachable from sprint-1, so ours_only = {"docs: plan-9"}
    # only. msg_pre = "docs: plan-9"; last-2 sprint-1 msgs = {"feat: c",
    # "feat: b"} — no message match → phantom stays False.
    assert bc.phantom_amend is False, (
        f"real behind-by-2: expected phantom_amend=False (no message "
        f"match for 'docs: plan-9' in last-2 sprint-1 commits), "
        f"got {bc!r}"
    )


# ─── atomicity_check (synthetic fixture; decoupled from live branches) ───────


def test_atomicity_check_catches_chore_src(tmp_repo_patched: tuple[Path, Path]):
    """Synthetic reproduction of the motivating case: a chore:-labeled
    commit touching rio-*/src/*.rs must abort with chore-touches-src.

    Decoupled from any live branch — the rix original tested against a
    real p142 branch via `@pytest.mark.skipif(git rev-parse p142 fails)`,
    which silently skipped once p142 was deleted and made the test a
    self-hostage (splitting the very commit it asserts on breaks the grep).
    Runs unconditionally."""
    from onibus.merge import atomicity_check as run
    tmp_repo, _ = tmp_repo_patched

    # Seed a plan doc on main so t_count=3. Commit it to main first so
    # find_plan_doc(999) resolves AND the doc commit isn't in main..p999.
    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True)
    (work / "plan-0999-synthetic.md").write_text("### T1 — a\n### T2 — b\n### T3 — c\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: seed plan", "--no-verify")

    # Branch off, create 35 src files, commit as chore:.
    _git(tmp_repo, "checkout", "-b", "p999")
    src = tmp_repo / "rio-fake" / "src"
    src.mkdir(parents=True)
    for i in range(35):
        (src / f"f{i}.rs").write_text("// x\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "chore: trivial hardening batch", "--no-verify")

    # Second commit so c_count=2 → mega stays False despite t_count>=3.
    # Validates the original test's mega-doesn't-fire-alongside-chore path.
    (src / "f0.rs").write_text("// y\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "fix(fake): tweak", "--no-verify")

    verdict = run("p999")
    assert verdict.abort_reason == "chore-touches-src"
    assert len(verdict.chore_violations) == 1
    v = verdict.chore_violations[0]
    assert v.subject == "chore: trivial hardening batch"
    assert len(v.src_files) == 35
    assert all(f.startswith("rio-fake/src/") for f in v.src_files)
    # mega_commit does NOT fire: t_count>=3 but c_count>1.
    assert not verdict.mega_commit
    assert verdict.t_count == 3
    assert verdict.c_count == 2


def test_atomicity_check_clean_branch_passes(tmp_repo_patched: tuple[Path, Path]):
    """Negative case: fix:-labeled commit touching src plus chore:-labeled
    commit touching only Cargo.lock → abort_reason is None. Validates both
    gates stay open for the well-behaved shape."""
    from onibus.merge import atomicity_check as run
    tmp_repo, _ = tmp_repo_patched

    # Non-pNNN branch name → _plan_num_from_branch returns None → t_count=0
    # → mega can't trip regardless of c_count. Keeps this test focused on
    # the chore-src gate alone.
    _git(tmp_repo, "checkout", "-b", "feature-clean")

    # fix: commit touching src — allowed.
    src = tmp_repo / "rio-fake" / "src"
    src.mkdir(parents=True)
    (src / "lib.rs").write_text("// fix\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "fix(fake): real fix", "--no-verify")

    # chore: commit touching only Cargo.lock — allowed (not rio-*/src/*.rs).
    (tmp_repo / "Cargo.lock").write_text("# lock\n")
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "chore: bump deps", "--no-verify")

    verdict = run("feature-clean")
    assert verdict.abort_reason is None
    assert verdict.chore_violations == []
    assert not verdict.mega_commit
    assert verdict.t_count == 0
    assert verdict.c_count == 2


# ─── schema emission ─────────────────────────────────────────────────────────


def test_schemas_are_valid_json():
    """Every output model has a valid JSON Schema."""
    from onibus.models import AtomicityVerdict, CollisionReport, StopSnapshot, TickReport, RenameReport, BuildReport
    # (RenameReport, BuildReport already imported above via onibus.models)

    for cls in (
        AtomicityVerdict,
        CollisionReport,
        TickReport,
        StopSnapshot,
        RenameReport,
        BuildReport,
        MergerReport,
    ):
        schema = cls.model_json_schema()
        # Round-trip through JSON to confirm it's serializable
        json.loads(json.dumps(schema))
        # Has the pydantic-standard structure
        assert "properties" in schema
        assert "title" in schema


# ─── CLI subcommand coverage + real-data smoke ────────────────────────────────

_REAL_LIB = Path(__file__).parent
_REAL_REPO = _REAL_LIB.parents[1]


def _copy_harness(lib: Path) -> None:
    """Copy state.py + _lib.py shims AND the onibus package into a tmp lib/.
    The shims do `sys.path.insert(0, __file__.parent)` and `from onibus import ...`,
    so onibus/ must be a sibling."""
    lib.mkdir(parents=True, exist_ok=True)
    shutil.copytree(
        _REAL_LIB / "onibus", lib / "onibus",
        ignore=shutil.ignore_patterns("__pycache__"),
    )
    bindir = lib.parent / "bin"
    bindir.mkdir(exist_ok=True)
    shutil.copy(_REAL_LIB.parents[0] / "bin" / "onibus", bindir / "onibus")
    (bindir / "onibus").chmod(0o755)


_REAL_BIN = _REAL_LIB.parents[0] / "bin" / "onibus"  # .claude/bin/onibus


def _onibus_cli(*args: str, cwd: Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(_REAL_BIN), *args],
        cwd=cwd or _REAL_REPO,
        capture_output=True,
        text=True,
    )


_REAL_DAG = _REAL_REPO / ".claude" / "dag.jsonl"
_no_dag = pytest.mark.skipif(
    not _REAL_DAG.exists() or _REAL_DAG.stat().st_size == 0,
    reason=".claude/dag.jsonl empty or not in nix sandbox src fileset (pre-backfill)",
)


@pytest.mark.skip(
    reason="rio-adapt: row count (194) is rix-specific; update once rio dag.jsonl stabilizes"
)
@_no_dag
def test_real_dag_jsonl_parses():
    """Real-data smoke test — the actual .claude/dag.jsonl on disk must
    parse against PlanRow. Catches schema drift the fixture tests miss.
    Row count is pinned; bump when plans are added."""
    rows = [
        PlanRow.model_validate_json(ln)
        for ln in _REAL_DAG.read_text().splitlines()
        if ln.strip() and not ln.startswith("#")
    ]
    assert len(rows) == 194
    # Sorted by plan number (dag-set-status rewrites sorted).
    assert [r.plan for r in rows] == sorted(r.plan for r in rows)


@_no_dag
def test_dag_deps_cli():
    """dag-deps subcommand emits JSON with `plan` key (not `phase`)."""
    r = _onibus_cli("dag", "deps", "1")
    assert r.returncode == 0, r.stderr
    out = json.loads(r.stdout)
    assert "plan" in out
    assert "phase" not in out
    assert "status" in out
    assert "deps" in out
    assert "all_deps_done" in out


def test_collision_index_live_compute(tmp_repo: Path, monkeypatch):
    """CollisionIndex.load() scans plan docs live — 2 docs sharing a file →
    one hot row with both plan numbers. No jsonl; no staleness."""
    from onibus.collisions import CollisionIndex
    import onibus.collisions
    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True)
    (work / "plan-0001-a.md").write_text(
        "## Files\n\n```json files\n"
        '[{"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY"}]\n'
        "```\n"
    )
    (work / "plan-0002-b.md").write_text(
        "## Files\n\n```json files\n"
        '[{"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY"},'
        ' {"path": "rio-store/src/manifest.rs", "action": "MODIFY"}]\n'
        "```\n"
    )
    monkeypatch.setattr(onibus.collisions, "WORK_DIR", work)
    cx = CollisionIndex.load()
    hot = cx.hot()
    # assignment.rs is shared (count=2), manifest.rs is singleton (filtered out).
    assert len(hot) == 1
    assert hot[0].path == "rio-scheduler/src/assignment.rs"
    assert hot[0].plans == [1, 2]
    assert hot[0].count == 2
    # Bidirectional index works.
    assert cx.files_of(1) == frozenset({"rio-scheduler/src/assignment.rs"})
    assert cx.check(1, {2}) == ["rio-scheduler/src/assignment.rs"]
    assert cx.check(1, {99}) == []


def test_dag_markers_cli(tmp_repo: Path):
    """dag-markers subcommand: joins UNIMPL plan ❤ Tracey refs with piped
    tracey-uncovered. Surfaces planning gaps (uncovered+unclaimed)."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True)
    # Plan 1 claims two markers; plan 2 claims one; plan 3 is DONE (excluded)
    (work / "plan-0001-a.md").write_text(
        "## Tracey\n\n"
        "- `r[sched.actor.dispatch]` — T1 implements\n"
        "- `r[gw.rate.per-tenant]` — T2\n"
        "See also docs/src/components/worker.md (NOT a marker — file path)\n"
    )
    (work / "plan-0002-b.md").write_text("`r[store.gc.mark-sweep]` here\n")
    (work / "plan-0003-done.md").write_text("`r[obs.span.context]`\n")
    # dag.jsonl: plans 1+2 UNIMPL, plan 3 DONE
    dag = tmp_repo / ".claude" / "dag.jsonl"
    dag.write_text(
        '{"plan":1,"title":"a","status":"UNIMPL"}\n'
        '{"plan":2,"title":"b","status":"UNIMPL"}\n'
        '{"plan":3,"title":"c","status":"DONE"}\n'
    )
    # Pipe simulated tracey output: one claimed, one unclaimed, one non-marker
    tracey_out = (
        "sched.actor.dispatch\n"     # claimed by plan 1
        "sec.jwt.rotation\n"         # UNCLAIMED — planning gap
        "obs.span.context\n"         # claimed only by DONE plan 3 — also a gap
        "not.a.real.domain.prefix\n" # filtered — 'not' isn't a domain
    )
    r = subprocess.run(
        [".claude/bin/onibus", "dag", "markers"],
        input=tracey_out, capture_output=True, text=True, cwd=tmp_repo,
    )
    assert r.returncode == 0, r.stderr
    out = json.loads(r.stdout)
    # Claim map: only UNIMPL plans' markers, no file-path false positives
    assert out["_summary"]["markers_claimed"] == 3
    assert out["claimed_uncovered"] == {"sched.actor.dispatch": [1]}
    # Gaps: sec.jwt.rotation (unclaimed) + obs.span.context (DONE plan only)
    assert set(out["unclaimed_uncovered"]) == {"sec.jwt.rotation", "obs.span.context"}
    # claimed_covered: markers in plans but NOT in tracey-uncovered
    assert set(out["claimed_covered"]) == {"gw.rate.per-tenant", "store.gc.mark-sweep"}


def test_followup_origin_cli_parse(tmp_repo: Path):
    """followup subcommand parses the positional into origin + discovered_from:
    P<N>            → discovered_from=N, origin="reviewer" (inferred)
    FollowupOrigin  → discovered_from=None, origin=<that>
    anything else   → error (typed boundary — no free-text)"""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    (tmp_repo / ".claude" / "state").mkdir(parents=True)
    sink = tmp_repo / ".claude" / "state" / "followups-pending.jsonl"

    def _run(source: str, check: bool = True) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                ".claude/bin/onibus", "state", "followup", source,
                '{"severity":"trivial","description":"x","proposed_plan":"P-batch-trivial"}',
            ],
            cwd=tmp_repo,
            check=check,
            capture_output=True,
            text=True,
        )

    def _last() -> Followup:
        return Followup.model_validate_json(sink.read_text().splitlines()[-1])

    # P<N> → discovered_from=N, origin=reviewer (inferred — reviewers pass P-nums).
    _run("P0109")
    f = _last()
    assert f.discovered_from == 109
    assert f.origin == "reviewer"
    assert f.source_plan == "P0109"
    _run("P109")
    assert _last().discovered_from == 109
    assert _last().origin == "reviewer"

    # FollowupOrigin values → origin=<that>, discovered_from=None.
    for o in ("consolidator", "bughunter", "coverage", "inline", "coordinator"):
        _run(o)
        f = _last()
        assert f.origin == o
        assert f.discovered_from is None
        assert f.source_plan == o

    # Free-text positionals now ERROR — typed boundary.
    for bad in ("tooling", "p109", "p142-cov", "Consolidator", "reviewer-foo"):
        r = _run(bad, check=False)
        assert r.returncode == 2, f"{bad!r} should have been rejected"
        assert "followup positional must be" in r.stderr


def test_skill_subcommands_cli(tmp_repo: Path):
    """Smoke the 6 subcommands extracted from inline `python3 -c` blocks.
    Each replaces fragile skill-embedded quote-escaping with a tested CLI."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    state = tmp_repo / ".claude" / "state"
    state.mkdir(parents=True)
    (tmp_repo / ".claude" / "dag.jsonl").write_text(
        '{"plan":1,"title":"batch-trivial-hardening","status":"UNIMPL"}\n'
        '{"plan":2,"title":"feat","status":"DONE"}\n'
    )
    # collisions-top now computes live from plan docs, not jsonl.
    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True, exist_ok=True)
    for n in (1, 2, 3):
        (work / f"plan-{n:04d}-x.md").write_text(
            '```json files\n[{"path":"rio-gateway/src/opcodes.rs"}]\n```\n'
        )
    (work / "plan-0004-y.md").write_text(
        '```json files\n[{"path":"rio-store/src/gc.rs"}]\n```\n'
    )
    (state / "agents-running.jsonl").write_text(
        '{"plan":"P0001","role":"impl","status":"done","note":"x"}\n'
    )
    (state / "merge-queue.jsonl").write_text(
        '{"plan":"P0001","worktree":"/tmp/p1","verdict":"PASS","commit":"abc"}\n'
    )

    def _run(*args: str) -> str:
        return subprocess.run(
            [".claude/bin/onibus", *args],
            cwd=tmp_repo, capture_output=True, text=True, check=True,
        ).stdout

    # agent-lookup: finds matching plan+role
    out = _run("state", "agent-lookup", "P0001", "impl")
    assert json.loads(out)["plan"] == "P0001"
    # agent-lookup: no match → empty
    assert _run("state", "agent-lookup", "P9999", "impl") == ""

    # merge-queue-gates: one row, gate=null → clear=true
    out = _run("merge", "queue-gates")
    row = json.loads(out.strip())
    assert row["plan"] == "P0001" and row["clear"] is True

    # followups-render --inline: validates + renders
    out = _run("state", "followups-render", "--inline",
               '[{"severity":"trivial","description":"x","proposed_plan":"P-batch-trivial"}]')
    assert "| trivial | x |" in out
    # followups-render --inline: invalid severity → ValidationError
    r = subprocess.run(
        [".claude/bin/onibus", "state", "followups-render", "--inline",
         '[{"severity":"bug","description":"x","proposed_plan":"P-new"}]'],
        cwd=tmp_repo, capture_output=True, text=True,
    )
    assert r.returncode != 0 and "severity" in r.stderr.lower()

    # open-batches: finds UNIMPL batch, skips DONE non-batch
    out = _run("state", "open-batches")
    assert "P0001" in out and "batch-trivial" in out
    assert "P0002" not in out

    # collisions top: sorted by count desc, limit works (live-computed from plan docs)
    out = _run("collisions", "top", "1")
    assert "opcodes.rs" in out and "gc.rs" not in out


def test_warn_cwd_elsewhere_fires(tmp_repo: Path, tmp_path_factory):
    """Positive case for _warn_if_cwd_elsewhere: invoke from a cwd OUTSIDE
    the REPO_ROOT that state.py resolves (parents[2] of __file__). The
    existing crud test checks the negative (cwd inside → silent)."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    (tmp_repo / ".claude" / "known-flakes.jsonl").write_text("# header\n")
    # tmp_repo IS tmp_path (fixture returns it) — need a sibling dir OUTSIDE.
    elsewhere = tmp_path_factory.mktemp("elsewhere")
    # Absolute invocation from a cwd outside REPO_ROOT → warning fires.
    r = subprocess.run(
        [
            str(tmp_repo / ".claude" / "bin" / "onibus"),
            "flake", "add",
            KnownFlake(
                test="t",
                symptom="s",
                root_cause="rc",
                fix_owner="P0999",
                fix_description="d",
                retry="Once",
            ).model_dump_json(),
        ],
        cwd=elsewhere,
        capture_output=True,
        text=True,
    )
    assert r.returncode == 0, r.stderr
    assert "warning:" in r.stderr
    assert "REPO_ROOT" in r.stderr
