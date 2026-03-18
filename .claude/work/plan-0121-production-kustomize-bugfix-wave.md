# Plan 0121: Production-kustomize bug-fix wave 2 — SSA field-manager conflicts + autoscaler races

## Design

P0119 fixed bugs that crashed the pod. This wave fixed bugs that made production deployments silently wrong — all masked by vm-phase3a's single-host, `min=1` topology. Twelve commits, found by deploying `deploy/overlays/prod/` to a real multi-node cluster and watching the autoscaler fight the reconciler.

**Reconciler/autoscaler SSA conflict** (`1e2cb0f`, CRITICAL): `build_statefulset` set `replicas=Some(min)` on every reconcile via SSA with `.force()`. The autoscaler uses a **different** field manager. When autoscaler scales the STS, `.owns(StatefulSet)` triggers a reconcile → reconciler reverts to `min`. Autoscaling was a no-op. Fix: GET the STS first. If it exists, pass `replicas=None` → k8s-openapi's `Serialize` omits the field → SSA releases ownership → autoscaler's value sticks. Initial create still sets `replicas=min`. Also fixed: `WorkerPoolStatus.desired_replicas` was always `min`; now reads actual `STS.spec.replicas`. vm-phase3a passed by coincidence (`min=1`, never scaled). `521a0db` added a VM test assertion that reconciler preserves STS replicas.

**store_addr derived wrong** (`1e2cb0f`, same commit): `build_container` derived `store_addr` via `.replace(":9001", ":9002")` — only changed the port. `deploy/base/controller.yaml` sets `RIO_SCHEDULER_ADDR=rio-scheduler:9001` and `RIO_STORE_ADDR=rio-store:9002` (different Service hostnames). Workers got `rio-scheduler:9002` → wrong hostname → startup fail. vm-phase3a passes by coincidence (both addrs use `control` hostname). Fix: thread `ctx.store_addr` through.

**Autoscaler SSA missing GVK** (`923e5f2`): same bug as P0119's `579ba8a` but in the autoscaler's replicas patch — `{"spec":{"replicas":N}}` missing `apiVersion`+`kind`. 400 on real apiserver. vm-phase3a used `kubectl scale` directly; this path never ran. Extracted `sts_replicas_patch()` with unit test asserting GVK present.

**Sentinel patch race** (`8371841`): Build reconciler spawned `drain_stream` BEFORE the sentinel patch. For a fast build (already cached): drain_stream receives `BuildStarted` → patches `{phase:Building, build_id:<uuid>}`; sentinel patch (still pending) overwrites with `{phase:Pending, build_id:"submitted"}`; build completes, CRD stuck at Pending. Fix: await sentinel patch FIRST (synchronous), then spawn.

**Lease rustls** (`4c2dd45`): same dual-provider panic as P0119, but in scheduler when `RIO_LEASE_NAME` is set (pulls in kube). `install_default()` first line. **Autoscaler skip-deleting** (`b3a994f`): autoscaler patched pools with `deletionTimestamp` set — racing the finalizer. Fix: filter out. **i32 clamp** (`8f91214`): `compute_desired` cast `u32 as i32` AFTER the max clamp — wraparound for large values. Clamp first. **kill_on_drop daemon** (`b551696`): cgroup error path left daemon running. **Dead "submitted" check** (`2358502`): `drain_stream` checked for sentinel that can't arrive there post-sentinel-first fix. Plus: `92666e0` (env var rename), `1ebc6b9` (gateway pname fallback for raw derivations), `107674f` (stale phase tag cleanup), `c14db29` (error_policy `warn!` not `debug!`).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "GET STS first, replicas=None if exists (SSA releases field); thread ctx.store_addr; error_policy warn"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "sts_replicas_patch with GVK; skip deletionTimestamp pools; clamp before i32 cast"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "await sentinel patch BEFORE spawn drain_stream; remove dead 'submitted' check"},
  {"path": "rio-controller/src/fixtures.rs", "action": "MODIFY", "note": "mock for GET-STS-first flow"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "rustls install_default for lease TLS path"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "kill_on_drop for cgroup error path"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "pname fallback to 'name' env var for raw derivations"},
  {"path": "infra/base/configmaps.yaml", "action": "MODIFY", "note": "RIO_PG_URL \u2192 RIO_DATABASE_URL"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "assert reconciler preserves STS replicas across reconcile"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[ctrl.reconcile.replicas-ssa-release]`, `r[ctrl.autoscale.ssa-gvk]`, `r[ctrl.build.sentinel-first]`. P0127 later removed false `r[verify ctrl.autoscale.skip-deleting]` (no deletionTimestamp test).

## Entry

- Depends on P0119: wave 2 follows wave 1; these bugs were masked by the wave-1 crash-loops.
- Depends on P0112: all fixes are in the controller P0112 created.
- Depends on P0114: lease rustls fix is in the lease code path.

## Exit

Merged as `92666e0..2358502` + `521a0db` (12 commits, non-contiguous). `.#ci` green at merge. `statefulset_replicas_omitted_when_none` asserts serialized JSON has NO replicas field when `None` (SSA semantics depend on field **absence**, not null). VM test asserts reconciler preserves STS replicas.
