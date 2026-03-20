# Plan 0239: VM PDB + Section H WPS lifecycle fragments

phase4c.md:40,45 — two VM test fragments added to [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix). (a) PDB ownership chain: `{pool}-pdb` exists, `maxUnavailable:1`, ownerRef→WorkerPool; delete WP → PDB GC'd. (b) Section H — WPS lifecycle: apply WPS CR → 3 children with correct sizeClass+ownerRef → delete WPS → `retry()` until children gone.

> **DISPATCH NOTE (rev-p239):** if wps-lifecycle needs scheduler gRPC
> verification (e.g., GetSizeClassStatus after children appear), use
> the existing `sched_grpc` helper at lifecycle.nix:400 — do NOT inline
> a 5th port-forward+grpcurlTls+trap block. [P0362] extracted this
> exactly to stop the copy-paste. The wps-lifecycle fragment runs in
> the SAME testScript scope, so `sched_grpc` is in scope.

**This plan carries `r[verify ctrl.wps.reconcile]` + `r[verify ctrl.wps.autoscale]`** — comments go at **col-0 in the .nix file header**, NOT inside the testScript string literal (per tracey-adoption memory: the parser doesn't see comments inside `''...''` strings).

**`lifecycle.nix` serialized after P0206+P0207.** Both 4b plans touch the `gc-sweep` subtest around `:1800` — different section than this plan's fragments (`:406` `fragments = {`), but same file. The fragment pattern (attrset keys) means adjacent additions don't conflict semantically, but git-level merges can still be noisy.

## Entry criteria

- [P0235](plan-0235-wps-main-wire-rbac-crd-regen.md) merged (WPS CRD yaml committed, controller has the reconciler wired, RBAC in place)
- [P0206](plan-0206-path-tenants-migration-upsert.md) merged (`lifecycle.nix` serialized — gc-sweep subtest extension at `:1800`)
- [P0207](plan-0207-mark-cte-tenant-retention.md) merged (`lifecycle.nix` serialized — same subtest)

## Tasks

### T1 — `test(vm):` PDB ownership fragment

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) — add fragment key `pdb-ownerref` in `fragments = {` at `:406`:

```nix
pdb-ownerref = ''
  with subtest("pdb-ownerref: PDB exists, owned by WorkerPool, GC'd on delete"):
      pool = "rio-worker-default"
      pdb = f"{pool}-pdb"

      # PDB exists with maxUnavailable: 1
      server.wait_until_succeeds(f"kubectl get pdb {pdb} -o jsonpath='{{.spec.maxUnavailable}}' | grep -q '^1$'")

      # ownerReferences[0] → WorkerPool
      owner_kind = server.succeed(f"kubectl get pdb {pdb} -o jsonpath='{{.metadata.ownerReferences[0].kind}}'").strip()
      assert owner_kind == "WorkerPool", f"expected ownerRef kind WorkerPool, got {owner_kind}"

      # Delete WP → PDB GC'd (ownerRef cascade)
      server.succeed(f"kubectl delete workerpool {pool}")
      server.wait_until_fails(f"kubectl get pdb {pdb}", timeout=60)
      print(f"pdb-ownerref PASS: {pdb} GC'd after {pool} delete")
'';
```

### T2 — `test(vm):` WPS lifecycle fragment

Add fragment key `wps-lifecycle` in the same `fragments = {` block:

```nix
wps-lifecycle = ''
  with subtest("wps-lifecycle: apply WPS → 3 children → delete → children gone"):
      # Apply a 3-class WPS
      server.succeed("""kubectl apply -f - <<'EOF'
  apiVersion: rio.build/v1alpha1
  kind: WorkerPoolSet
  metadata:
    name: test-wps
    namespace: default
  spec:
    classes:
      - {name: small, cutoffSecs: 60, resources: {requests: {cpu: "1", memory: 2Gi}}}
      - {name: medium, cutoffSecs: 300, resources: {requests: {cpu: "2", memory: 4Gi}}}
      - {name: large, cutoffSecs: 1800, resources: {requests: {cpu: "4", memory: 8Gi}}}
    poolTemplate:
      image: rio-worker:latest
  EOF
  """)

      # 3 child WorkerPools appear with correct sizeClass + ownerRef
      for cls in ["small", "medium", "large"]:
          child = f"test-wps-{cls}"
          server.wait_until_succeeds(f"kubectl get workerpool {child}", timeout=60)
          sc = server.succeed(f"kubectl get workerpool {child} -o jsonpath='{{.spec.sizeClass}}'").strip()
          assert sc == cls, f"expected sizeClass={cls}, got {sc}"
          owner = server.succeed(f"kubectl get workerpool {child} -o jsonpath='{{.metadata.ownerReferences[0].name}}'").strip()
          assert owner == "test-wps", f"expected ownerRef name=test-wps, got {owner}"

      # Delete WPS → children GC'd via finalizer cleanup + ownerRef
      server.succeed("kubectl delete workerpoolset test-wps")
      # retry() with default timeout — finalizer cleanup is explicit delete, should be fast
      for cls in ["small", "medium", "large"]:
          server.wait_until_fails(f"kubectl get workerpool test-wps-{cls}", timeout=60)

      print("wps-lifecycle PASS: 3 children created + GC'd")
'';
```

