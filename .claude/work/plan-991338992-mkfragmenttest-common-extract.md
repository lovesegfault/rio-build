# Plan 991338992: Extract mkFragmentTest/assertChains into common.nix — 3× 95%-identical

Consolidator finding (mc160). [`lifecycle.nix:2304-2341`](../../nix/tests/scenarios/lifecycle.nix), [`scheduling.nix:1238-1276`](../../nix/tests/scenarios/scheduling.nix), and [`leader-election.nix:488-506`](../../nix/tests/scenarios/leader-election.nix) each have a near-identical `mkTest` tail (`{ name, subtests, globalTimeout ? ... } → runNixOSTest { name = "rio-${scenario}-${name}"; globalTimeout = globalTimeout + common.covTimeoutHeadroom; inherit (fixture) nodes; testScript = prelude + concat fragments + collectCoverage }`) and two of the three have an `assertChains` let-binding that differs only in the specific `(before, after)` ordering constraints.

**Shape match:**
| Scenario | assertChains | mkTest body | Default timeout |
|---|---|---|---|
| lifecycle.nix:2304 | 2 rules (finalizer←autoscaler, ephemeral←finalizer) | identical | 900 |
| scheduling.nix:1238 | 3 rules (fuse-direct←fanout, fuse-slowpath←fanout, fuse-slowpath=LAST) | identical | 600 |
| leader-election.nix:488 | none (explicit comment :482-486 "no ordering assertion needed") | identical | 900 |

[P0334](plan-0334-covtimeoutheadroom-common-extract.md) already hoisted `covTimeoutHeadroom` into common.nix — same pattern one level up. The `mkTest` body is the next extraction target. ~30L in common.nix, -48L net across the three scenario files.

## Entry criteria

- [P0334](plan-0334-covtimeoutheadroom-common-extract.md) merged (`common.covTimeoutHeadroom` exists — the extracted `mkFragmentTest` references it)

## Tasks

### T1 — `refactor(nix):` add mkFragmentTest + mkAssertChains to common.nix

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) — add after the `covTimeoutHeadroom` export (near `:72` or wherever P0334 placed it):

```nix
# ── Fragment-test composition (lifecycle/scheduling/leader-election) ────
# mkFragmentTest builds a runNixOSTest from a scenario-local `prelude`
# (Python test-setup), a `fragments` attrset (name → `with subtest: ...`
# body string), and a `subtests` selection list. Eval-time ordering
# constraints go through `chains` (list of { before, after, msg } or
# { name, last = true, msg } — see mkAssertChains below).
#
# Three scenarios share this exact shape; before P0NNNN each had a
# verbatim ~20L let-binding. `scenario` prefixes the test name
# (`rio-lifecycle-full`, `rio-scheduling-fuse`, ...).
mkFragmentTest =
  { scenario, prelude, fragments, fixture
  , chains ? [ ]
  , defaultTimeout ? 600
  }:
  { name, subtests, globalTimeout ? defaultTimeout }:
  assert mkAssertChains scenario chains subtests;
  pkgs.testers.runNixOSTest {
    name = "rio-${scenario}-${name}";
    globalTimeout = globalTimeout + covTimeoutHeadroom;
    inherit (fixture) nodes;
    testScript = ''
      ${prelude}
      ${lib.concatMapStrings (s: fragments.${s} + "\n") subtests}
      ${collectCoverage fixture.pyNodeVars}
    '';
  };

# Chain assertions: each entry is either
#   { before = "a"; after = "b"; msg = "..."; }  → a must precede b
#   { name = "x"; last = true; msg = "..."; }    → x must be the last subtest
# Skipped if neither `before` nor `name` is in `subtests` (subset runs
# don't trip the chain). Both lifecycle's finalizer←autoscaler and
# scheduling's fuse-slowpath=LAST fit; leader-election has no chains
# (empty list).
mkAssertChains =
  scenario: chains: subtests:
  let
    idx = name: lib.lists.findFirstIndex (s: s == name) (-1) subtests;
    has = name: builtins.elem name subtests;
    last = builtins.elemAt subtests (builtins.length subtests - 1);
    checkOne =
      c:
      if c ? last then
        lib.assertMsg (
          !(has c.name) || last == c.name
        ) "${scenario}: ${c.msg}"
      else
        lib.assertMsg (
          !(has c.after) || (has c.before && idx c.before < idx c.after)
        ) "${scenario}: ${c.msg}";
  in
  builtins.all checkOne chains;
```

