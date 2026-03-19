# Plan 0324: conftest.py — tmp_repo_patched fixture (consolidate 22× monkeypatch setup)

**Consolidator finding (mc-window).** The `tmp_repo` + `monkeypatch` setup block is copied 22× across [`test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) and [`test_scripts.py`](../../.claude/lib/test_scripts.py). [`conftest.py:12`](../../.claude/lib/conftest.py) has the `tmp_repo` fixture but no module-global wiring — every test that needs onibus's path constants pointed at the scratch repo does the patching inline.

The duplication breaks down as:

| Patch target | Count | Example sites |
|---|---|---|
| `onibus.merge.STATE_DIR` | 11× | test_onibus_dag.py:605, :641, :697, :736, :756 |
| `onibus.git_ops.REPO_ROOT` | 11× | test_onibus_dag.py:440, :454, :468; test_scripts.py:1472, :1519 |
| `onibus.merge.INTEGRATION_BRANCH` | 5× | test_onibus_dag.py:606, :642, :698 |
| `onibus.plan_doc.REPO_ROOT` | 2× | test_scripts.py:1473, :1520 |

The full 5-line block (state.mkdir + chdir + two setattrs) appears verbatim at [`test_onibus_dag.py:602-606`](../../.claude/lib/test_onibus_dag.py), `:638-642`, `:694-698` — three copies character-identical. [P0319](plan-0319-merge-shas-pre-amend-dangling.md) added the 11th `STATE_DIR` patch at `:641`; each new test that touches `onibus.merge` copies the block again.

**Why the patches target submodules, not the package:** [`merge.py:21`](../../.claude/lib/onibus/merge.py) does `from onibus import INTEGRATION_BRANCH, STATE_DIR` — this binds a *module-local name* at import time. Patching `onibus.STATE_DIR` doesn't propagate to `onibus.merge.STATE_DIR`. Tests must patch each submodule's copy. The fixture centralizes the list of which submodules need which patches.

**ROI window:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T9, [P0322](plan-0322-flake-mitigation-dup-key-guard.md) T3, and [P0323](plan-0323-mergesha-pydantic-model.md) T4 all add new tests to `test_scripts.py` — each will want the same setup. Landing this first means those tests write `def test_foo(tmp_repo_patched):` instead of copying the block a 12th/13th/14th time.

## Tasks

### T1 — `refactor(harness):` add tmp_repo_patched fixture to conftest.py

MODIFY [`.claude/lib/conftest.py`](../../.claude/lib/conftest.py) — add after the existing `tmp_repo` fixture at `:28`:

```python
@pytest.fixture
def tmp_repo_patched(
    tmp_repo: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[Path, Path]:
    """tmp_repo + onibus path constants patched to point at it. Returns
    (repo_root, state_dir).

    Each onibus submodule does `from onibus import REPO_ROOT, STATE_DIR`
    at import time (merge.py:21, git_ops.py, plan_doc.py). That binds a
    module-local name — patching onibus.__init__ doesn't propagate. The
    loop below patches every submodule's copy. hasattr guards modules
    that don't import a given constant (not all do).

    INTEGRATION_BRANCH is re-patched to the package-level value. This
    looks like a no-op but isn't: conftest's tmp_repo init'd the scratch
    repo ON that branch name. If a test's local import order races with
    the package's file-read, onibus.merge.INTEGRATION_BRANCH could be
    stale — explicit setattr is the defensive fence.

    chdir: several onibus functions shell out with relative paths
    (git cwd defaults, plan-doc globs). Tests that only call
    absolute-path-taking functions don't need it, but patching it
    for everyone is harmless."""
    import onibus
    import onibus.git_ops
    import onibus.merge
    import onibus.plan_doc

    state = tmp_repo / ".claude" / "state"
    state.mkdir(parents=True, exist_ok=True)
    monkeypatch.chdir(tmp_repo)

    for mod in (onibus.merge, onibus.git_ops, onibus.plan_doc):
        if hasattr(mod, "REPO_ROOT"):
            monkeypatch.setattr(mod, "REPO_ROOT", tmp_repo)
        if hasattr(mod, "STATE_DIR"):
            monkeypatch.setattr(mod, "STATE_DIR", state)
    monkeypatch.setattr(onibus.merge, "INTEGRATION_BRANCH", onibus.INTEGRATION_BRANCH)

    return tmp_repo, state
```

~25 lines added (including docstring). The `hasattr` guards make the loop extensible — when a new submodule gets a path constant, add it to the tuple once.

**NOT covered by this fixture:** `dag_tick.STATE_DIR` (3×) and `dag_stop.STATE_DIR` (1×) in `test_scripts.py` — those are separate skill-script modules, not `onibus.*` submodules. They use `tmp_path` not `tmp_repo` (no git repo needed, just a scratch dir). Leave them as-is; a second fixture for skill-script STATE_DIR is a different consolidation axis.

### T2 — `refactor(harness):` migrate test_onibus_dag.py call sites

MODIFY [`.claude/lib/test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py). Two migration tiers:

**Tier A — full 5-line block (verbatim copies).** Replace the block with fixture unpacking. At `:595-606`, `:618-642`, `:682-698`, `:730-736` (check), `:750-756` (check):

```python
# BEFORE:
def test_count_bump_records_merge_sha(tmp_repo: Path, monkeypatch):
    import json as _json
    import onibus.merge
    from onibus import INTEGRATION_BRANCH
    state = tmp_repo / ".claude" / "state"
    state.mkdir(parents=True, exist_ok=True)
    monkeypatch.chdir(tmp_repo)
    monkeypatch.setattr(onibus.merge, "STATE_DIR", state)
    monkeypatch.setattr(onibus.merge, "INTEGRATION_BRANCH", INTEGRATION_BRANCH)
    n = onibus.merge.count_bump()
    # ...

# AFTER:
def test_count_bump_records_merge_sha(tmp_repo_patched: tuple[Path, Path]):
    import json as _json
    import onibus.merge
    from onibus import INTEGRATION_BRANCH
    tmp_repo, state = tmp_repo_patched
    n = onibus.merge.count_bump()
    # ...
```

Tests that reference `tmp_repo` in their body (e.g. `_git(tmp_repo, ...)`) still have it via unpacking. Tests that only touch `state` (e.g. reading `state / "merge-shas.jsonl"`) have that too.

**Tier B — REPO_ROOT-only (single setattr).** At `:438-440`, `:452-454`, `:466-468`, `:474-477`, `:488-491`, `:506-509`, `:523-525`, `:540-542`, `:895-897`:

```python
# BEFORE:
def test_convco_check(tmp_repo: Path, monkeypatch):
    import onibus.git_ops
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    # ...

# AFTER:
def test_convco_check(tmp_repo_patched: tuple[Path, Path]):
    import onibus.git_ops
    tmp_repo, _ = tmp_repo_patched
    # ...
```

These tests don't need `state`, but getting it via the fixture is harmless (it's just a Path that happens to exist). Discard with `_`.

