# Plan 969024005: onibus flake excusable misses nixbuild.net infra-error patterns

Coordinator-filed correctness finding at [`.claude/lib/onibus/build.py`](../../.claude/lib/onibus/build.py). `onibus flake excusable` checks whether a CI failure matches a `known-flakes.jsonl` entry, but the pattern matcher doesn't recognize nixbuild.net remote-builder infrastructure errors: `Broken pipe`, `internal_error`, `resource vanished`, `Transient build error`. These are sandbox-crash / network-partition failures on the remote side, not test failures — they should be excusable-retryable.

**Observed impact:** P0430 iter-1 hit a `vm-le-build-k3s` infra flake. The test WAS in `known-flakes.jsonl`, but `excusable` returned `false` because the log contained a remote-builder crash signature rather than a `FAIL`/`Cannot build` line the matcher recognizes. The implementer was blocked on a false-negative.

## Tasks

### T1 — `fix(harness):` build.py excusable — add nixbuild.net infra-error patterns

In [`build.py`](../../.claude/lib/onibus/build.py)'s `excusable` checker (grep for the existing `FAIL\|Cannot build` pattern list), extend the recognized-patterns set to include:

- `Broken pipe` (SSH connection dropped mid-build)
- `internal_error` (nixbuild.net server-side crash)
- `resource vanished` (remote sandbox killed)
- `Transient build error` (nixbuild.net's own transient-retry marker)

These should match as infra-flake regardless of whether the test name is in `known-flakes.jsonl` — they're infrastructure failures, not test flakes. Consider a two-tier check: (1) infra-error patterns → always excusable; (2) test-name in known-flakes + test-failure pattern → excusable.

### T2 — `test(harness):` excusable recognizes nixbuild infra patterns

Add to [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py): `test_excusable_nixbuild_infra_patterns` — feed each of the four patterns through `excusable()`, assert `True`. Include a negative case (a real test FAIL that ISN'T in known-flakes) asserting `False`.

## Exit criteria

- `/nbr .#ci` green
- `pytest .claude/lib/test_scripts.py -k excusable_nixbuild` → passed
- `grep 'Broken pipe\|internal_error\|resource vanished\|Transient build error' .claude/lib/onibus/build.py` → ≥4 hits (all four patterns added)

## Tracey

No tracey marker — harness tooling.

## Files

```json files
[
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T1: extend excusable pattern list with 4 nixbuild.net infra-error signatures"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_excusable_nixbuild_infra_patterns"}
]
```

```
.claude/lib/
├── onibus/
│   └── build.py           # T1: +4 infra-error patterns
└── test_scripts.py        # T2: regression test
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "No deps — the excusable checker has been stable. Pure pattern-list extension."}
```

**Depends on:** none.
**Conflicts with:** `build.py` is low-traffic. `test_scripts.py` is shared by [P969024004](plan-969024004-merge-agent-start-path-padding.md) T2 — both add tests, append-only, rebase-clean.
