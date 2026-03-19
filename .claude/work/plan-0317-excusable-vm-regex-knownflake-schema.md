# Plan 317: `onibus flake excusable()` VM-regex + `KnownFlake` schema evolution

**Bughunter mc21 finding, coordinator-promoted.** The retry gate the entire TCG fast-fail chain ([P0313](plan-0313-kvm-fast-fail-preamble.md)→[P0315](plan-0315-kvm-ioctl-probe.md)→[P0316](plan-0316-qemu-force-accel-kvm.md)) feeds into is **a no-op for VM tests.** [`build.py:132`](../../.claude/lib/onibus/build.py) `_NEXTEST_FAIL_RE` only matches nextest `FAIL [Ns] crate::path` lines. VM test failures log as `error: Cannot build '/nix/store/<hash>-vm-test-run-<name>.drv'.` — verified in `/tmp/rio-dev/rio-p0209-impl-*.log`. All 7 [`known-flakes.jsonl`](../../.claude/known-flakes.jsonl) entries have VM-style `test` fields (`vm-lifecycle-recovery-k3s` etc.); the regex returns `failing=[]`; [`build.py:145`](../../.claude/lib/onibus/build.py) says `"no FAIL lines in log"`; `excusable=False`. Implementers hitting TCG get red `.#ci` reported as real — wasted debug cycles on infra flake.

**Three independent sources carry the same false mental model:** [`common.nix:131`](../../nix/tests/common.nix) comment claims "KVM-DENIED-BUILDER marker lets `onibus flake excusable` pattern-match this" — `excusable()` does not grep log content; the coordinator's own P0316-validates followup stated "the `symptom` string is what matters for grep-recognition" — [`known-flakes.jsonl:4`](../../.claude/known-flakes.jsonl) header says `symptom` is "for human cross-check" only, match key is `test`; [`test_onibus_dag.py:655`](../../.claude/lib/test_onibus_dag.py) fixture uses nextest-style `crate::mod::flaky_test`, not a real VM drv name, so the test passes against the wrong shape. Two agents + a code comment converged on symptom-grep behavior that doesn't exist.

**Scope expansion discovered during verification:** the drv name extracted by the bughunter's proposed regex (`rio-lifecycle-recovery` from `vm-test-run-rio-lifecycle-recovery.drv`) does NOT match the known-flakes `test` field (`vm-lifecycle-recovery-k3s`). The `test` field is the **flake attr** ([`flake.nix:594`](../../flake.nix)); the drv name is the **nixosTest `name`** ([`lifecycle.nix:1406`](../../nix/tests/scenarios/lifecycle.nix) `name = "rio-lifecycle-${name}"` + [`default.nix:270`](../../nix/tests/default.nix) `name = "recovery"`). No algorithmic mapping exists — `vm-le-stability-k3s` → `rio-leader-election-stability`, `vm-cli-k3s` → `rio-cli`. A schema field is required.

**Duplicate-key bug surfaced during verification:** [`known-flakes.jsonl:7`](../../.claude/known-flakes.jsonl) and `:11` both have `test: "vm-lifecycle-recovery-k3s"` (sched_metric timeout vs KVM-denied/TCG — two distinct flake modes, same test). [`build.py:141`](../../.claude/lib/onibus/build.py) `{f.test: f for f in ...}` silently overwrites line 7 with line 11; [`cli.py:363`](../../.claude/lib/onibus/cli.py) `onibus flake remove` deletes both. Disambiguated via `drv_name` uniqueness validator.

