# Plan 0457: VM test scenario — builder/fetcher split end-to-end

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) splits the monolithic `WorkerPool` into `BuilderPool` (airgapped, non-FOD only) + `FetcherPool` (egress-open, FOD only). P0452 lands the scheduler routing, P0453 the two reconcilers, P0454 the per-role NetworkPolicies. This plan adds the VM test that proves the whole chain end-to-end in k3s: FOD → fetcher pod, non-FOD → builder pod, builder airgap holds, fetcher egress open but IMDS-blocked.

This is the FIRST test that exercises both pool types in one fixture. The existing [`netpol.nix`](../../nix/tests/scenarios/netpol.nix) and [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) test the pre-split `WorkerPool` + squid-proxy model that ADR-019 deletes.

## Entry criteria

- [P0452](plan-0452-sched-dispatch-role-routing.md) merged — scheduler's `is_fixed_output()` → `Role::Fetcher`/`Role::Builder` routing
- [P0453](plan-0453-ctrl-builderpool-fetcherpool-reconcilers.md) merged — `BuilderPool`/`FetcherPool` CRDs + reconcilers + helm templates
- [P0454](plan-0454-netpol-per-role-builder-airgap-fetcher-egress.md) merged — `builder-egress` (airgap) + `fetcher-egress` (0.0.0.0/0 except RFC1918/link-local) NetworkPolicies

## Tasks

### T1 — `test(nix):` new scenario `nix/tests/scenarios/fetcher-split.nix`

New file, `pkgs.testers.runNixOSTest` shape matching [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix). k3s-full fixture with one `BuilderPool` (1 replica) + one `FetcherPool` (1 replica) via `extraValues`. Subtests:

1. **`dispatch-fod`** — submit a FOD (`drvs.fodFetch` or a new `fetchurl` tarball drv), grep scheduler logs for the assignment line, assert the assigned pod name matches `default-fetchers-0` (or whatever the FetcherPool STS ordinal naming is). Follow [`scheduling.nix:491-512`](../../nix/tests/scenarios/scheduling.nix)'s assigned-worker-resolution pattern.
2. **`dispatch-nonfod`** — submit a non-FOD depending on the FOD output (so the graph has both roles in one `nix-build`). Assert the non-FOD lands on `default-builders-0`. Both dispatch asserts together prove the routing is role-aware, not just "first available."
3. **`builder-airgap`** — `nsenter -n` into the builder pod's netns (pattern from [`netpol.nix:84-109`](../../nix/tests/scenarios/netpol.nix)), `curl --max-time 5` an external URL → rc≠0. **Positive control first** (same "proves-nothing guard" as netpol.nix:111-137): `nc -z` to `rio-scheduler` ClusterIP MUST succeed — otherwise the rc≠0 is vacuous.
4. **`fetcher-egress`** — `nsenter -n` into the fetcher pod, `curl` the local origin HTTP server (same `python -m http.server` on the k3s-server node as fod-proxy.nix:132-172) → rc==0. Proves fetcher egress is OPEN where builder's is closed.
5. **`fetcher-imds-blocked`** — from fetcher netns, `curl 169.254.169.254` → rc≠0. ADR-019:84's link-local deny inherited. Same WEAK-in-VM caveat as netpol.nix:176-178 (no IMDS listener in QEMU) — the `fetcher-egress` positive subtest above is the non-vacuous gate.
6. **`fetcher-node-dedicated`** — `kubectl get pod default-fetchers-0 -o jsonpath='{.spec.nodeName}'` vs builder pod's nodeName. In the 2-node k3s-full fixture (server+agent), assert they landed on DIFFERENT nodes if the taint/toleration is wired; or at minimum assert the fetcher pod HAS the `rio.build/fetcher=true` toleration in its spec. (Full Karpenter NodePool isolation isn't testable in k3s — document the gap.)

Build graph: reuse `drvs.fodFetch` for the FOD leaf, add a new `drvs.fodConsumer` (or inline) — a trivial non-FOD `derivation {}` that takes the FOD output as `src` and `cat $src > $out`. One `nix-build` of the consumer pulls both through the scheduler.

### T2 — `test(nix):` wire into `nix/tests/default.nix`

Add `vm-fetcher-split-k3s` attr after [`vm-netpol-k3s`](../../nix/tests/default.nix) (~line 644). Fixture: `k3sFull { extraValues = { "builderPool.enabled" = "true"; "fetcherPool.enabled" = "true"; "networkPolicy.enabled" = "true"; ... }; }` — exact helm value keys depend on what P0453/P0454 land.

`r[verify ...]` markers at the `subtests = [ ... ]` entries per CLAUDE.md VM-test placement rule (NOT in the scenario file header):

```nix
subtests = [
  # r[verify sched.dispatch.fod-to-fetcher]
  "dispatch-fod"
  "dispatch-nonfod"
  # r[verify builder.netpol.airgap]
  "builder-airgap"
  # r[verify fetcher.netpol.egress-open]
  "fetcher-egress"
  "fetcher-imds-blocked"
  # r[verify fetcher.node.dedicated]
  "fetcher-node-dedicated"
];
```

If the scenario doesn't use the `{ fragments, mkTest }` split pattern (it's a single-path test, not fanout-parallel), markers go in a comment block immediately above the `vm-fetcher-split-k3s = ...` line — same placement as [`vm-netpol-k3s` at :631-636](../../nix/tests/default.nix) doesn't use markers but `vm-lifecycle-core-k3s` at :448-475 does. Follow whichever P0454's netpol wiring used.

### T3 — `test(nix):` `drvs.fodConsumer` helper in `nix/tests/lib/derivations.nix`