**Leave alone:** tests that patch `STATE_DIR` to `tmp_path` directly (no git repo): `:568`, `:579`, `:589`, `:846`, `:861`, `:875`. These test state-file I/O without git operations — `tmp_path` is cheaper than `tmp_repo` (no `git init`). Migrating them would slow the test suite for no benefit. The special `INTEGRATION_BRANCH = "HEAD"` patch at `:780` is also a deliberate override — leave it.

**Expected delta:** ~15 test functions migrated, ~45-50 lines deleted, ~15 lines added (unpacking). Net ~-35L in this file.

### T3 — `refactor(harness):` migrate test_scripts.py call sites

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Only two sites here — both do the double-REPO_ROOT patch (git_ops + plan_doc):

At `:1466-1473` and `:1519-1520`:

```python
# BEFORE:
def test_atomicity_check_something(tmp_repo: Path, monkeypatch):
    # atomicity_check.run shells out via _lib.git (cwd defaults to REPO_ROOT)
    # and globs plan docs via _lib.find_plan_doc (also REPO_ROOT-rooted).
    # Both read REPO_ROOT from _lib's module namespace at call time — one
    # setattr per module.
    import onibus.git_ops, onibus.plan_doc
    monkeypatch.setattr(onibus.git_ops, "REPO_ROOT", tmp_repo)
    monkeypatch.setattr(onibus.plan_doc, "REPO_ROOT", tmp_repo)
    # ...

# AFTER:
def test_atomicity_check_something(tmp_repo_patched: tuple[Path, Path]):
    tmp_repo, _ = tmp_repo_patched
    # ...
```

The comment block at `:1466-1470` explaining WHY both modules need patching — **move it into the fixture's docstring** (T1 already includes a version). Delete the comment from the call site; the fixture is the single source of truth now.

**Expected delta:** 2 functions migrated, ~8 lines deleted, ~2 lines added. Net ~-6L in this file.

**Leave alone:** `dag_tick.STATE_DIR` (`:1269`, `:1321`, `:1359`) and `dag_stop.STATE_DIR` (`:1399`) — see T1 NOT-covered note.

