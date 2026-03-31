# Plan 991618102: disruption-drain internal 60s timeout tight under coverage-mode instrumentation

[P0512](plan-0512-manifest-reconcile-vm-test.md)'s coverage-full at mc=67 hit `controller never logged 'DisruptionTarget: DrainWorker force=true' within 60s on either node` at [`lifecycle.nix:2677-2680`](../../nix/tests/scenarios/lifecycle.nix). Subsequent merge-185 (P0513) same subtest GREEN — one-off, not persistent. The failure was in `vm-test-run-rio-lifecycle-core` (log: `/tmp/rio-dev/rio-sprint-1-merge-183.log:84614`).

Two compounding factors:
1. **P0512 added ~12s** to the scenario — the `manifest-pool` subtest (cold-start Job spawn + label checks + status patch waits) runs BEFORE `disruption-drain` in the core split's subtest list ([`default.nix:567`](../../nix/tests/default.nix) — `disruption-drain` is LAST). Timing shifts: the controller's watch loop now has more prior work competing for CPU.
2. **Coverage instrumentation slows everything.** [P0509](plan-0509-trace-id-propagation-timeout-flake.md) hit the same class of brittleness at the scenario-level `globalTimeout`; it fixed that with `+ common.covTimeoutHeadroom` (300s pad when `coverage=true`, [`common.nix:80`](../../nix/tests/common.nix)). But `covTimeoutHeadroom` applies at [`common.nix:121`](../../nix/tests/common.nix) `mkFragmentTest`'s `globalTimeout` — it does NOT reach the Python-side `deadline = time.time() + 60` loops INSIDE fragment strings.

The `60` at [`lifecycle.nix:2662`](../../nix/tests/scenarios/lifecycle.nix) is a hardcoded Python literal inside a Nix string. The scenario file receives `common` as an argument (`:150`) and `common.coverage` is re-exported (`common.nix:74`), so the fix is a Nix-side conditional interpolation.

There is a SECOND identical loop at [`:2692`](../../nix/tests/scenarios/lifecycle.nix) (scheduler `force-drain` grep) with the same `deadline = time.time() + 60`. Same fix.

## Entry criteria