### T3 — `test(vm):` tracey r[verify] comments at file header

MODIFY the file header (col-0, BEFORE the `{`, per tracey-adoption memory):

```nix
# r[verify ctrl.wps.reconcile]
# r[verify ctrl.wps.autoscale]
# — wps-lifecycle fragment verifies: WPS apply → children with ownerRef;
#    delete → finalizer cleanup. The autoscale r[verify] here is the
#    end-to-end complement to P0234's unit test.
{
  ...existing file body...
```

### T4 — `test(vm):` helm-render CRD check

**Hidden dep at dispatch:** `grep workerpoolset nix/helm-render.nix` — if `helm-render.nix` doesn't include the WPS CRD, the test's `kubectl apply` fails with "no matches for kind WorkerPoolSet". Fix: either (a) add `infra/helm/crds/workerpoolset.yaml` to the helm-render source set, or (b) `kubectl apply -f /path/to/workerpoolset.yaml` in testScript setup before the subtest.

May need a separate `mkTest` variant (`vm-lifecycle-wps-k3s`) if the k3s-full fixture setup doesn't include CRD apply — check at dispatch.

## Exit criteria

- `/nbr .#ci` green — including `vm-lifecycle-*` tests. OR: documented in `.claude/notes/kvm-pending.md` as "still pending KVM fleet" (confirmatory-not-gating — same shape as P0311-T10/T15/T32)
- `pdb-ownerref` subtest: PDB exists, ownerRef→WorkerPool, GC'd on WP delete
- `wps-lifecycle` subtest: 3 children created with correct sizeClass + ownerRef; deleted on WPS delete
- `tracey query rule ctrl.wps.reconcile` shows verify (this plan completes the cross-plan verify from P0233)

## Tracey

References existing markers (added to component specs by [P0233](plan-0233-wps-child-builder-reconciler.md) + [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md)):
- `r[ctrl.wps.reconcile]` — `r[verify]` at file header (col-0, before `{`)
- `r[ctrl.wps.autoscale]` — `r[verify]` at file header (col-0, before `{`)

**Placement:** col-0 comments BEFORE the `{`, NOT inside testScript `''...''` literal. Per tracey-adoption memory: the .nix parser only sees col-0 comments before the attrset; inline comments inside string literals are doc-only.

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1+T2+T3: pdb-ownerref + wps-lifecycle fragments at :406 fragments block; r[verify ctrl.wps.*] comments at col-0 file header"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T4: separate mkTest variant (vm-lifecycle-wps-k3s) if needed — decide at dispatch based on helm-render CRD inclusion"}
]
```

```
nix/tests/
├── scenarios/lifecycle.nix   # T1-T3: 2 fragments + r[verify] header comments (count=6)
└── default.nix               # T4: mkTest variant if needed
```

## Dependencies

```json deps
{"deps": [235, 206, 207], "soft_deps": [], "note": "deps:[P0235(CRD installed+RBAC), P0206+P0207(lifecycle.nix serial — gc-sweep extensions)]. lifecycle.nix count=6; different section (:406 fragments vs :1800 gc-sweep)."}
```

**Depends on:** [P0235](plan-0235-wps-main-wire-rbac-crd-regen.md) — WPS CRD yaml committed, controller wired, RBAC in place. [P0206](plan-0206-path-tenants-migration-upsert.md) + [P0207](plan-0207-mark-cte-tenant-retention.md) — `lifecycle.nix` serialization (both 4b plans extend gc-sweep subtest at `:1800`; this plan adds at `:406` — different section, but same file).
**Conflicts with:** `lifecycle.nix` count=6 — fragment keys are attrset additions, independent lines. Soft conflict on `nix/tests/default.nix` with P0241/P0243 IF a new mkTest variant is needed.

**Hidden check at dispatch:** `grep workerpoolset nix/helm-render.nix` — if absent, either add the CRD yaml to helm-render OR kubectl apply in testScript setup.