`mkFragmentTest` is curried: the scenario file partially applies `{ scenario, prelude, fragments, fixture, chains, defaultTimeout }` once and re-exports the resulting `{ name, subtests } → test` function — same call signature as before from `default.nix`'s perspective.

### T2 — `refactor(nix):` migrate lifecycle.nix to common.mkFragmentTest

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) at `:2300-2342`. Replace the `assertChains` + `mkTest` let-bindings with:

```nix
mkTest = common.mkFragmentTest {
  scenario = "lifecycle";
  inherit prelude fragments fixture;
  defaultTimeout = 900;
  chains = [
    {
      before = "autoscaler";
      after = "finalizer";
      msg = "finalizer requires autoscaler earlier (pod-1 reverse-ordinal coverage)";
    }
    {
      before = "finalizer";
      after = "ephemeral-pool";
      msg = "ephemeral-pool requires finalizer earlier (no STS workers stealing dispatch)";
    }
  ];
};
```

The per-chain `msg` text comes from the existing `assertMsg` strings at `:2313,:2319` (drop the `"lifecycle: "` prefix — `mkAssertChains` prepends `${scenario}:`). Keep the explanatory comments about v24/v25 regression + 10s tick above the `chains` list (not inside the attrset — nix comments inside list-of-attrsets are awkward to read).

### T3 — `refactor(nix):` migrate scheduling.nix to common.mkFragmentTest

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) at `:1234-1277`:

```nix
mkTest = common.mkFragmentTest {
  scenario = "scheduling";
  inherit prelude fragments fixture;
  defaultTimeout = 600;
  chains = [
    {
      before = "fanout";
      after = "fuse-direct";
      msg = "fuse-direct requires fanout earlier (FUSE cache state)";
    }
    {
      before = "fanout";
      after = "fuse-slowpath";
      msg = "fuse-slowpath requires fanout earlier (busybox in cache)";
    }
    {
      name = "fuse-slowpath";
      last = true;
      msg = "fuse-slowpath is destructive (cache rm) — must run LAST";
    }
  ];
};
```

### T4 — `refactor(nix):` migrate leader-election.nix to common.mkFragmentTest

MODIFY [`nix/tests/scenarios/leader-election.nix`](../../nix/tests/scenarios/leader-election.nix) at `:482-507`:

```nix
# graceful-release and failover both kill the leader. graceful-release
# leaves the cluster at 2/2 (waits for replacement); failover doesn't.
# If build-during-failover follows failover, its own buildprep handles
# the stabilization wait. No ordering assertion needed — fragments are
# independent at the Python level.
mkTest = common.mkFragmentTest {
  scenario = "leader-election";
  inherit prelude fragments fixture;
  defaultTimeout = 900;
  # chains = [] — default, see comment above
};
```

### T5 — `test(nix):` eval-time assert — mkAssertChains fires on violation

No file change — dispatch-time verification. After T1-T4 land, confirm the chain assertions still fire:

```bash
# Should FAIL with "lifecycle: finalizer requires autoscaler earlier ..."
nix eval .#checks.x86_64-linux.vm-lifecycle-full.driver --apply \
  'd: (import ./nix/tests/scenarios/lifecycle.nix { ... }).mkTest {
     name = "bad"; subtests = [ "finalizer" "autoscaler" ]; }'
```