**Consolidator fold-in (same file, same schema evolution):** `KnownFlake.fix_description` has grown to 724 chars on line 11 via 3 same-shape `[PXXX LANDED sha: note]` bracket appends this window ([`e9734c1d`](https://github.com/search?q=e9734c1d&type=commits), [`d76ac685`](https://github.com/search?q=d76ac685&type=commits) prematurely, [`5c68733e`](https://github.com/search?q=5c68733e&type=commits) string-surgery-corrected). The string-surgery-to-fix-premature-append pattern is the signal — extract to structured `mitigations: list[Mitigation]` + `onibus flake mitigation` verb. P0316 already appended a 4th bracket ([`900ac467`](https://github.com/search?q=900ac467&type=commits)). Done here while the schema is already being evolved; avoids two separate migration passes on the same JSONL.

**Relationship to [P0304](plan-0304-trivial-batch-p0222-harness.md) T10:** T10's `_TCG_MARKERS` early-return design has a masking risk without this plan's VM-regex: if a real nextest FAIL co-occurs with a TCG marker (multi-test `.#ci` run; one VM hits TCG, one unit test genuinely fails), T10's early-return grants excusability blindly. With `_VM_FAIL_RE` from this plan, `failing` contains both → "2 failures, not excusable" → correct. T10's `_TCG_MARKERS` should be re-scoped to a **supplementary grant** (T7 below forward-references this): if `failing` is exactly one VM test AND a TCG marker is present, excusable even if that test isn't in known-flakes (TCG is pure infra). Ship order: **this plan first**, T10 layers on safely.

## Tasks

### T1 — `fix(harness):` `_VM_FAIL_RE` — extract VM drv names from `Cannot build` lines

MODIFY [`.claude/lib/onibus/build.py`](../../.claude/lib/onibus/build.py) at `:132`.

Actual CI log line (verified `/tmp/rio-dev/rio-p0209-impl-1.log:15780`, `rio-sprint-1-merge-*.log`):

```
error: Cannot build '/nix/store/lbb1v37c1dm9dmx0ghcy3zzjwk6kzywd-vm-test-run-rio-lifecycle-recovery.drv'.
```

Single-line top-level nix error. The bughunter's proposed regex (`builder for .+failed`) matches the wrong line (`Reason: builder failed with exit code 143` — separate line, no drv path). Add after `_NEXTEST_FAIL_RE`:

```python
# VM test drv failure: "error: Cannot build '/nix/store/<hash>-vm-test-run-<name>.drv'."
# <name> is the nixosTest `name` attr (e.g., rio-lifecycle-recovery), NOT the
# flake attr (vm-lifecycle-recovery-k3s). known-flakes.jsonl stores flake-attr
# names in `test`; drv names in `drv_name`. Match is against drv_name.
# Verified against /tmp/rio-dev/rio-p0209-impl-*.log — this is the top-level
# nix error line; the `Reason: builder failed with exit code N` is a SEPARATE
# line with no drv path.
_VM_FAIL_RE = re.compile(
    r"^error: Cannot build '/nix/store/[a-z0-9]+-vm-test-run-([\w-]+)\.drv'",
    re.MULTILINE,
)
```

Then in `excusable()` at `:140`, union both extraction sets:

```python
def excusable(log_path: Path) -> ExcusableVerdict:
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
        reason, ok = f"{failing[0]!r} not in known-flakes.jsonl (neither test nor drv_name)", False
    elif matched_row.retry == "Never":
        reason, ok = f"{matched[0]!r} is known-flake but retry=Never — investigate, don't retry", False
    else:
        reason, ok = f"single failure {matched[0]!r} is known-flake retry={matched_row.retry}", True
    return ExcusableVerdict(
        excusable=ok, failing_tests=failing, matched_flakes=matched, reason=reason,
    )
```

### T2 — `feat(harness):` `KnownFlake.drv_name` field + migrate 7 entries

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) at `:138-148`:

```python
class KnownFlake(BaseModel):
    test: str = Field(
        description="Flake-attr name (vm-lifecycle-recovery-k3s) for VM tests, "
        "crate::module::test_name for nextest. Human-identifier."
    )
    drv_name: str | None = Field(
        default=None,
        description="nixosTest name attr — the <N> in vm-test-run-<N>.drv as it "
        "appears in `error: Cannot build` CI log lines. VM tests ONLY (nextest "
        "entries leave this None). Match key for excusable() VM-regex. Set from "
        "nix/tests/scenarios/*.nix `name = \"rio-...\"` composition, NOT the "
        "default.nix attrset key. e.g., vm-lifecycle-recovery-k3s → "
        "rio-lifecycle-recovery (lifecycle.nix:1406 name=\"rio-lifecycle-${name}\" "
        "+ default.nix:270 name=\"recovery\")."
    )
    symptom: str
    ...