New attr `fodConsumer` — a parameterized `.nix` file taking `--argstr fodPath` (the FOD's store path), non-FOD `derivation {}` that `cat $fodPath > $out`. Trivial — the point is forcing the scheduler to route two jobs with different roles from one client invocation.

### T4 — `feat(xtask):` smoke test grows a FOD step

[`xtask/src/k8s/eks/smoke.rs:33-53`](../../xtask/src/k8s/eks/smoke.rs) has a `TRIVIAL_EXPR` that's already a `builtin:fetchurl` FOD + raw consumer — so the smoke ALREADY exercises both roles once the split lands. Verify the existing `smoke_build("fast", ...)` at :74 still passes post-split (it should — scheduler routes the FOD half to fetchers transparently). If P0452/P0453 changed the deploy flow to require explicit FetcherPool creation, add a `step_fetcherpool_reconciled` alongside `step_workerpool_reconciled` at :340. k3s smoke at [`xtask/src/k8s/k3s/smoke.rs:30`](../../xtask/src/k8s/k3s/smoke.rs) delegates to the same `chaos::smoke_build` — one change covers both.

### T5 — `chore(tracey):` config.styx already covers default.nix

[`config.styx:74`](../../.config/tracey/config.styx) already has `nix/tests/default.nix` in `test_include`. No edit needed — the new `r[verify]` markers in T2 are picked up automatically. Verify via `tracey query rule sched.dispatch.fod-to-fetcher` post-merge.

## Exit criteria

- `/nixbuild .#ci` green — includes the new `vm-fetcher-split-k3s` test
- `nix build .#checks.x86_64-linux.vm-fetcher-split-k3s` runnable standalone
- `tracey query untested | grep -E 'sched.dispatch.fod-to-fetcher|builder.netpol.airgap|fetcher.netpol.egress-open|fetcher.node.dedicated'` → empty (all four markers now have `verify` annotations)
- `grep 'r\[verify sched.dispatch.fod-to-fetcher\]' nix/tests/default.nix` → ≥1 hit
- `grep 'vm-fetcher-split-k3s' nix/tests/default.nix` → ≥1 hit
- `cargo xtask deploy k3s --smoke` (or equivalent) → FOD step passes

## Tracey

New `verify` annotations (all spec markers already exist in [ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md)):
- `r[sched.dispatch.fod-to-fetcher]` → subtest `dispatch-fod` + `dispatch-nonfod`
- `r[builder.netpol.airgap]` → subtest `builder-airgap`
- `r[fetcher.netpol.egress-open]` → subtest `fetcher-egress` + `fetcher-imds-blocked`
- `r[fetcher.node.dedicated]` → subtest `fetcher-node-dedicated`

References (not newly verified, exercised indirectly):
- `r[ctrl.fetcherpool.reconcile]` — FetcherPool pod existing is a precondition; P0453's own tests cover reconcile directly
- `r[fetcher.sandbox.strict-seccomp]` — NOT tested here (would need a syscall-probe build; out of scope, note as followup)

## Files

```json files
[
  {"path": "nix/tests/scenarios/fetcher-split.nix", "action": "CREATE", "note": "T1: new scenario — 6 subtests covering dispatch routing + netpol per role + node dedication"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: vm-fetcher-split-k3s attr + r[verify] markers after vm-netpol-k3s (~:644)"},
  {"path": "nix/tests/lib/derivations.nix", "action": "MODIFY", "note": "T3: fodConsumer attr — trivial non-FOD consuming a FOD output"},
  {"path": "nix/tests/lib/derivations/fod-consumer.nix", "action": "CREATE", "note": "T3: parameterized drv file for fodConsumer"},
  {"path": "xtask/src/k8s/eks/smoke.rs", "action": "MODIFY", "note": "T4: step_fetcherpool_reconciled if P0453 requires explicit pool creation; otherwise no-op (TRIVIAL_EXPR already has FOD)"},
  {"path": "xtask/src/k8s/k3s/smoke.rs", "action": "MODIFY", "note": "T4: delegates to eks chaos — likely no-op unless k3s needs separate fetcherpool step"}
]
```

```
nix/tests/
├── default.nix                        # T2: vm-fetcher-split-k3s + r[verify] markers
├── scenarios/
│   └── fetcher-split.nix              # T1: new — 6 subtests
└── lib/
    ├── derivations.nix                # T3: fodConsumer attr
    └── derivations/
        └── fod-consumer.nix           # T3: new
xtask/src/k8s/
├── eks/smoke.rs                       # T4: step_fetcherpool_reconciled (conditional)
└── k3s/smoke.rs                       # T4: likely no-op
```

## Dependencies

```json deps
{"deps": [452, 453, 454], "soft_deps": [], "note": "Hard deps on the three implementation plans — scenario is meaningless without routing (452), pools (453), and NetworkPolicies (454). No soft deps."}
```

**Depends on:** [P0452](plan-0452-sched-dispatch-role-routing.md) (routing), [P0453](plan-0453-ctrl-builderpool-fetcherpool-reconcilers.md) (reconcilers), [P0454](plan-0454-netpol-per-role-builder-airgap-fetcher-egress.md) (NetworkPolicies). All three must be merged before this scenario can run — a missing piece means the fixture either doesn't deploy both pools, doesn't route by role, or doesn't enforce the netpol split.

**Conflicts with:** `nix/tests/default.nix` is HOT (every VM-test plan appends here) — T2 is append-only at ~:644, low merge risk. `nix/tests/lib/derivations.nix` is low-traffic. `xtask/src/k8s/eks/smoke.rs` may collide with P0453 if that plan also touches the smoke for `BuilderPool` rename — coordinate, but edits are likely adjacent-not-overlapping. None in collisions top-30.
