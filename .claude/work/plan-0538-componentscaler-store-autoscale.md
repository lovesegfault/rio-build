# Plan 0538: ComponentScaler — predictive store autoscaling with learned ratio

## Design

Store replica count is currently fixed (`store.replicas: 8`). Builders scale 0→500 ephemerally; store does not. Under-provisioned store causes the I-105 cascade (PG pool exhausts → builder FUSE blocks → circuit trip → all builds fail). Over-provisioned store wastes pods at idle.

**Mechanism:** new CRD `ComponentScaler` reconciled by rio-controller. Reconciler computes `desired_replicas` from a **predictive** signal (scheduler's `Σ(queued+running)` builder count, same `GetSizeClassStatus` RPC the builderpool reconciler already polls) divided by a **learned** `builders_per_replica` ratio. The ratio self-calibrates against an **observed** load signal (new `StoreAdminService.GetLoad` RPC returning `pg_pool_utilization`).

**Why not k8s HPA:** no Prometheus / metrics-server / KEDA / `custom.metrics.k8s.io` adapter exists in-cluster. HPA-on-custom-metrics would require deploying that stack first. Controller already has the demand signal and the reconcile-loop pattern.

**Why predictive over reactive:** the I-105→I-123 saga showed store saturation is a cliff, not a ramp. A reactive signal (inflight requests, p99 latency) measures the cliff after the fall. Scheduler knows N builders are about to exist *before* they exist; store can scale ahead of the burst. The cost is one assumption — `store_load ∝ builder_count` — which I-110's batching made true (~12 batch-RPCs/builder).

**Why learned ratio:** we don't know `builders_per_replica` precisely (empirically ~70 from 555-on-8), and it will drift as rio evolves. The ratio is EMA-adjusted against observed load and persisted in `.status.learnedRatio` — same pattern as the size-class estimator. Asymmetric correction: high load → immediate +1 *and* ratio decay (under-provisioning is dangerous); sustained low load → slow ratio growth (over-provisioning is cheap).

**Gateway out of scope:** gateway load ∝ connected `nix` clients, not builders. Signal architecture differs. CRD `spec.signal` enum (`scheduler-builders` | `self-reported`) reserves the extension point; second CR ships when multi-client load exists to test against.

### Reconcile loop (10s tick)

```
builders   = Σ(class.queued + class.running) over GetSizeClassStatus.classes
predicted  = ceil(builders / status.learnedRatio)
max_load   = max(GetLoad().pg_pool_utilization for pod in rio-store-headless endpoints)

if max_load > spec.loadThresholds.high:          # default 0.8
    desired = current + 1
    learnedRatio *= 0.95
    lowLoadTicks = 0
elif max_load < spec.loadThresholds.low:         # default 0.3
    lowLoadTicks += 1
    if lowLoadTicks >= 30: learnedRatio *= 1.02; lowLoadTicks = 0
    desired = predicted
else:
    desired = predicted; lowLoadTicks = 0

desired = clamp(desired, spec.replicas.min, spec.replicas.max)
if desired < current and (now - status.lastScaleUpTime) < 5m: desired = current
if desired < current: desired = current - 1      # max -1 per tick

patch apps/v1 Deployment {targetRef} /scale to desired
write .status: {learnedRatio, observedLoadFactor: max_load, desiredReplicas, lastScaleUpTime}
```

### Scale-down safety

I-125a (scopeguard cleanup on stream drop) + I-125b (adopt-on-conflict retry) make mid-PutPath termination *correct*. Mitigations make it *cheap*: 5-min down-stabilization window, max −1/tick, existing store PDB, store's existing SIGTERM grace period.

### CRD shape

```yaml
apiVersion: rio.build/v1
kind: ComponentScaler
metadata: {name: store, namespace: rio-store}
spec:
  targetRef: {kind: Deployment, name: rio-store}
  signal: scheduler-builders                 # enum; "self-reported" reserved for gateway
  replicas: {min: 2, max: 14}                # max=14: Aurora 16ACU≈2800 conns / 200 per replica
  seedRatio: 50.0
  loadEndpoint: rio-store-headless:9002      # where to poll GetLoad
  loadThresholds: {high: 0.8, low: 0.3}
status:
  learnedRatio: 67.3
  observedLoadFactor: 0.42
  desiredReplicas: 4
  lastScaleUpTime: "2026-04-03T..."
  lowLoadTicks: 12
```

## Stages (6 commits)

| | Commit | Scope |
|---|---|---|
| 1 | `feat(store): GetLoad RPC + rio_store_pg_pool_utilization gauge` | proto + store handler + describe_gauge + observability.md row |
| 2 | `feat(crds): ComponentScaler CRD` | rio-crds type + crdgen regen → infra/helm/crds/ |
| 3 | `feat(controller): ComponentScaler reconciler` | reconcilers/componentscaler/ + scaling/component.rs + store-admin client + controller.md spec |
| 4 | `feat(helm): ComponentScaler CR + conditional store replicas + RBAC` | templates/componentscaler.yaml; store.yaml omits .spec.replicas when enabled; rbac.yaml +deployments/scale; values.yaml block |
| 5 | `chore(xtask): enable componentScaler.store, drop store.replicas=8` | deploy.rs |
| 6 | `test(nix): componentscaler VM scenario` | nix/tests/componentscaler.nix + default.nix wiring |

Stages 1-2 are independent. Stage 3 depends on both. Stage 4 depends on 2. Stages 5-6 depend on 4.

## Files

```json files
[
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "add rpc GetLoad to StoreAdminService; GetLoadRequest/Response{pg_pool_utilization: float}"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "impl get_load: (pool.size()-pool.num_idle())/max_connections"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "describe_gauge rio_store_pg_pool_utilization"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "add rio_store_pg_pool_utilization + rio_controller_component_scaler_* rows; r[obs.metric.store-pg-pool]"},
  {"path": "rio-crds/src/componentscaler.rs", "action": "CREATE", "note": "#[derive(CustomResource)] ComponentScalerSpec/Status; signal enum"},
  {"path": "rio-crds/src/lib.rs", "action": "MODIFY", "note": "pub mod componentscaler + crdgen export"},
  {"path": "infra/helm/crds/componentscalers.rio.build.yaml", "action": "REGEN", "note": "via xtask regen crds"},
  {"path": "rio-controller/src/reconcilers/componentscaler/mod.rs", "action": "CREATE", "note": "reconcile loop; r[ctrl.scaler.component]"},
  {"path": "rio-controller/src/scaling/component.rs", "action": "CREATE", "note": "compute_desired + adjust_ratio; r[ctrl.scaler.ratio-learn]; unit tests"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "register componentscaler reconciler"},
  {"path": "rio-controller/src/lib.rs", "action": "MODIFY", "note": "describe rio_controller_component_scaler_{learned_ratio,desired_replicas,observed_load} gauges"},
  {"path": "rio-proto/src/client/mod.rs", "action": "MODIFY", "note": "StoreAdminClient helper if not present"},
  {"path": "infra/helm/rio-build/templates/componentscaler.yaml", "action": "CREATE", "note": "CR instance, gated on .Values.componentScaler.store.enabled"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "wrap .spec.replicas in {{if not componentScaler.store.enabled}}"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "controller ClusterRole: +apps/deployments[get,list,watch] +apps/deployments/scale[get,patch,update] +componentscalers CRD verbs"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "componentScaler.store.{enabled:false,min:2,max:14,seedRatio:50,loadThresholds}"},
  {"path": "xtask/src/k8s/eks/deploy.rs", "action": "MODIFY", "note": "drop .set(store.replicas,8); .set(componentScaler.store.enabled,true)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "ComponentScaler section; r[ctrl.scaler.component] r[ctrl.scaler.ratio-learn] r[store.admin.get-load]"},
  {"path": "nix/tests/componentscaler.nix", "action": "CREATE", "note": "submit N drvs, assert Deployment scaled >min within timeout"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "wire componentscaler scenario; # r[verify ctrl.scaler.component]"}
]
```

## Exit criteria

- [ ] `kubectl get componentscaler -n rio-store store -o jsonpath='{.status.learnedRatio}'` returns a float
- [ ] Submit 200 drvs from idle: store Deployment scales `2→N` (N>2) within 60s, *before* `rio_store_pg_pool_utilization` exceeds 0.8 on any replica
- [ ] After builds drain + 5min: store scales back toward `spec.replicas.min`
- [ ] Controller restart preserves `.status.learnedRatio` (no reset to seedRatio)
- [ ] `helm upgrade` does not fight controller (store Deployment `.spec.replicas` absent from rendered template when scaler enabled)
- [ ] `.#ci` green incl. new `vm-componentscaler` scenario
- [ ] `tracey query validate` clean; `r[ctrl.scaler.*]` + `r[store.admin.get-load]` + `r[obs.metric.store-pg-pool]` all have impl+verify

## Deps

None. Builds on sprint-1@c619e7ea.
