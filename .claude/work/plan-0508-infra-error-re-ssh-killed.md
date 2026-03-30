# Plan 508: _INFRA_ERROR_RE — corpus-derived SSH + Killed-OOM patterns

[P0447](plan-0447-onibus-flake-excusable-nixbuild-patterns.md) shipped
four patterns for `_INFRA_ERROR_RE` at
[`build.py:173`](../lib/onibus/build.py): `Broken pipe`, `internal_error`,
`resource vanished`, `Transient build error`. Zero of these appear in the
current `/tmp/rio-dev` log corpus (269 files). The patterns came from the
plan doc (prescribed, nixbuild.net-era), not a corpus grep. The module
docstring at [`:166-167`](../lib/onibus/build.py) says the regex targets
"anyone re-enabling remote builders" — but the #1 failure mode for that
scenario is absent.

**`failed to start SSH connection`** — 8 hits across `rio-p0450-impl-{2,3}`
and `rio-sprint-1-{impl-1,merge-1..5}.log`. Same false-negative class as
[P0430](plan-0430-flake-excusable-false-negative-remote-crash.md): SSH
connect fails → no `FAIL:`/`Cannot build` lines emitted → tier-2 "no FAIL
lines" branch returns `False` → implementer blocked on a CI retry that
would have succeeded.

**`Killed` (SIGKILL during rustc compilation)** — P0501 bg CI hit this
2026-03-30 on keynes under concurrent CI runs. `error: Cannot build
.../rust_<crate>-test-cov-test.drv ... Killed`. keynes is ssh-ng (not
nixbuild.net), so none of the 4 shipped patterns match. OOM on a loaded
remote builder is infrastructure, not a test failure.

Both patterns go into the same regex. One commit.

## Entry criteria

- [P0447](plan-0447-onibus-flake-excusable-nixbuild-patterns.md) merged
  (provides `_INFRA_ERROR_RE` and the tier-1/tier-2 excusability logic)

## Tasks

### T1 — `fix(tooling):` add SSH-connect + Killed-OOM to _INFRA_ERROR_RE

MODIFY [`build.py:172-175`](../lib/onibus/build.py). Current:

```python
_INFRA_ERROR_RE = re.compile(
    r"^error: .*?(Broken pipe|internal_error|resource vanished|Transient build error)",
    re.MULTILINE,
)
```

After:

```python
# 4 original patterns: nixbuild.net-era (P0447 plan-prescribed; zero
# corpus hits 2026-03). Retained — nixbuild.net may return.
# 2 corpus-derived (P0508):
#   - "failed to start SSH connection" — 8 hits across p0450/sprint-1
#     logs; SSH connect fails → no FAIL lines → tier-2 false-negative.
#     Same class as P0430.
#   - "Killed" at EOL — SIGKILL on remote builder (OOM under concurrent
#     CI on keynes). Anchored `Killed\s*$` to avoid matching test
#     assertions that mention Killed mid-line.
_INFRA_ERROR_RE = re.compile(
    r"^error: .*?(Broken pipe|internal_error|resource vanished|Transient build error"
    r"|failed to start SSH connection|Killed\s*$)",
    re.MULTILINE,
)
```

The `Killed\s*$` anchor is the false-positive guard: a test that asserts
on the string "Killed" mid-line (`assert "Killed" in output`) would not
produce `^error: .*Killed$`. Nix's actual OOM line ends with `Killed` (the
kernel's `SIGKILL` message gets suffixed to the drv build failure).

### T2 — `test(tooling):` corpus-grounded positive cases

MODIFY [`test_scripts.py`](../lib/test_scripts.py) near the existing
`_INFRA_ERROR_RE` positive block at `:1508-1511`. Add two new positive
cases grepped from the actual corpus (not prescribed):

```python
# P0508: corpus-derived (rio-sprint-1-merge-3.log:NNNN, 2026-03-30)
"error: failed to start SSH connection to 'ssh-ng://keynes.home.meurer.org'",
# P0508: corpus-derived (rio-p0501-bg.log:NNNN, 2026-03-30 — keynes OOM)
"error: Cannot build '/nix/store/abc-rust_rio_store-test-cov-test.drv': ... Killed",
```

**Before committing:** grep the actual corpus files for the exact line
(byte-for-byte), replace `NNNN` with the real line number. The existing
false-positive guards at `:1537`/`:1549` are log-verified
(`rio-p0454-impl-2.log:34269` checks out byte-for-byte per the P0447
review); these new positives should meet the same bar. If the corpus files
are gone (log rotation), mark the line as
`# reconstructed from P0508 followup description` — plausible over
prescribed.

Add one negative case for the `Killed` anchor:

```python
# P0508 false-positive guard: test assertion mentioning Killed
# mid-line — must NOT match (\s*$ anchor).
"error: test assertion failed: expected 'Killed signal' in output",
```

## Exit criteria

- `python3 -c 'from onibus.build import _INFRA_ERROR_RE; assert _INFRA_ERROR_RE.search("error: failed to start SSH connection to ssh-ng://foo")'` — matches
- `python3 -c 'from onibus.build import _INFRA_ERROR_RE; assert _INFRA_ERROR_RE.search("error: Cannot build /nix/store/abc.drv: builder for ... Killed")'` — matches
- `python3 -c 'from onibus.build import _INFRA_ERROR_RE; assert not _INFRA_ERROR_RE.search("error: test failed: expected Killed signal in output")'` — does NOT match (anchor works)
- `nix develop -c pytest .claude/lib/test_scripts.py -k INFRA_ERROR` — passes, ≥2 new positive cases + 1 new negative
- `grep -c 'corpus-derived' .claude/lib/onibus/build.py` ≥ 1 (T1 comment names provenance)
- `/nixbuild .#ci` green

## Tracey

No domain markers. `_INFRA_ERROR_RE` is internal tooling
(`onibus` CI retry gate), not a component spec. `docs/src/components/`
covers rio-{gateway,scheduler,controller,builder,store}; tooling
excusability logic has no spec marker and doesn't need one.

## Files

```json files
[
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T1: add SSH-connect + Killed\\s*$ to _INFRA_ERROR_RE at :173 + provenance comment"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: 2× corpus-derived positive + 1× Killed-anchor negative near :1508"}
]
```

```
.claude/lib/
├── onibus/build.py    # T1: regex + comment
└── test_scripts.py    # T2: corpus positives + anchor negative
```

## Dependencies

```json deps
{"deps": [447], "soft_deps": [], "note": "Edits the same regex line P0447 introduced. P0447 is DONE; no rebase risk. Zero collision with other open plans (build.py not in collisions top-30)."}
```

**Depends on:** [P0447](plan-0447-onibus-flake-excusable-nixbuild-patterns.md)
— introduced `_INFRA_ERROR_RE` and the tier-1 excusability hook this
extends.

**Conflicts with:** none. `build.py` and `test_scripts.py` not in
`onibus collisions top 30`. [P0295](plan-0295-doc-rot-batch-sweep.md)
T503 touches `test_scripts.py:1506` (the "realistic"→"plausible"
comment one block above this plan's insertion point at `:1508`) —
adjacent lines, not overlapping.