```

MODIFY [`.claude/known-flakes.jsonl`](../../.claude/known-flakes.jsonl) — add `drv_name` to each of 7 rows. Mapping table (verified against `/tmp/rio-dev/rio-p0209-impl-3.log` drv-copy lines 63-193):

| Line | `test` | `drv_name` | Source |
|---|---|---|---|
| 5 | `vm-lifecycle-core-k3s` | `rio-lifecycle-core` | [`lifecycle.nix:1406`](../../nix/tests/scenarios/lifecycle.nix) + [`default.nix:258`](../../nix/tests/default.nix) |
| 6 | `vm-lifecycle-autoscale-k3s` | `rio-lifecycle-autoscale` | same `:1406` + `:275` |
| 7 | `vm-lifecycle-recovery-k3s` | `rio-lifecycle-recovery` | same `:1406` + `:270` |
| 8 | `vm-le-stability-k3s` | `rio-leader-election-stability` | [`leader-election.nix:502`](../../nix/tests/scenarios/leader-election.nix) + [`default.nix:288`](../../nix/tests/default.nix) |
| 9 | `vm-le-build-k3s` | `rio-leader-election-build` | same `:502` + `:299` |
| 10 | `vm-cli-k3s` | `rio-cli` | [`cli.nix:43`](../../nix/tests/scenarios/cli.nix) |
| 11 | `vm-lifecycle-recovery-k3s` | see T3 | duplicate — disambiguated in T3 |

Edit each JSON row inline (no `onibus flake add/remove` roundtrip — that would drop+re-add, losing ordering and risking the duplicate-delete bug). `jq` one-liner or manual edit; verify with `python3 -c 'from onibus.models import KnownFlake; from onibus.jsonl import read_jsonl; [print(f.test, f.drv_name) for f in read_jsonl(".claude/known-flakes.jsonl", KnownFlake)]'`.

### T3 — `fix(harness):` disambiguate duplicate `test` key (lines 7+11)

Both [`known-flakes.jsonl:7`](../../.claude/known-flakes.jsonl) and `:11` have `test: "vm-lifecycle-recovery-k3s"` — line 7 is the sched_metric timeout flake (P0176), line 11 is KVM-denied/TCG (P0179). The dict comprehension at [`build.py:141`](../../.claude/lib/onibus/build.py) silently collapses them (line 11 wins). [`cli.py:363`](../../.claude/lib/onibus/cli.py) `onibus flake remove vm-lifecycle-recovery-k3s` deletes both.

Line 11 is the TCG entry. It applies to **every** VM test (any derivation change can hit a KVM-denied builder), not just `vm-lifecycle-recovery-k3s` — it was filed against that test because that's where it was first observed. After this plan's `_VM_FAIL_RE` + T7's forward-pointer to [P0304](plan-0304-trivial-batch-p0222-harness.md) T10 `_TCG_MARKERS`, the TCG case is handled by **marker-grep supplementary grant**, not by test-name match.

**Resolution:** Set line 11's `drv_name: null` (T2 leaves it unset) and change its `test` to a sentinel: `"<tcg-builder-allocation>"`. This keeps it in the JSONL for provenance (the 4 bracket-changelogs, the nixbuild.net frontend context) but makes it unmatchable-by-name — which is correct: TCG is infra-wide, not test-specific. P0304 T10's `_TCG_MARKERS` grants excusability when the log has the marker, independent of which test's drv failed.

Also MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) — add a model validator to `KnownFlake` (class-level, after the field_validator):

```python
# Module-level, alongside the other validators:
@model_validator(mode="after")
def _match_surface_defined(self) -> "KnownFlake":
    # Sentinel entries (<tcg-builder-allocation> etc.) are intentionally
    # unmatchable by name — provenance-only rows that P0304 T10's
    # _TCG_MARKERS handles via log-grep. They need neither surface.
    if self.test.startswith("<"):
        return self
    # VM-test entries (vm-*-k3s, vm-*-standalone) need drv_name for
    # _VM_FAIL_RE matching. Nextest entries (crate::path) match via test.
    if self.test.startswith("vm-") and self.drv_name is None:
        raise ValueError(
            f"VM-test known-flake {self.test!r} missing drv_name — "
            f"_VM_FAIL_RE matches against drv_name, not test. Set drv_name "
            f"to the nixosTest name attr (see nix/tests/scenarios/*.nix)."
        )
    return self