- [P0512](plan-0512-manifest-reconcile-vm-test.md) merged (manifest-pool subtest exists — it's the timing-shift cause)

## Tasks

### T1 — `test(lifecycle):` disruption-drain internal wait-for-log timeouts coverage-aware

At [`lifecycle.nix:2662`](../../nix/tests/scenarios/lifecycle.nix) and [`:2692`](../../nix/tests/scenarios/lifecycle.nix), replace hardcoded `60` with a Nix-interpolated conditional. Define once at the top of the `disruption-drain` fragment (after `:2576`, inside the `''` string but as a Python variable so both loops share it):

```nix
disruption-drain = ''
  # ══════════════════════════════════════════════════════════════════
  # disruption-drain — pod eviction → DisruptionTarget → force=true
  # ══════════════════════════════════════════════════════════════════
  # ... existing header comment stays ...

  # P991618102: 60s was tight under coverage-mode instrumentation
  # slowdown (mc=67 merge-183 hit it post-P0512's ~12s manifest-pool
  # subtest; subsequent merge-185 green — one-off). covTimeoutHeadroom
  # (common.nix:80) pads globalTimeout but not INTERNAL Python
  # wait-loops. 120 under coverage gives the watcher's applied_objects
  # stream + gRPC RTT + JSON log flush the same 2× slack ratio.
  _drain_deadline_s = ${if common.coverage then "120" else "60"}

  with subtest("disruption-drain: eviction → DisruptionTarget → DrainWorker force=true"):
```

Then both `deadline = time.time() + 60` sites become `deadline = time.time() + _drain_deadline_s`. The assert messages at `:2679` and `:2708` reference the variable: `f"... within {_drain_deadline_s}s on either node"`.

**Why not a common.nix helper?** `covTimeoutHeadroom` is additive (600+300=900) — good for `globalTimeout` which scales with subtest count. Internal wait-for-log loops have a FIXED event-latency budget (watch event + gRPC RTT + log flush); doubling is the right shape, not adding 300s. A helper like `common.covInternalMult` (1 vs 2) would be over-engineering for two callsites; if a third appears, extract then.

### T2 — `test(tooling):` `onibus flake add` — register the known-flake entry

Per `## Known-flake entry` below. Run from the implementation worktree:

```bash
.claude/bin/onibus flake add \
  --test 'vm-lifecycle-core-k3s/disruption-drain' \
  --drv-name 'vm-test-run-rio-lifecycle-core' \
  --symptom 'controller never logged DisruptionTarget within 60s' \
  --root-cause 'coverage-mode instrumentation slowdown + P0512 manifest-pool timing shift' \
  --fix-owner 'P991618102' \
  --fix-description 'internal wait-for-log timeout 60→120s under coverage via Nix conditional' \
  --retry Once
```

Commit the resulting `known-flakes.jsonl` row alongside T1.

## Exit criteria

- `grep '_drain_deadline_s = \${if common.coverage' nix/tests/scenarios/lifecycle.nix` → 1 hit
- `grep 'deadline = time.time() + 60' nix/tests/scenarios/lifecycle.nix` → 0 hits in `disruption-drain` fragment (both `:2662` and `:2692` replaced)
- `grep 'deadline = time.time() + _drain_deadline_s' nix/tests/scenarios/lifecycle.nix` → 2 hits
- `nix build .#vm-lifecycle-core-k3s` green (non-coverage: `_drain_deadline_s = 60`, same behavior)
- `nix eval --raw '.#cov-vm-lifecycle-core-k3s.passthru.testScript' 2>/dev/null | grep '_drain_deadline_s = 120'` → 1 hit (coverage mode interpolates 120) — or equivalently inspect `.#checks.x86_64-linux.coverage-vm-lifecycle-core-k3s.drvPath`'s builder
- `grep 'vm-lifecycle-core-k3s/disruption-drain' .claude/known-flakes.jsonl` → 1 hit with `fix_owner` = this plan's real number

## Tracey

References existing marker:
- `r[ctrl.drain.disruption-target]` ([`controller.md:350`](../../docs/src/components/controller.md)) — T1 keeps the existing `r[verify]` at [`default.nix:562`](../../nix/tests/default.nix) valid; this is a test-timing hardening, not a behavior change. No new `r[verify]`, no bump.

## Known-flake entry

```json
{"test":"vm-lifecycle-core-k3s/disruption-drain","drv_name":"vm-test-run-rio-lifecycle-core","symptom":"controller never logged DisruptionTarget within 60s","root_cause":"coverage-mode instrumentation slowdown + P0512 manifest-pool timing shift","fix_owner":"P991618102","fix_description":"internal wait-for-log timeout 60→120s under coverage via Nix conditional","retry":"Once"}
```

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1: disruption-drain fragment :2576+ add _drain_deadline_s Nix-conditional; :2662+:2692 deadline use variable; :2679+:2708 assert messages f-string the variable"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T2: append disruption-drain row via onibus flake add"}
]
```

```
nix/tests/scenarios/
└── lifecycle.nix                  # T1: :2576 _drain_deadline_s + :2662,:2692 use it
.claude/
└── known-flakes.jsonl             # T2: flake entry
```

## Dependencies

```json deps
{"deps": [512], "soft_deps": [509], "note": "P0512 is the timing-shift cause (manifest-pool subtest adds ~12s before disruption-drain runs). P0509 is the pattern precedent (covTimeoutHeadroom for globalTimeout — this is the INTERNAL-timeout analogue). discovered_from=512 (coverage regression)."}
```

**Depends on:** [P0512](plan-0512-manifest-reconcile-vm-test.md) DONE — the manifest-pool subtest is the timing delta that tips 60s from safe to tight.
**Soft-deps:** [P0509](plan-0509-trace-id-propagation-timeout-flake.md) DONE — pattern precedent (`covTimeoutHeadroom`).
**Conflicts with:** `lifecycle.nix` count=28 (highest in top-20). P0295-T991618107 (this run's batch-append) edits `:2454,:2481,:2498` (scenario-header r[] markers — DIFFERENT fragment, `manifest-pool`). P0304-T991618104 (this run's batch-append) references `:2048,:2079,:2354,:2384` heredocs — also `manifest-pool`/`ephemeral-pool` fragments, not `disruption-drain`. All non-overlapping. No serialization needed within this plan run.
