# Plan 0319: merge-shas.jsonl records PRE-amend SHAs — every row dangles

**Bughunter mc28 finding.** [`rio-impl-merger.md:125`](../../.claude/agents/rio-impl-merger.md) calls `onibus merge count-bump` (records `HEAD` to `merge-shas.jsonl`), **then** `:133` amends the dag-flip into `HEAD` — invalidating what was just recorded. Both mc=27 ([`e94632ba`](https://github.com/search?q=e94632ba&type=commits)) and mc=28 ([`a161b4f4`](https://github.com/search?q=a161b4f4&type=commits)) are **reflog-only**, zero refs contain them:

```
$ git reflog sprint-1
196fdc5b commit (amend): test(gateway): reword inline comments…   ← mc=28 SHOULD be this
a161b4f4 merge p305: Fast-forward                                  ← mc=28 IS this (dangling)
ad080ee1 commit (amend): fix(harness): _cadence_range…             ← mc=27 SHOULD be this
e94632ba merge p306: Fast-forward                                  ← mc=27 IS this (dangling)

$ git for-each-ref --contains e94632ba   → empty
$ git for-each-ref --contains ad080ee1   → docs-926870, p207, p247, …
```

**Three failure modes:**

1. **gc-fragile.** `_cadence_range()` at [`merge.py:188`](../../.claude/lib/onibus/merge.py) returns `dangling..dangling`. `git diff` works while reflog keeps them alive (default `gc.reflogExpireUnreachable=30d`), then `fatal: bad object`.
2. **Window too wide by 1.** `dangling_A..dangling_B` includes the LIVE twin of A — it's A's sibling, not A's ancestor. Range is one dag.jsonl-only commit too wide. Cadence agents see a spurious dag-flip diff in every window.
3. **CadenceReport stale.** [`rio-impl-merger.md:129`](../../.claude/agents/rio-impl-merger.md) `onibus merge cadence` also computes ranges BEFORE the amend — the report handed to coordinator has `end=pre-amend-sha`.

**Test gap:** [`test_onibus_dag.py:595`](../../.claude/lib/test_onibus_dag.py) `test_count_bump_records_merge_sha` commits once, bumps once, asserts SHA matches. No amend step. Passes against the broken ordering because there's nothing to amend.

The irony: [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) created `merge-shas.jsonl` specifically because `_cadence_range` was counting commits instead of merges ([`merge.py:126-128`](../../.claude/lib/onibus/merge.py): "bughunter audited a rio-*/src diff of literally zero lines"). The mc→SHA map fixes commit-counting but the SHAs themselves are wrong.

## Tasks

### T1 — `fix(agents):` reorder §7.5 — amend FIRST, then count-bump+cadence

MODIFY [`.claude/agents/rio-impl-merger.md`](../../.claude/agents/rio-impl-merger.md) at `:124-134`.

Current order (count-bump at `:125`, amend at `:133`):

```bash
.claude/bin/onibus merge count-bump                   # ← records PRE-amend tip
N=<plan-number-without-P-prefix>
.claude/bin/onibus dag set-status $N DONE
.claude/bin/onibus dag unblocked-by $N
.claude/bin/onibus merge cadence                      # ← computes ranges from pre-amend SHAs
.claude/bin/onibus merge queue-consume "P$N"
.claude/bin/onibus dag render
git add .claude/dag.jsonl
git commit --amend --no-edit                          # ← invalidates the recorded SHA
```

Reorder — dag-flip+amend FIRST (so `HEAD` is stable), count-bump+cadence AFTER:

```bash
N=<plan-number-without-P-prefix>   # e.g. 134 for p134
.claude/bin/onibus dag set-status $N DONE
.claude/bin/onibus dag render
git add .claude/dag.jsonl
git commit --amend --no-edit
# HEAD is now stable. Record it.
.claude/bin/onibus merge count-bump              # ← records POST-amend tip
.claude/bin/onibus dag unblocked-by $N           # pure read, order-irrelevant
.claude/bin/onibus merge cadence                 # ← ranges from stable SHAs
.claude/bin/onibus merge queue-consume "P$N"     # gitignored state, order-irrelevant
```

Update the prose at `:138` ("Amend, not a fresh commit") — add one sentence: "Amend **before** `count-bump` — otherwise the recorded SHA dangles post-amend (P0319)." The `dag_delta_commit` note at `:138` ("same as `hash`") becomes **more** correct — the recorded SHA now IS the merged-to SHA.

### T2 — `fix(harness):` append corrected mc=27,28 rows

The live `merge-shas.jsonl` (gitignored, `.claude/state/`) has:

```json
{"mc":27,"sha":"e94632ba94a06efe9b56fd63bcd4420ce25c8aaf","ts":"2026-03-19T13:14:19.642255+00:00"}
{"mc":28,"sha":"a161b4f483121fcc656a3ec6853eadd4fb79846c","ts":"2026-03-19T13:22:43.951725+00:00"}
```

Both dangling. [`merge.py:178-185`](../../.claude/lib/onibus/merge.py) is last-row-per-mc-wins — append corrected rows; old ones shadow. From the main worktree (where the state file lives):

```bash
# Post-amend SHAs from reflog verification:
#   mc=27: e94632ba → ad080ee1 (P0306 merge)
#   mc=28: a161b4f4 → 196fdc5b (P0305 merge)
python3 -c '
import json
from datetime import datetime, timezone
now = datetime.now(timezone.utc).isoformat()
with open(".claude/state/merge-shas.jsonl", "a") as f:
    f.write(json.dumps({"mc": 27, "sha": "ad080ee1", "ts": now, "corrected_by": "P0319"}) + "\n")
    f.write(json.dumps({"mc": 28, "sha": "196fdc5b", "ts": now, "corrected_by": "P0319"}) + "\n")
'
```

**Use full SHAs** — `git rev-parse ad080ee1` and `git rev-parse 196fdc5b` at dispatch to get the full 40-char hashes (the existing rows are full; short SHAs in `_cadence_range`'s `A..B` work but full is what `count_bump` writes at [`merge.py:142`](../../.claude/lib/onibus/merge.py)).

**NOT committed** — gitignored state. Do this from the main worktree as a side-effect during impl, or coordinator does it manually. The `corrected_by` extra key is tolerated (json.loads ignores unknown keys into a dict; `_cadence_range` at `:184-185` only reads `mc` and `sha`).

### T3 — `test(harness):` amend-post-bump regression test

MODIFY [`.claude/lib/test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) — new test after `:615` (after `test_count_bump_records_merge_sha`).

The existing test commits once, bumps once — no amend. Add a test that **mimics the merger flow**: commit → bump → amend → assert the recorded SHA **dangles** (this is the REGRESSION-CATCH test, it should FAIL against current merger.md ordering; it's a direct-call to `count_bump`, so it always passes against `merge.py` — the ordering bug is in the AGENT, not the Python). The test documents the protocol.

```python
def test_count_bump_after_amend_records_stable_sha(tmp_repo: Path, monkeypatch):
    """P0319: merger flow is ff-merge → dag-flip amend → count-bump.
    count-bump MUST come AFTER the amend or the recorded SHA dangles
    (amend rewrites HEAD; the pre-amend commit is reflog-only).

    This test does NOT catch the merger.md ordering bug directly — it
    exercises merge.py's count_bump, which is correct. The test DOCUMENTS
    the correct flow + proves the post-amend SHA is git-ref-reachable.
    The actual bug was in .claude/agents/rio-impl-merger.md:125-133 call
    order. Reflog proof: sprint-1@{3}=e94632ba (mc=27 recorded, dangling)
    vs sprint-1@{2}=ad080ee1 (post-amend, live).

    For-each-ref --contains is the check: it returns empty for
    reflog-only commits, non-empty for ref-reachable ones."""
    import json as _json
    import onibus.merge
    from onibus import INTEGRATION_BRANCH
    state = tmp_repo / ".claude" / "state"
    state.mkdir(parents=True, exist_ok=True)
    monkeypatch.chdir(tmp_repo)
    monkeypatch.setattr(onibus.merge, "STATE_DIR", state)
    monkeypatch.setattr(onibus.merge, "INTEGRATION_BRANCH", INTEGRATION_BRANCH)

    # Simulate merger flow: ff-merge lands (initial commit), then dag-flip amend.
    # The amend is what the merger does at §7.5 :133 — folds dag.jsonl into HEAD.
    pre_amend = _git(tmp_repo, "rev-parse", "HEAD")
    _git(tmp_repo, "commit", "--amend", "-m", "ff-merge + dag-flip (amended)",
         "--allow-empty")
    post_amend = _git(tmp_repo, "rev-parse", "HEAD")
    assert pre_amend != post_amend, "amend must rewrite HEAD"

    # Pre-amend SHA is NOT reachable from any ref (reflog-only).
    # This is what makes recording it a bug — git diff pre..X fails after gc.
    refs_pre = _git(tmp_repo, "for-each-ref", "--contains", pre_amend)
    assert refs_pre == "", \
        f"pre-amend {pre_amend[:8]} should be reflog-only, found in: {refs_pre}"

    # CORRECT order: bump AFTER amend. Recorded SHA is post_amend → live.
    onibus.merge.count_bump()
    rows = [_json.loads(line)
            for line in (state / "merge-shas.jsonl").read_text().splitlines()]
    assert rows[-1]["sha"] == post_amend, \
        "count_bump after amend must record the stable post-amend SHA"

    # Post-amend SHA IS reachable (integration branch points at it).
    refs_post = _git(tmp_repo, "for-each-ref", "--contains", post_amend)
    assert INTEGRATION_BRANCH in refs_post, \
        f"post-amend {post_amend[:8]} must be ref-reachable"
```

## Exit criteria

- `/nbr .#ci` green (harness tests pass)
- `grep -B2 'merge count-bump' .claude/agents/rio-impl-merger.md | grep -q 'amend'` → match (T1: amend precedes count-bump in the §7.5 bash block)
- `grep -A1 'dag set-status' .claude/agents/rio-impl-merger.md | head -2 | grep -qv 'cadence\|count-bump'` → match (dag-flip comes before count-bump/cadence)
- `nix develop -c pytest .claude/lib/test_onibus_dag.py::test_count_bump_after_amend_records_stable_sha -v` → PASS
- `tail -2 .claude/state/merge-shas.jsonl | jq -r .sha | while read s; do git for-each-ref --contains $s | head -1; done` → both non-empty (T2: corrected rows point at live SHAs — manual check from main worktree post-T2)
- `grep 'P0319\|before.*count-bump\|dangles post-amend' .claude/agents/rio-impl-merger.md` → ≥1 hit (T1: prose note present)

## Tracey

No markers. Harness/agent-doc ordering fix — no spec behavior.

## Files

```json files
[
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T1: reorder §7.5 bash block :124-134 — amend BEFORE count-bump+cadence; add prose note at :138"},
  {"path": ".claude/lib/test_onibus_dag.py", "action": "MODIFY", "note": "T3: new test_count_bump_after_amend_records_stable_sha after :615"}
]
```

**Gitignored state (NOT in Files fence):** `.claude/state/merge-shas.jsonl` — T2 appends 2 corrected rows from the main worktree. Manual side-effect; not a committed file.

```
.claude/
├── agents/rio-impl-merger.md     # T1: reorder §7.5
└── lib/test_onibus_dag.py        # T3: amend-post-bump test
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [306], "note": "No hard deps — bug is live NOW (mc=27,28 both dangling). discovered_from=bughunter mc28. soft_dep P0306 for context only (P0306 created merge-shas.jsonl; DONE ad080ee1). Prio 80: correctness + data-dangling + gc-fragile. The gc.reflogExpireUnreachable=30d clock started 2026-03-19T13:14 — 30 days until git diff dangling..dangling starts failing. Earlier merge = earlier safe."}
```

**Depends on:** none. [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) (DONE [`ad080ee1`](https://github.com/search?q=ad080ee1&type=commits)) created `merge-shas.jsonl` and the `count_bump` machinery — that's context, not a dep (the fix is to the agent doc's call order, not to `merge.py`).

**Conflicts with:** [`rio-impl-merger.md`](../../.claude/agents/rio-impl-merger.md) — no UNIMPL plan touches it (collisions check: not in top-50). [`test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) — [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) T6 fixes fixture at `:655`; this adds a test after `:615`. Different sections — trivial merge.

**Session-cache note:** `.claude/agents/*.md` are session-cached — this edit becomes live for **NEW** merger spawns post-merge, not for in-flight mergers. The in-flight mc=29 merger (if any) will still record a pre-amend SHA. T2's correction approach (append override rows, last-wins) handles stragglers: coordinator can append corrected rows manually until the fixed merger.md is the one being spawned.