```

Import `model_validator` from pydantic at the top of the file if not already there (check `:1-20` imports).

### T4 — `feat(harness):` `KnownFlake.mitigations` — structured bracket-changelog

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) at `:138`. Add before `KnownFlake`:

```python
class Mitigation(BaseModel):
    """One entry in a KnownFlake's fix-history. Was: [PXXX LANDED sha: note]
    bracket-appends in fix_description (724 chars on known-flakes.jsonl:11
    after 4 same-shape appends; 5c68733e did string-surgery to fix a
    premature append — structured list avoids that failure mode)."""
    plan: int = Field(description="P<N> → N")
    landed_sha: str = Field(pattern=r"^[0-9a-f]{8,40}$")
    note: str = Field(description="What the mitigation does + any new symptom string")
```

Then in `KnownFlake`:

```python
    mitigations: list[Mitigation] = Field(
        default_factory=list,
        description="Ordered fix-history. Replaces [PXXX LANDED sha: note] "
        "bracket-appends in fix_description. Appended via `onibus flake "
        "mitigation <test> <plan> <sha> <note>`.",
    )
```

MODIFY [`.claude/known-flakes.jsonl:11`](../../.claude/known-flakes.jsonl) — migrate the 4 brackets from `fix_description` to `mitigations`. Post-T3, this is the `<tcg-builder-allocation>` sentinel row. Trim `fix_description` to just the BRIDGE text (the `fast-fail preamble` paragraph, everything before the first `[P0313`):

```json
{
  "test": "<tcg-builder-allocation>",
  "drv_name": null,
  "symptom": "falling back to tcg — nixbuild.net builder KVM-denied; QEMU software emulation ~10-20x slower; test timeout exit-143; zero code involvement",
  "root_cause": "Subset of nixbuild.net builder pool has KVM permission issue (/dev/kvm inaccessible). QEMU falls back to TCG. 3 different builders observed (ec2-builder4/183/216). Any derivation change that invalidates the VM test cache triggers this — not a code regression.",
  "fix_owner": "P0179",
  "fix_description": "Infrastructure — requires nixbuild.net-side KVM permission fix. BRIDGE: fast-fail preamble (test -r /dev/kvm in testScript) would give 10s fail instead of 15min timeout. Until fixed: any rio-worker change that invalidates recovery drv cache will hit this.",
  "retry": "Once",
  "mitigations": [
    {"plan": 313, "landed_sha": "5bbb5469", "note": "preamble fires exit-77 + KVM-DENIED-BUILDER at ~4s (catches 4/7 O_RDWR-deniable); ioctl-gap remains 3/7. New symptom: builder failed with exit code 77."},
    {"plan": 315, "landed_sha": "a6178a38", "note": "ioctl(KVM_CREATE_VM) probe closes the 3/7 gap — GET_API_VERSION proved insufficient (8/9 passed it, QEMU still fell back to TCG); CREATE_VM is the real LSM gate. New symptom remains: builder failed with exit code 77."},
    {"plan": 316, "landed_sha": "900ac467", "note": "-machine accel=kvm forces per-VM hard-fail — closes the concurrent-VM race (protocol-warm 3×QEMU: kvmCheck single-shot probe succeeds, 2/3 children race-lose on their own CREATE_VM). New symptom: QEMU 'failed to initialize kvm: Permission denied' in VM serial log — distinct from exit-77 (that's preamble-caught). nixbuild.net frontend flag is STALE (all derivations show kvmEnabled=True + slurm kvm:y; actual /dev/kvm state varies) — scheduling never routes away. Feedback mechanism (exit-77 → frontend flag refresh) would need nixbuild.net API coordination; out of rio-build scope."}
  ]
}
```

**Note the P0316 migration fixes [P0295](plan-0295-doc-rot-batch-sweep.md) T25's pre-pivot `-accel kvm` text** — the `note` field now correctly says `-machine accel=kvm`.

### T5 — `feat(harness):` `onibus flake mitigation` verb

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) at `:357-375` (`_cmd_flake`) and `:506` (argparse):

```python
# In _cmd_flake, after the "exists" branch at :368:
if args.flake_cmd == "mitigation":
    from onibus.models import Mitigation
    rows = read_jsonl(KNOWN_FLAKES, KnownFlake)
    target = next((r for r in rows if r.test == args.test), None)
    if target is None:
        print(f"no known-flake with test={args.test!r}", file=sys.stderr)
        return 1
    target.mitigations.append(
        Mitigation(plan=args.plan, landed_sha=args.sha, note=args.note)
    )
    # Rewrite whole file (atomic write_jsonl — already the pattern in jsonl.py)
    write_jsonl(KNOWN_FLAKES, rows)
    print(f"appended mitigation P{args.plan:04d} @ {args.sha} to {args.test!r}")
    return 0
```

Argparse at `:506`:

```python
sp = g.add_parser("mitigation")
sp.add_argument("test")
sp.add_argument("plan", type=int)
sp.add_argument("sha")
sp.add_argument("note")
```

Import `write_jsonl` from `onibus.jsonl` if not already imported at the top of `cli.py`.

**Usage pattern for future implementers** (goes in the merge-step commit message or implementer report, not here): instead of appending `[P0XXX LANDED sha: ...]` brackets to `fix_description`, run `onibus flake mitigation '<tcg-builder-allocation>' <N> <sha> '<note>'`.

### T6 — `test(harness):` fix excusable() fixtures + new VM-regex tests

MODIFY [`.claude/lib/test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) at `:649-704`.

