# Plan 0235: WPS main.rs wire + RBAC + CRD regen

phase4c.md:34-35 — the WPS spine closeout. Three mechanical pieces: (a) third `Controller::new(wps_api).owns(...).run()` in `main.rs`, join with the other two; (b) RBAC `ClusterRole` additions for `workerpoolsets`/`status`/`finalizers`; (c) CRD regen — this commits the actual `infra/helm/crds/*.yaml` files, picking up BOTH [P0223](plan-0223-seccomp-localhost-profile.md)'s `seccomp_profile` field AND [P0232](plan-0232-wps-crd-struct-crdgen.md)'s WPS CRD.

**NEVER local `nix build`** — use `/nbr .#crds` or raw `nix-build-remote --no-nom --dev -- .#crds`. Per dev-workflow memory: 3 prior machine crashes. The `result` symlink feeds `./scripts/split-crds.sh result`.

**CRD regen conflict with P0223:** both plans regen. Whoever merges second re-runs. The regen is deterministic — re-running here picks up BOTH the seccomp field (P0223) and the WPS CRD (P0232). Expected diff: 2 CRD yaml files change.

## Entry criteria

- [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) merged (reconciler + autoscaler complete — main.rs wires a working reconciler)
- [P0212](plan-0212-gc-automation-cron.md) merged (`main.rs` serialized — gc_schedule task spawn in the same `join!` block)

## Tasks

### T1 — `feat(controller):` third Controller in main.rs

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) — near `:315-364` where the existing two controllers are joined:

```rust
let wps_api: Api<WorkerPoolSet> = Api::all(client.clone());
let wp_api: Api<WorkerPool> = Api::all(client.clone());

let wps_controller = Controller::new(wps_api, watcher::Config::default())
    .owns(wp_api, watcher::Config::default())  // watch child WorkerPools
    .run(
        reconcilers::workerpoolset::reconcile,
        reconcilers::workerpoolset::error_policy,
        ctx.clone(),
    );

// Before: join!(build_controller, wp_controller)
// After:
tokio::join!(build_controller, wp_controller, wps_controller);
// or: futures::future::join3(...)
```

Check what `join!` form P0212 uses (it adds `gc_schedule` to the same block). `tokio::join!` is variadic; `futures::join3` is explicit. Use whichever is already in place.

### T2 — `feat(infra):` RBAC ClusterRole additions

MODIFY [`infra/helm/rio-build/templates/rbac.yaml`](../../infra/helm/rio-build/templates/rbac.yaml) — add to the controller `ClusterRole` rules:

```yaml
- apiGroups: ["rio.build"]
  resources: ["workerpoolsets"]
  verbs: ["get", "list", "watch", "patch", "update"]
- apiGroups: ["rio.build"]
  resources: ["workerpoolsets/status"]
  verbs: ["get", "patch", "update"]
- apiGroups: ["rio.build"]
  resources: ["workerpoolsets/finalizers"]
  verbs: ["update"]
```

### T3 — `feat(infra):` CRD regen + commit

```bash
nix-build-remote --no-nom --dev -- .#crds
./scripts/split-crds.sh result
```

Commit `infra/helm/crds/workerpoolset.yaml` (NEW) + `infra/helm/crds/workerpool.yaml` (MODIFIED — seccomp_profile from P0223).

**On rebase against P0223:** re-run the two commands above. Deterministic regen; both changes appear.

### T4 — `docs:` close remaining WPS deferrals

MODIFY [`docs/src/capacity-planning.md`](../../docs/src/capacity-planning.md) — close `:64` WPS deferral.
MODIFY [`docs/src/decisions/015-size-class-routing.md`](../../docs/src/decisions/015-size-class-routing.md) — close `:54-58` WPS deferral.

## Exit criteria

- `/nbr .#ci` green
- Controller startup log shows 3 reconcilers running (grep for `WorkerPoolSet` in startup spans)
- `kubectl get crd workerpoolsets.rio.build` works in the VM test fixture (verified in P0239, but `kubectl apply -f infra/helm/crds/workerpoolset.yaml --dry-run=server` here proves schema validity)
- `infra/helm/crds/workerpoolset.yaml` committed (NEW)
- `infra/helm/crds/workerpool.yaml` diff shows `seccompProfile` field (from P0223 if merged)

## Tracey

No markers — wiring, not new behavior. Behavior markers landed in P0233/P0234.

## Files

```json files
[
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T1: third Controller::new(wps_api).owns(wp_api).run() + join3 near :315-364; serialized after P0212"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "T2: workerpoolsets + /status + /finalizers RBAC rules"},
  {"path": "infra/helm/crds/workerpoolset.yaml", "action": "NEW", "note": "T3: CRD regen output (WorkerPoolSet schema)"},
  {"path": "infra/helm/crds/workerpool.yaml", "action": "MODIFY", "note": "T3: CRD regen picks up P0223 seccomp_profile field"},
  {"path": "docs/src/capacity-planning.md", "action": "MODIFY", "note": "T4: close :64 WPS deferral"},
  {"path": "docs/src/decisions/015-size-class-routing.md", "action": "MODIFY", "note": "T4: close :54-58 WPS deferral"}
]
```

```
rio-controller/src/main.rs            # T1: 3rd controller + join (count=17)
infra/helm/
├── rio-build/templates/rbac.yaml     # T2: RBAC rules
└── crds/
    ├── workerpoolset.yaml            # T3 (NEW): regen
    └── workerpool.yaml               # T3: regen (seccomp from P0223)
docs/src/
├── capacity-planning.md              # T4: close :64
└── decisions/015-size-class-routing.md  # T4: close :54-58
```

## Dependencies

```json deps
{"deps": [234, 212], "soft_deps": [223], "note": "WPS spine closeout. deps:[P0234(module), P0212(main.rs serial — gc_schedule in same join!)]. CRD regen picks up P0223's seccomp field too — on rebase, re-run regen."}
```

**Depends on:** [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) — reconciler + autoscaler complete. [P0212](plan-0212-gc-automation-cron.md) — **main.rs serialization** (`gc_schedule` task spawn in the same `join!` block).
**Conflicts with:** `main.rs` count=17 — serialized after P0212 via dep. Single 4c touch. CRD regen yaml: soft conflict with P0223; deterministic, whoever merges second re-runs `nix-build-remote -- .#crds && ./scripts/split-crds.sh result`.
