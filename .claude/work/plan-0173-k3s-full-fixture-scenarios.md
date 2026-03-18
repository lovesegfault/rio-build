# Plan 0173: k3s-full fixture + scenarios — phase*.nix → scenarios/*.nix

## Design

The topology×scenario rearchitecture. Previous model: each `phase<N>.nix` was a monolithic test with inline systemd-unit deployment + inline test script. Problem: production uses the Helm chart in k8s pods; VM tests used systemd on bare NixOS. The "production uses pod path, VM tests use systemd" gap meant chart templates, RBAC, PDB, and leader-election were never exercised in CI.

**`fixtures/k3s-full.nix` (`a14107e`):** 2-node k3s running everything as pods from the Helm chart. Pattern from `nixpkgs/nixos/tests/k3s/multi-node.nix`: shared `tokenFile`, `role=server` + `clusterInit` on one, `role=agent` + `serverAddr` on the other. All images preloaded on BOTH (pods can land anywhere). `local-path-provisioner` kept for PG PVC. `waitReady` gates on the bitnami image (last to import at ~140MB). **First time gateway/scheduler/store/pdb/rbac/postgres-secret templates are applied in CI.** `scheduler.replicas=2` + `podAntiAffinity` spreads across server+agent → enables leader-election testing.

**`scenarios/*.nix` (`0de9e81`, `8bd55dd`, `ab276f6`):** protocol, scheduling, security, observability, lifecycle, leader-election. Each scenario takes a fixture as input; can run against standalone OR k3s-full.

**Coverage validation (`44e4b9c`):** deleted all 8 `phase*.nix` after lcov comparison proved zero regression for standalone topology. `phase1b-2c ∩ ¬scenario = 0 lines`; `scenario ∩ ¬phase1b-2c = 466 lines` (net gain: rio-nix opcode paths, HMAC, tenant-resolve). `phase3-4 ∩ ¬scenario = 5844 lines` deferred to lifecycle.nix k3s stabilization. All 7 `r[verify]` markers migrated.

**`values/vmtest-full.yaml`:** all rio services enabled; bitnami PG subchart with `image.tag=18.3.0` (matches `nix/docker-pulled.nix` FOD); inline chunk backend; NodePort gateway; plaintext (no cert-manager — phase4c).

**Fill gaps (`557ee9d`, `6d57e0d`):** standalone scenario fixes + `NOT_SERVING` + Build CRD flow coverage. **ci-fast/ci-slow split (`cf9c421`)** by fixture (k3s takes longer).

## Files

```json files
[
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "NEW", "note": "2-node k3s, helm chart, airgap images, waitReady gate"},
  {"path": "infra/helm/rio-build/values/vmtest-full.yaml", "action": "NEW", "note": "bitnami PG, inline chunks, NodePort, plaintext"},
  {"path": "nix/tests/scenarios/protocol.nix", "action": "NEW", "note": "opcode coverage + protocol-cold (airgapped builtin:fetchurl via VM-local http.server)"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "NEW", "note": "fanout, chain, sizeclass, FUSE"},
  {"path": "nix/tests/scenarios/security.nix", "action": "NEW", "note": "HMAC, tenant-resolve"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "NEW", "note": "metric scrapes, trace assertion"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "NEW", "note": "recovery, gc, autoscaler, Build CRD flow"},
  {"path": "nix/tests/scenarios/leader-election.nix", "action": "NEW", "note": "failover, graceful-release"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "fixture×scenario wiring layer"},
  {"path": "nix/docker-pulled.nix", "action": "NEW", "note": "bitnami PostgreSQL pullImage FOD for airgap k3s (10beee2)"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "MODIFY", "note": "{{- with $g.service.nodePort }} conditional"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "mkClientNode gatewayPort/gatewayUser params"},
  {"path": "nix/tests/phase1a.nix", "action": "DELETE", "note": "44e4b9c — deleted after lcov proves zero regression"},
  {"path": "nix/tests/phase1b.nix", "action": "DELETE", "note": "44e4b9c"},
  {"path": "nix/tests/phase2a.nix", "action": "DELETE", "note": "44e4b9c"},
  {"path": "nix/tests/phase2b.nix", "action": "DELETE", "note": "44e4b9c"},
  {"path": "nix/tests/phase2c.nix", "action": "DELETE", "note": "44e4b9c"},
  {"path": "nix/tests/phase3a.nix", "action": "DELETE", "note": "44e4b9c"},
  {"path": "nix/tests/phase3b.nix", "action": "DELETE", "note": "44e4b9c"},
  {"path": "nix/tests/phase4.nix", "action": "DELETE", "note": "44e4b9c"}
]
```

## Tracey

- `# r[verify worker.overlay.stacked-lower]` — `8bd55dd` (scheduling.nix)
- `# r[verify worker.ns.order]` — `8bd55dd`
- `# r[verify obs.metric.scheduler]` — `8bd55dd`
- `# r[verify obs.metric.worker]` — `8bd55dd`
- `# r[verify obs.metric.store]` — `8bd55dd`
- `# r[verify sec.boundary.grpc-hmac]` — `8bd55dd`
- `# r[verify sched.lease.k8s-lease]` — `ab276f6` (leader-election.nix)
- `# r[verify ctrl.build.watch-by-uid]` — `ab276f6`
- `# r[verify obs.metric.controller]` — `ab276f6`

9 `.nix` verify markers (all migrations from phase*.nix → scenarios/, enabled by P0167's tracey bump).

## Entry

- Depends on P0172: VM test lib extraction
- Depends on P0168: helm migration (k3s-full applies the chart)

## Exit

Merged as `10beee2`, `a14107e`, `0de9e81`, `44e44e4`, `8bd55dd`, `ab276f6`, `557ee9d`, `6d57e0d`, `441bae7`, `cf9c421`, `44e4b9c` (11 commits, -3624L phase tests). Scenario tests eval; k3s-full stabilization in P0176.