`test_excusable_single_known_flake` at `:649-662` uses nextest-style `crate::mod::flaky_test` — keep it (tests the nextest path), but add a parallel VM-path test:

```python
def test_excusable_single_vm_known_flake(tmp_path: Path, monkeypatch):
    """T1: _VM_FAIL_RE extracts drv name from Cannot-build line; matches
    against drv_name, not test. Shape verified against /tmp/rio-dev/
    rio-p0209-impl-1.log:15780."""
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text(
        "error: Cannot build '/nix/store/"
        "lbb1v37c1dm9dmx0ghcy3zzjwk6kzywd-vm-test-run-rio-lifecycle-recovery.drv'.\n"
        "       Reason: builder failed with exit code 143.\n"
    )
    flakes = tmp_path / "known-flakes.jsonl"
    flakes.write_text(KnownFlake(
        test="vm-lifecycle-recovery-k3s",  # flake attr — NOT what the regex extracts
        drv_name="rio-lifecycle-recovery",  # nixosTest name — IS what the regex extracts
        symptom="s", root_cause="r", fix_owner="P0999", fix_description="d",
        retry="Once",
    ).model_dump_json() + "\n")
    monkeypatch.setattr(build, "KNOWN_FLAKES", flakes)
    v = build.excusable(log)
    assert v.excusable
    assert v.failing_tests == ["rio-lifecycle-recovery"]
    assert v.matched_flakes == ["rio-lifecycle-recovery"]


def test_excusable_vm_without_drv_name_not_matched(tmp_path: Path, monkeypatch):
    """T2: VM known-flake with drv_name=None is unmatchable by _VM_FAIL_RE —
    the model_validator should reject this at write time for vm-* tests, but
    if an old row slips through, excusable() correctly reports not-in-flakes."""
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text(
        "error: Cannot build '/nix/store/abc-vm-test-run-rio-cli.drv'.\n"
    )
    # Construct via dict to bypass the model_validator (testing excusable's
    # behavior on already-persisted bad data, not the validator):
    flakes = tmp_path / "known-flakes.jsonl"
    flakes.write_text(
        '{"test":"vm-cli-k3s","drv_name":null,"symptom":"s","root_cause":"r",'
        '"fix_owner":"P0999","fix_description":"d","retry":"Once","mitigations":[]}\n'
    )
    monkeypatch.setattr(build, "KNOWN_FLAKES", flakes)
    v = build.excusable(log)
    assert not v.excusable
    assert "not in known-flakes" in v.reason


def test_excusable_mixed_nextest_and_vm_rejects(tmp_path: Path, monkeypatch):
    """T1: nextest FAIL + VM Cannot-build in same log → 2 failures, not
    excusable. This is the masking risk P0304 T10's early-return design
    would miss without _VM_FAIL_RE."""
    from onibus import build
    log = tmp_path / "ci.log"
    log.write_text(
        "        FAIL [  1.0s] crate::a::real_bug\n"
        "error: Cannot build '/nix/store/abc-vm-test-run-rio-lifecycle-recovery.drv'.\n"
    )
    monkeypatch.setattr(build, "KNOWN_FLAKES", tmp_path / "empty.jsonl")
    v = build.excusable(log)
    assert not v.excusable
    assert "2 failures" in v.reason
    assert v.failing_tests == ["crate::a::real_bug", "rio-lifecycle-recovery"]


def test_knownflake_validator_vm_requires_drv_name():
    """T3: model_validator rejects vm-* test without drv_name."""
    import pytest
    with pytest.raises(ValueError, match="missing drv_name"):
        KnownFlake(
            test="vm-lifecycle-core-k3s", symptom="s", root_cause="r",
            fix_owner="P0999", fix_description="d", retry="Once",
        )
    # Sentinel bypasses:
    KnownFlake(
        test="<tcg-builder-allocation>", symptom="s", root_cause="r",
        fix_owner="P0179", fix_description="d", retry="Once",
    )
    # Nextest entries (no vm- prefix) don't need it:
    KnownFlake(
        test="crate::mod::test", symptom="s", root_cause="r",
        fix_owner="P0999", fix_description="d", retry="Once",
    )
```