If `mkAssertChains` silently passes a misordered list, T1's `checkOne` logic is wrong (likely `idx` returning `-1` for both → `-1 < -1 == false` but the `!(has c.after)` guard wrong-way). Fix and re-verify. The existing per-scenario `assertChains` implementations are the reference behavior.

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'mkFragmentTest\|mkAssertChains' nix/tests/common.nix` → ≥2 (T1: both helpers defined)
- `grep 'assertChains =\|mkTest =' nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/scheduling.nix nix/tests/scenarios/leader-election.nix | grep -v 'common.mkFragmentTest'` → 0 per-scenario `let`-binding definitions remaining (T2-T4: all migrated to `common.mkFragmentTest` call)
- `wc -l nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/scheduling.nix nix/tests/scenarios/leader-election.nix` → net -48L vs pre-change (rough — the `chains` list is verbose, so -30..-50 acceptable)
- T5: `nix eval .#...lifecycle... --apply '(...).mkTest { subtests = ["finalizer" "autoscaler"]; }'` → `error: lifecycle: finalizer requires autoscaler earlier` (T1 chain logic correct, same error shape as pre-refactor)
- **Refactor-only check:** `/nixbuild .#checks.x86_64-linux.vm-lifecycle-*` — drv-hash SHOULD change (nix hashes the scenario file body), test EXECUTION byte-identical (same `testScript` concatenation). If any lifecycle/scheduling/leader-election VM test that was green before goes red, T1's `mkFragmentTest` body diverges from the per-scenario originals — diff `testScript` output.

## Tracey

No new markers. Pure nix-test-infrastructure refactor. The existing `r[verify ...]` markers at [`nix/tests/default.nix`](../../nix/tests/default.nix) subtests entries are untouched — `mkFragmentTest` produces the same `runNixOSTest` call, same `testScript` body; tracey scans `default.nix` not scenario files (per [P0341](plan-0341-vmtest-marker-subtests-entry-convention.md) convention encoded at [`config.styx:15-21`](../../.config/tracey/config.styx)).

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: +mkFragmentTest (curried: scenario-locals → {name,subtests} → runNixOSTest) + mkAssertChains (before/after/last chain check) ~30L near :72 covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T2: :2300-2342 assertChains+mkTest let-bindings → common.mkFragmentTest call with chains=[finalizer←autoscaler, ephemeral←finalizer]"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T3: :1234-1277 assertChains+mkTest let-bindings → common.mkFragmentTest call with chains=[fuse-direct←fanout, fuse-slowpath←fanout, fuse-slowpath=LAST]"},
  {"path": "nix/tests/scenarios/leader-election.nix", "action": "MODIFY", "note": "T4: :482-507 mkTest let-binding → common.mkFragmentTest call, chains=[]"}
]
```

```
nix/tests/
├── common.nix                      # T1: +mkFragmentTest + mkAssertChains
└── scenarios/
    ├── lifecycle.nix               # T2: -36L → common.mkFragmentTest
    ├── scheduling.nix              # T3: -39L → common.mkFragmentTest
    └── leader-election.nix         # T4: -16L → common.mkFragmentTest
```

## Dependencies

```json deps
{"deps": [334], "soft_deps": [304, 311], "note": "discovered_from=consol-mc160. P0334 (DONE) hoisted covTimeoutHeadroom — mkFragmentTest references it. lifecycle.nix (count=40+) and scheduling.nix (count=20+) are HOT — but T2-T4 touch only the tail let-block (:2300+, :1234+, :488+), not fragment bodies. P0304-T109 (grpcurl_json_stream helper) touches lifecycle.nix preamble (:379) + scattered callsites (:434/:1198/:1492/:1715/:1864/:1912) — non-overlapping with T2's :2300+ tail. P0311-T11/T13/T14/T30/T38 all add fragments or subtests to scheduling.nix — fragment-body additions, not the mkTest tail; T3's tail refactor is orthogonal. Sequence: no strict ordering with P0304/P0311, all additive to different regions. CARE: P0304-T15 touches common.nix kvmCheck (:159-169) — T1 here adds ~30L elsewhere in the rec block, non-overlapping."}
```

**Depends on:** [P0334](plan-0334-covtimeoutheadroom-common-extract.md) — `common.covTimeoutHeadroom` exists.
**Soft-deps:** [P0304](plan-0304-trivial-batch-p0222-harness.md) T15/T109 touch `common.nix`/`lifecycle.nix` non-overlapping sections. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T11/T13/T14/T30/T38 add fragments to `scheduling.nix` body, not tail.
**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=40+ — T2 edits `:2300+` tail only; [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) count=20+ — T3 edits `:1234+` tail only; [`common.nix`](../../nix/tests/common.nix) — T1 adds after `covTimeoutHeadroom` (`:72` region). All tail-append or dedicated-region edits; rebase-clean against fragment-body touchers.
