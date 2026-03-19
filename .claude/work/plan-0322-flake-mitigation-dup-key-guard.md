# Plan 0322: flake-mitigation picks FIRST on dup test key — add guard + flake-add uniqueness check

[P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) T3 fixed the one extant duplicate in [`known-flakes.jsonl`](../../.claude/known-flakes.jsonl) (`vm-lifecycle-recovery-k3s` ×2 → sentinel rename). Then P0317 T5 added the `flake mitigation` verb at [`cli.py:369-383`](../../.claude/lib/onibus/cli.py) — which has the **same bug class** with no guard:

```python
target = next((r for r in rows if r.test == args.test), None)   # :372
```

`next()` on a generator picks the **first** match. If two rows share a `test` key, the mitigation silently appends to the first; the second is unreachable. And [`flake add`](../../.claude/lib/onibus/cli.py) at `:360` is plain `append_jsonl` with zero uniqueness check — re-introducing a duplicate is one CLI call away.

The `flake remove` verb at `:363` uses `remove_jsonl(..., lambda f: f.test == args.test)` which removes **all** matching rows — so a dup-aware caller could clean up by remove-then-add. But nothing enforces that sequence. The failure is silent: mitigation "succeeds" (returncode 0), appends to one row, the operator never knows the other row exists.

Both `excusable()` (match-by-drv_name, [`build.py`](../../.claude/lib/onibus/build.py)) and `flake mitigation` (match-by-test) treat `test` as a key. It's a key in practice but not in schema — no uniqueness enforcement anywhere.

## Entry criteria

- [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) merged (`flake mitigation` verb + `Mitigation` model exist) — **DONE**

## Tasks

### T1 — `fix(harness):` flake-add rejects duplicate test key

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) at `:359-361`. Before `append_jsonl`, read existing rows and check:

```python
if args.flake_cmd == "add":
    new = KnownFlake.model_validate_json(args.json_row)
    existing = read_jsonl(KNOWN_FLAKES, KnownFlake)
    dups = [r for r in existing if r.test == new.test]
    if dups:
        print(
            f"known-flake with test={new.test!r} already exists. "
            f"Use `onibus flake remove {new.test!r}` first, "
            f"or pick a different test key (sentinel names like "
            f"`<tcg-builder-allocation>` are fine for infra-wide entries).",
            file=sys.stderr,
        )
        return 1
    append_jsonl(KNOWN_FLAKES, new)
    return 0
```

**`test` as primary key is now enforced at the write-path.** Operators who WANT to track two distinct flake modes for the same test should use distinct sentinel names (P0317 T3 set the precedent: `<tcg-builder-allocation>` is a sentinel, not a real test name).

### T2 — `fix(harness):` flake-mitigation errors on multiple matches

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) at `:371-375`. Replace `next(...)` with an explicit length check:

```python
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
    # ... rest unchanged (header-preserving rewrite at :380-381)
```

Returncode 2 (distinct from 1=not-found) so callers can discriminate if needed.

### T3 — `test(harness):` both guards reject

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — add near the existing `flake add`/`flake remove` test at `:1063`:

```python
def test_flake_add_rejects_duplicate_test_key(tmp_repo: Path):
    """T1 guard: flake-add with an existing test key → rc=1, stderr
    message, file unchanged. Without the guard, append_jsonl silently
    creates a duplicate and flake-mitigation picks the first."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    flakes = tmp_repo / ".claude" / "known-flakes.jsonl"
    first = KnownFlake(
        test="vm-foo-k3s", symptom="s", root_cause="rc",
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
    lines = [l for l in flakes.read_text().splitlines() if not l.startswith("#")]
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
        test="vm-dup", symptom="s", root_cause="rc",
        fix_owner="P0999", fix_description="d", retry="Once",
    ).model_dump_json()
    # Two rows, same test key — bypasses T1's guard by writing directly.
    flakes.write_text(f"# header\n{row}\n{row}\n")

    r = subprocess.run(
        [
            ".claude/bin/onibus", "flake", "mitigation",
            "--test", "vm-dup", "--plan", "999",
            "--sha", "deadbeef", "--note", "n",
        ],
        cwd=tmp_repo, capture_output=True, text=True,
    )
    assert r.returncode == 2
    assert "AMBIGUOUS" in r.stderr
    # File untouched — no mitigation appended to either row.
    assert "deadbeef" not in flakes.read_text()
```

**Check at dispatch:** `flake mitigation` argparse signature (`--test`, `--plan`, `--sha`, `--note` vs positional) — grep `cli.py` for the `mitigation` subparser setup. Adjust test invocation to match.

## Exit criteria

- `/nbr .#ci` green
- `.claude/bin/onibus flake add '<json-for-existing-test>'` → rc=1, stderr contains "already exists"
- `grep -c 'len(matches) > 1\|AMBIGUOUS' .claude/lib/onibus/cli.py` ≥ 2 (T2: guard + error message)
- `grep 'next((r for r in rows if r.test' .claude/lib/onibus/cli.py` → 0 (T2: the silent-first-match `next()` pattern is gone)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'flake_add_rejects or mitigation_errors'` → 2 passed
- **Mutation criterion for T1:** comment out the `if dups:` block → `test_flake_add_rejects_duplicate_test_key` fails (rc=0 instead of rc=1). Restore → passes.
- **Mutation criterion for T2:** change `if len(matches) > 1:` to `if len(matches) > 999:` → `test_flake_mitigation_errors_on_multiple_matches` fails (mitigation appends to first row, "deadbeef" appears in file). Restore → passes.

## Tracey

No markers. Harness code — `onibus` CLI is tooling, not spec'd behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T1: flake-add dup-check before append :359-361. T2: flake-mitigation len(matches)>1 guard :371-375, replace next() with list"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T3: +test_flake_add_rejects_duplicate_test_key + test_flake_mitigation_errors_on_multiple_matches (near existing :1063 flake test)"}
]
```

```
.claude/lib/
├── onibus/cli.py                 # T1+T2: dup guards at :360, :372
└── test_scripts.py               # T3: 2 negative-path tests
```

## Dependencies

```json deps
{"deps": [317], "soft_deps": [304, 311], "note": "P0317 DONE (flake-mitigation verb + Mitigation model exist). Soft-conflict P0304 T13 (also touches cli.py — --link argparse plumbing; different verb, different subparser, no overlap). Soft-conflict P0311 T9 (see below — happy-path mitigation tests; both additive to test_scripts.py). P0304 T18 (_read_header DRY extraction) touches cli.py:380 and jsonl.py:68 — T2 here keeps the header-extraction at :380 as-is (only changes :371-375); if P0304 T18 lands first and extracts _read_header(), T2 sees the helper call instead of the list-comp. Either order works."}
```

**Depends on:** [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) — **DONE**. The verb and model this plan guards both landed there.

**Conflicts with:**
- [`cli.py`](../../.claude/lib/onibus/cli.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T13 adds `--link` argparse plumbing (different subparser entirely); T21-in-P0304 (new batch append, this same docs run) extracts `_read_header()` from `:380` (T2 here doesn't touch `:380`). Both low-risk overlaps.
- [`test_scripts.py`](../../.claude/lib/test_scripts.py) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T9 (also appended this docs run) adds happy-path `flake mitigation` tests + `Mitigation.landed_sha` pattern tests. T3 here adds negative-path (dup-reject) tests. Both additive test functions; different names; zero conflict. The two plans are **complementary**: T9 proves the verb works, T3 here proves it refuses when it shouldn't work.