### T7 — `docs:` fix `common.nix:131` false comment + forward-reference P0304 T10 re-scope

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) at `:130-132`. Current text:

```
# The KVM-DENIED-BUILDER marker lets `onibus flake excusable` pattern-match
# this as a known-flake and retry once (a KVM builder on retry passes).
```

This is false as-written (`excusable()` does not grep markers). After [P0304](plan-0304-trivial-batch-p0222-harness.md) T10 lands, it becomes **partially** true (T10's `_TCG_MARKERS` does grep). Rewrite to be correct both before and after:

```
# The KVM-DENIED-BUILDER marker is a log signature for `onibus flake
# excusable`'s _TCG_MARKERS supplementary grant (P0304 T10) — when the
# marker is present AND the only failure is a single VM drv (extracted by
# _VM_FAIL_RE, P0317), excusable=True even if that drv isn't in
# known-flakes.jsonl (TCG is infra-wide, not test-specific). The
# <tcg-builder-allocation> sentinel row in known-flakes.jsonl is the
# provenance/mitigations-history record, not a match key.
```

MODIFY [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md) — add a prepend note above T10's code block at `:159` (before the `_TCG_MARKERS` definition):

> **Sequenced after [P0317](plan-317-excusable-vm-regex-knownflake-schema.md):** P0317 T1 adds `_VM_FAIL_RE` which populates `failing` with VM drv names. Without that, this T10's early-return design masks co-occurring real failures (nextest FAIL + VM TCG in same `.#ci` run → 2 failures, but early-return grants excusability on marker alone). **Re-scope `_TCG_MARKERS` as a supplementary grant, not an early-return:** check it AFTER `failing` is computed, inside the `elif not matched:` branch — if `len(failing) == 1 and failing[0] in vm_fails and any(m in text for m in _TCG_MARKERS)`, override to `ok=True` with reason `"TCG marker present — builder-side infra, retry"`. This preserves the 1-failure-exactly discipline while granting the infra-always-excusable property.

Also MODIFY [`.claude/work/plan-0313-kvm-fast-fail-preamble.md`](plan-0313-kvm-fast-fail-preamble.md) at `:116` and [`.claude/work/plan-0315-kvm-ioctl-probe.md`](plan-0315-kvm-ioctl-probe.md) at `:53` — both reference the same false model. These are DONE plans (archaeology), so erratum brackets rather than rewrites:

- P0313 `:116`: append `[CORRECTION P0317: excusable() did NOT grep markers at this plan's merge — _NEXTEST_FAIL_RE only. P0317 T1 added _VM_FAIL_RE; P0304 T10 adds _TCG_MARKERS supplementary grant.]`
- P0315 `:53`: same erratum bracket

## Exit criteria

- `/nbr .#ci` green
- `grep -c '_VM_FAIL_RE' .claude/lib/onibus/build.py` → ≥2 (T1: defined + used)
- `python3 -c 'import re; assert re.search(r"^error: Cannot build ./nix/store/[a-z0-9]+-vm-test-run-([\w-]+)\.drv.", open("/tmp/rio-dev/rio-p0209-impl-1.log").read(), re.M).group(1) == "rio-lifecycle-recovery"'` — regex matches real log (T1; adjust log path at dispatch if p0209 logs are gone)
- `grep -c '"drv_name"' .claude/known-flakes.jsonl` → ≥6 (T2: 6 rows with non-null drv_name; sentinel row has `null`)
- `python3 -c 'from onibus.jsonl import read_jsonl; from onibus.models import KnownFlake; rows = read_jsonl(".claude/known-flakes.jsonl", KnownFlake); assert len([r for r in rows if r.test == "vm-lifecycle-recovery-k3s"]) == 1'` — duplicate resolved (T3)
- `grep '"<tcg-builder-allocation>"' .claude/known-flakes.jsonl` → 1 hit (T3: line 11 renamed to sentinel)
- `python3 -c 'from onibus.models import KnownFlake; KnownFlake(test="vm-foo-k3s", symptom="s", root_cause="r", fix_owner="P0001", fix_description="d", retry="Once")'` → raises ValueError mentioning `drv_name` (T3: validator)
- `grep 'class Mitigation' .claude/lib/onibus/models.py` → 1 hit (T4)
- `python3 -c 'from onibus.jsonl import read_jsonl; from onibus.models import KnownFlake; r = [x for x in read_jsonl(".claude/known-flakes.jsonl", KnownFlake) if x.test.startswith("<")][0]; assert len(r.mitigations) == 3; assert r.mitigations[0].plan == 313'` (T4: brackets migrated)
- `grep -c '\[P0313 LANDED\|P0315 LANDED\|P0316 LANDED' .claude/known-flakes.jsonl` → 0 (T4: bracket-strings gone from fix_description)
- `.claude/bin/onibus flake mitigation --help` → shows `test plan sha note` args (T5)
- `grep -c 'def test_excusable.*vm\|test_knownflake_validator' .claude/lib/test_onibus_dag.py` → ≥3 (T6: new tests)
- `python3 -m pytest .claude/lib/test_onibus_dag.py -k excusable -v` → all pass (T6)
- `grep 'excusable.*pattern-match' nix/tests/common.nix` → 0 hits (T7: false claim removed)
- `grep 'CORRECTION P0317' .claude/work/plan-0313-*.md .claude/work/plan-0315-*.md` → 2 hits (T7: archaeology errata)
- `grep 'Sequenced after.*P0317\|supplementary grant' .claude/work/plan-0304-*.md` → ≥1 hit (T7: T10 re-scope forward-reference)

## Tracey

No marker changes. `onibus` is harness tooling — no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:15`](../../.claude/lib/onibus/tracey.py). Same disposition as [P0313](plan-0313-kvm-fast-fail-preamble.md), [P0315](plan-0315-kvm-ioctl-probe.md), [P0316](plan-0316-qemu-force-accel-kvm.md): CI-robustness tooling, not product behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T1: _VM_FAIL_RE regex at :132 + excusable() dual-match logic :135-156"},
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T2: KnownFlake.drv_name field :138-148; T3: model_validator vm-requires-drv_name; T4: Mitigation model + KnownFlake.mitigations list"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T2: drv_name on 6 rows; T3: line 11 test → <tcg-builder-allocation> sentinel; T4: migrate 3 bracket-appends to mitigations list (also corrects -accel → -machine accel in P0316 note)"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T5: onibus flake mitigation verb at :357-375 + argparse :506"},
  {"path": ".claude/lib/test_onibus_dag.py", "action": "MODIFY", "note": "T6: 4 new tests — VM-regex match, drv_name-None unmatchable, mixed-failure rejection, model_validator"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T7: fix false pattern-match claim at :130-132"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T7: prepend re-scope note above T10's _TCG_MARKERS block at :159 — supplementary grant, not early-return"},
  {"path": ".claude/work/plan-0313-kvm-fast-fail-preamble.md", "action": "MODIFY", "note": "T7: CORRECTION bracket at :116 (DONE plan, archaeology erratum)"},
  {"path": ".claude/work/plan-0315-kvm-ioctl-probe.md", "action": "MODIFY", "note": "T7: CORRECTION bracket at :53 (DONE plan, archaeology erratum)"}
]
```

```
.claude/lib/onibus/
├── build.py              # T1: _VM_FAIL_RE + dual-match excusable()
├── models.py             # T2: drv_name  T3: validator  T4: Mitigation
└── cli.py                # T5: flake mitigation verb
.claude/lib/test_onibus_dag.py    # T6: 4 new tests
.claude/known-flakes.jsonl        # T2+T3+T4: drv_name + sentinel + mitigations
nix/tests/common.nix              # T7: :130-132 comment fix
.claude/work/plan-0304-*.md       # T7: T10 re-scope forward-reference
.claude/work/plan-031{3,5}-*.md   # T7: archaeology errata
```

## Dependencies

```json deps
{"deps": [316], "soft_deps": [304], "note": "P0316 merged — its bracket-append to known-flakes.jsonl:11 is the 4th mitigation to migrate (T4); also the consolidator's SEQUENCE AFTER P0316 MERGES caveat is now satisfied. Soft: P0304 T10 (_TCG_MARKERS) layers on top of T1's _VM_FAIL_RE — ship order is THIS plan first, then P0304 T10 becomes safe (supplementary grant rather than masking early-return). T7 edits P0304's plan doc to record this re-scope. discovered_from: bughunter mc21 + coordinator promotion + consolidator fold-in."}
```

**Depends on:** [P0316](plan-0316-qemu-force-accel-kvm.md) — DONE at [`900ac467`](https://github.com/search?q=900ac467&type=commits). The 4th bracket-append it made to [`known-flakes.jsonl:11`](../../.claude/known-flakes.jsonl) is T4's migration input. The consolidator's "SEQUENCE AFTER P0316 MERGES" note (active worktree at `/root/src/rio-build/p316`) is now moot — P0316 is merged.

**Soft-dep:** [P0304](plan-0304-trivial-batch-p0222-harness.md) T10 — `_TCG_MARKERS`. T10's current early-return design is **unsafe** without T1's `_VM_FAIL_RE` (masks co-occurring real failures). T7 here edits P0304's plan doc to re-scope T10 as a supplementary grant. If P0304 dispatches before this plan, T10's implementer MUST read the T7 note.

**Conflicts with:** [`build.py`](../../.claude/lib/onibus/build.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T10+T13 both touch it (T10: `_TCG_MARKERS` at `:129-155`; T13: `link=` param at `:38-115`). T1 here touches `:132-156`. T10 and T1 overlap exactly — **serialize: this plan first**. T13 is non-overlapping (`:38-115` vs T1's `:132+`). [`models.py`](../../.claude/lib/onibus/models.py) — P0304 T1 touches `:322` (`PlanFile.path` regex), [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) T2 touches `LockStatus` — both non-overlapping with `:138-167` (T2-T4 here, `KnownFlake` region). [`common.nix`](../../nix/tests/common.nix) — P0304 T15 (new, this docs batch) touches `:155-175` (kvmCheck refactor); T7 here touches `:130-132` (comment) — non-overlapping. [`known-flakes.jsonl`](../../.claude/known-flakes.jsonl) — no other UNIMPL plan touches it. Not in collisions top-30.
