# Plan 0254: CA cutoff metrics + VM scenario (MILESTONE CLAUSE 1)

End-to-end demonstration that CA early cutoff works. Submit a CA chain, complete it once, resubmit identical — assert `rio_scheduler_ca_cutoff_saves_total > 0` AND the second submit completes in <5s (vs ~30s normal build time).

**USER A10 impact:** [P0253](plan-0253-ca-resolution-dependentrealisations.md) is T0 — this VM test MUST include a CA-depends-on-CA chain (not just a single CA drv). If the VM shows `ca_cutoff_saves_total = 0`, do NOT mark milestone clause 1 complete — investigate whether resolution is the blocker.

## Entry criteria

- [P0252](plan-0252-ca-cutoff-propagate-skipped.md) merged (Skipped variant + cascade)
- [P0253](plan-0253-ca-resolution-dependentrealisations.md) merged (CA-on-CA resolution — USER A10)

## Tasks

### T1 — `feat(scheduler):` cutoff-saves metrics

MODIFY [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) — register:
- `rio_scheduler_ca_cutoff_saves_total` — counter, derivations skipped via cutoff (already incremented in P0252 T3)
- `rio_scheduler_ca_cutoff_seconds_saved` — gauge, sum of `ema_duration_secs` of skipped drvs

### T2 — `test(vm):` ca-cutoff scenario

NEW [`nix/tests/scenarios/ca-cutoff.nix`](../../nix/tests/scenarios/ca-cutoff.nix). Pattern from `nix/tests/lib/derivations/chain.nix` + `__contentAddressed = true;`:

```nix
# r[verify sched.ca.cutoff-propagate]
# (col-0 BEFORE the `{` — NOT in testScript literal per tracey-adoption)
{ fixture, ... }: {
  name = "ca-cutoff";
  testScript = ''
    start_all()
    scheduler.wait_for_unit("rio-scheduler")

    # Build 1: CA chain A→B→C (all __contentAddressed=true), completes normally.
    # Takes ~30s (each node is a sleep 10).
    gateway.succeed("nix build -f ${caChain} --store ssh-ng://localhost")

    # Prometheus scrape: record baseline saves count.
    before = int(scheduler.succeed(
        "curl -s localhost:9090/metrics | grep '^rio_scheduler_ca_cutoff_saves_total ' | awk '{print $2}'"
    ).strip() or "0")

    # Build 2: resubmit identical chain. A completes, hash matches,
    # B+C should Skipped without running.
    import time
    t0 = time.monotonic()
    gateway.succeed("nix build -f ${caChain} --store ssh-ng://localhost --rebuild")
    elapsed = time.monotonic() - t0

    after = int(scheduler.succeed(
        "curl -s localhost:9090/metrics | grep '^rio_scheduler_ca_cutoff_saves_total ' | awk '{print $2}'"
    ).strip())

    # B and C both skipped → saves ≥ 2
    assert after - before >= 2, f"expected ≥2 cutoff saves, got {after - before}"
    # A rebuilds (~10s), B+C skip (instant) → total <15s, not 30s
    assert elapsed < 15, f"second build took {elapsed}s, cutoff not working"
  '';
}
```

Where `caChain` is a NEW derivation fixture: 3-node CA-on-CA chain with `__contentAddressed = true;` on each, `outputHashMode = "recursive";`.

### T3 — `test(vm):` register scenario

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) — add `vm-ca-cutoff-standalone` (or whichever fixture is lightest).

### T4 — `docs:` observability + scheduler doc updates

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add `rio_scheduler_ca_cutoff_saves_total` / `rio_scheduler_ca_cutoff_seconds_saved` entries.
MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — document the cascade + Skipped variant.
MODIFY [`docs/src/data-flows.md`](../../docs/src/data-flows.md) — close the `:58` deferral block (CA cutoff was deferred there).

## Exit criteria

- `/nbr .#ci` green INCLUDING new `vm-ca-cutoff-*` test
- VM test asserts `ca_cutoff_saves_total ≥ 2` AND second-build elapsed < 15s
- **MILESTONE CLAUSE 1 ("CA early cutoff skips downstream") DEMONSTRATED**

## Tracey

References existing markers:
- `r[sched.ca.cutoff-propagate]` — T2 VM-verifies (col-0 header in `.nix`). Closes the cross-plan verify from [P0252](plan-0252-ca-cutoff-propagate-skipped.md).

## Files

```json files
[
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T1: register cutoff-saves metrics"},
  {"path": "nix/tests/scenarios/ca-cutoff.nix", "action": "NEW", "note": "T2: CA-on-CA chain VM demo"},
  {"path": "nix/tests/lib/derivations/ca-chain.nix", "action": "NEW", "note": "T2: 3-node CA fixture"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: register vm-ca-cutoff scenario"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: metric entries"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4: cascade + Skipped docs"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "T4: close :58 deferral"}
]
```

```
rio-scheduler/src/
└── lib.rs                        # T1: metric registration
nix/tests/
├── scenarios/ca-cutoff.nix       # T2: VM demo
├── lib/derivations/ca-chain.nix  # T2: fixture
└── default.nix                   # T3: register
docs/src/
├── observability.md              # T4
├── components/scheduler.md       # T4
└── data-flows.md                 # T4: close deferral
```

## Dependencies

```json deps
{"deps": [252, 253], "soft_deps": [268], "note": "USER A10: deps P0253 — VM demo MUST include CA-on-CA chain. default.nix SOFT-conflict with P0268 (chaos) — coordinator serializes dispatch, not dag dep (4c A9 pattern)."}
```

**Depends on:** [P0252](plan-0252-ca-cutoff-propagate-skipped.md) — Skipped cascade. [P0253](plan-0253-ca-resolution-dependentrealisations.md) — CA-on-CA resolution (USER A10: T0, chain must work).
**Conflicts with:** `default.nix` SOFT-conflict with [P0268](plan-0268-chaos-harness-toxiproxy.md) — both add one line. Coordinator serializes dispatch.
