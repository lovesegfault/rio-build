# Plan 0202: Post-remediation VM stabilization — placeholder key, RollingUpdate overlap, TIME_WAIT port

## Design

VM flakes introduced or surfaced by the remediation fixes. Each is a "remediation X shifted timing enough to trigger flake Y" case.

**Placeholder key (`6da3676`):** remediations 04/20's retry loops changed gateway startup timing; the `sshKeySetup` in `k3s-full.nix` patched the Secret but the gateway had already read the empty one. Placeholder key in the initial Secret unblocks startup.

**RollingUpdate overlap (`933bbde`):** rem-11's drain grace period + RollingUpdate = brief window where old pod is NOT_SERVING but new pod isn't Ready yet. `BalancedChannel` sees zero healthy endpoints. Bumped `maxSurge` so new pod starts before old enters drain.

**TIME_WAIT port (`4409e7a`):** rem-08's lease timeout fix shifted scheduler restart timing; the old pod's port was in `TIME_WAIT` when the new pod started. `SO_REUSEADDR` in tonic listener config.

**Coverage timeout headroom (`b46c947`):** coverage-instrumented binaries are slower; `covTimeoutHeadroom` param adds +30s per scenario.

**Remaining (`b3ba9b8`, `d434068`, `15dbb62`):** `protocol.nix` subtest race; `scheduling.nix` FUSE test ordering; `lifecycle.nix` gc-sweep flake.

## Files

```json files
[
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "placeholder SSH key in initial Secret; maxSurge bump; SO_REUSEADDR"},
  {"path": "nix/tests/scenarios/protocol.nix", "action": "MODIFY", "note": "covTimeoutHeadroom; subtest race fix"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "FUSE test ordering"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "gc-sweep flake"}
]
```

## Tracey

No tracey markers — test stabilization.

## Entry

- Depends on P0181: rem-02 (the empty-refs e2e test in lifecycle.nix)
- Depends on P0201: VM perf (timing changes compound)

## Exit

Merged as `6da3676`, `933bbde`, `4409e7a`, `b3ba9b8`, `d434068`, `15dbb62`, `b46c947` (7 commits). `.#ci` green at >99% pass rate.