## Exit criteria

- `nix develop -c pytest .claude/lib/ -x` → all existing tests still pass (no behavior change — pure refactor)
- `grep -c 'setattr.*onibus.merge.*STATE_DIR\|setattr.*onibus.git_ops.*REPO_ROOT\|setattr.*onibus.plan_doc.*REPO_ROOT' .claude/lib/test_onibus_dag.py .claude/lib/test_scripts.py` → ≤ 1 (only the `:780` INTEGRATION_BRANCH="HEAD" special case remains in test_onibus_dag.py; it's an override, not the setup block)
- `grep -c 'tmp_repo_patched' .claude/lib/test_onibus_dag.py` → ≥ 14 (Tier A + Tier B sites migrated)
- `grep -c 'tmp_repo_patched' .claude/lib/test_scripts.py` → ≥ 2
- `grep -A3 'def tmp_repo_patched' .claude/lib/conftest.py` → fixture exists with `tuple[Path, Path]` return annotation
- Net line delta: `git diff --stat sprint-1 -- .claude/lib/conftest.py .claude/lib/test_onibus_dag.py .claude/lib/test_scripts.py` → overall negative (more deleted than added)
- **Regression guard — the pattern that motivated this:** `grep -B2 'state.mkdir(parents=True' .claude/lib/test_onibus_dag.py | grep -c 'def test_'` → 0 (no test function body still has the inline mkdir-then-patch block)

## Tracey

No markers. This plan touches `.claude/lib/` test infrastructure only — harness code is not spec'd in `docs/src/components/`. No `r[...]` annotations apply.

## Files

```json files
[
  {"path": ".claude/lib/conftest.py", "action": "MODIFY", "note": "T1: add tmp_repo_patched fixture (~25L after existing tmp_repo at :28)"},
  {"path": ".claude/lib/test_onibus_dag.py", "action": "MODIFY", "note": "T2: migrate ~15 test fns to tmp_repo_patched, net ~-35L. Leave tmp_path-only tests (:568/:579/:589/:846/:861/:875) and :780 override alone"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T3: migrate 2 test fns (:1466-1473, :1519-1520), delete explanatory comment, net ~-6L. Leave dag_tick/dag_stop STATE_DIR alone"}
]
```

```
.claude/lib/
├── conftest.py           # T1: +tmp_repo_patched fixture
├── test_onibus_dag.py    # T2: ~15 fns migrated, -35L
└── test_scripts.py       # T3: 2 fns migrated, -6L
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [311, 322, 323], "note": "No hard deps — pure refactor, conftest.py:12 tmp_repo already exists, all setup blocks already exist in test files. SHIP BEFORE P0311 T9 / P0322 T3 / P0323 T4 — all three add tests to test_scripts.py and will otherwise copy the setup block a 12th/13th/14th time. If this lands first, those tests can use the fixture directly. soft_deps are REVERSE — this doesn't depend on them, they benefit from this landing first. Consolidator origin (mc-window). Priority: 60 (low complexity, high ROI for the three queued test plans)."}
```

**Depends on:** none — [`conftest.py:12`](../../.claude/lib/conftest.py) `tmp_repo` already exists; the 22 call sites already exist.

**Ship order:** land BEFORE [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T9, [P0322](plan-0322-flake-mitigation-dup-key-guard.md) T3, [P0323](plan-0323-mergesha-pydantic-model.md) T4 if possible. All three add tests to `test_scripts.py`; each will copy the setup block unless the fixture exists first. Not a hard dep — those plans can land without this (they just copy-paste like the prior 22 did), but the coordinator should prefer sequencing this first.

**Conflicts with:** [`test_scripts.py`](../../.claude/lib/test_scripts.py) — P0311 T9, P0322 T3, P0323 T4 all ADD test functions (new `def test_*` at end of file or near `:1063`). T3 here MODIFIES existing functions at `:1466-1520`. Non-overlapping line ranges → textual merge clean. [`test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) — no other UNIMPL plan touches it (checked collisions-top-50, not present). [`conftest.py`](../../.claude/lib/conftest.py) — no other plan touches it. **[P0323](plan-0323-mergesha-pydantic-model.md) also edits [`merge.py`](../../.claude/lib/onibus/merge.py)** at `:145-185` — irrelevant here (this plan doesn't touch merge.py), but if the coordinator batches P0324 + [P0325](plan-0325-rename-unassigned-post-ff-rewrite-skip.md) + P0323 in parallel, note that P0325 and P0323 both edit merge.py (different functions, `:145-185` vs `:329-361`).
