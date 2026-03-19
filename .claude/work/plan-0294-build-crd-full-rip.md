# Plan 0294: Build CRD full rip — ~1860 LoC feature deletion

**USER ESCALATION from retro P0116.** The original P0116 finding was "Build CRD single-node DAG, Phase-4 deferral is prose not a tagged TODO." The user's decision: **the feature isn't wanted.** K8s-native `kubectl apply -f build.yaml` builds landed in phase3a ([`64455fb8`](https://github.com/search?q=64455fb8&type=commits)) and were never asked for. SSH (`nix build --store ssh-ng://`) is THE submission path; the Build CR is an alternate path nobody uses.

**Actual LoC:** [`rio-controller/src/crds/build.rs`](../../rio-controller/src/crds/build.rs) = 238 lines, [`rio-controller/src/reconcilers/build.rs`](../../rio-controller/src/reconcilers/build.rs) = 1620 lines, [`infra/helm/crds/builds.rio.build.yaml`](../../infra/helm/crds/builds.rio.build.yaml) = 172 lines. ~2030 LoC incl. tests/wiring.

**P0238/P0270 already scope-shrunk at [`f8b2ef10`](https://github.com/search?q=f8b2ef10&type=commits):** [P0238](plan-0238-buildstatus-conditions-fields-deferred.md) now proto+scheduler only (`BuildEvent::InputsResolved` survives, CRD condition writes dropped); [P0270](plan-0270-buildstatus-critpath-workers.md) dashboard reads gRPC. Their `reconcilers/build.rs` touches are already noted as dead-after-P0294 in their bodies.

**P0289 sequencing:** the 09-doc `build-timeout` test fragment at `:628-645` uses `kubectl apply Build.spec.timeoutSeconds`. This plan's cancel-cgroup-kill retarget (T5) establishes the gRPC-direct pattern P0289 will inherit.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `refactor(controller):` DELETE crds/build.rs + reconcilers/build.rs

DELETE [`rio-controller/src/crds/build.rs`](../../rio-controller/src/crds/build.rs) (238 LoC).
DELETE [`rio-controller/src/reconcilers/build.rs`](../../rio-controller/src/reconcilers/build.rs) (1620 LoC).

### T2 — `refactor(controller):` unwire from mod.rs + lib.rs + main.rs

MODIFY [`rio-controller/src/crds/mod.rs`](../../rio-controller/src/crds/mod.rs) — delete `pub mod build;` at `:13`.

MODIFY [`rio-controller/src/reconcilers/mod.rs`](../../rio-controller/src/reconcilers/mod.rs):
- Delete `pub mod build;` at `:13`
- Delete the `// r[impl ctrl.build.watch-by-uid]` block at `:65-78` (watch-tasks DashMap, Build-specific)
- Delete Build-related `Ctx` fields (`:52` store_addr is Build-reconciler-only per its docstring; check if workerpool uses it)
- `:36-37` SubmitBuild/WatchBuild/CancelBuild client comment — check if AdminService uses same client; likely survives stripped

MODIFY [`rio-controller/src/lib.rs`](../../rio-controller/src/lib.rs):
- Delete `:3`, `:12`, `:26-29`, `:41`, `:52` Build-CRD doc comments
- Delete `:61` `pub use crds::build::{Build, BuildSpec, BuildStatus};`
- Delete `:78`, `:84` reconciler-label docstrings that mention `build|` (keep `workerpool`)
- Delete `:99-107` `rio_controller_build_watch_*` metric registrations

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs):
- Delete `:7` Build reconciler doc comment
- Delete `:21` `use rio_controller::crds::build::Build;`
- Delete `:23` `build` from `use rio_controller::reconcilers::{Ctx, build, workerpool};`
- Delete `:44` store-addr CLI arg (Build-reconciler-only per docstring) — **check**: if workerpool also uses store_addr, keep it
- Delete the `Controller::new(...).run(build::reconcile, ...)` future from the `tokio::join!`

### T3 — `refactor(infra):` DELETE CRD yaml + strip RBAC

DELETE [`infra/helm/crds/builds.rio.build.yaml`](../../infra/helm/crds/builds.rio.build.yaml) (172 LoC).

MODIFY [`infra/helm/rio-build/templates/rbac.yaml`](../../infra/helm/rio-build/templates/rbac.yaml):
- `:81` `resources: [workerpools, builds]` → `resources: [workerpools]`
- `:86` `resources: [workerpools/status, builds/status]` → `resources: [workerpools/status]`

MODIFY [`infra/helm/rio-build/templates/pdb.yaml`](../../infra/helm/rio-build/templates/pdb.yaml) at `:32` — the comment "Build CRD path still works, but most" is stale; update to reflect SSH is the only path.

### T4 — `docs:` remove Build CRD section + tracey markers from controller.md

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md):

- DELETE `:9` `r[ctrl.crd.build]` marker + the entire `### Build` YAML example block that follows
- DELETE `:177` "Build reconciler: create Build CRDs..." bullet
- DELETE `:287-333` entire `## Build CRD Lifecycle` section incl. `r[ctrl.build.sentinel]` (`:295`), `r[ctrl.build.watch-by-uid]` (`:298`), `r[ctrl.build.reconnect]` (`:301`), the finalizer paragraph (`:304`), and the `spec.timeoutSeconds >= 0` validation bullet (`:333`)

Run `tracey query validate` after — any `r[impl ctrl.crd.build]` / `r[impl ctrl.build.*]` annotations left in deleted .rs files are gone (files deleted), so no dangling refs. Grep for `r[verify ctrl.build` in `nix/tests/` — any hits are in `watch-dedup` which T5 deletes.

### T5 — `test:` retarget cancel-cgroup-kill, DELETE watch-dedup

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix):

**DELETE `watch-dedup`** (`:518-~700`, the `build-crd-flow` subtest block). Feature-specific — it tests reconciler UID-keyed DashMap dedup. Meaningless without the reconciler.

**RETARGET `cancel-cgroup-kill`** (`:878-~`). Current flow: delete Build CR → finalizer `cleanup()` → `CancelBuild` RPC → scheduler dispatches Cancel → worker `cgroup.kill()`. New flow: call `CancelBuild` via gRPC directly (grpcurl). Same scheduler→worker code path, different trigger.

```python
cancel-cgroup-kill = ''
  # ══════════════════════════════════════════════════════════════════
  # cancel-cgroup-kill — gRPC CancelBuild mid-exec → cgroup.kill="1"
  # ══════════════════════════════════════════════════════════════════
  # Post-P0294: Build CR is gone. Cancel via gRPC CancelBuild directly.
  # SAME scheduler→worker path: scheduler dispatches Cancel → runtime.rs
  # try_cancel_build → cgroup::kill_cgroup → fs::write(cgroup.kill,"1").
  # Log signal: "build cancelled via cgroup.kill" at runtime.rs:197.
  with subtest("cancel-cgroup-kill: gRPC CancelBuild → cgroup.kill"):
      # Submit via SubmitBuild (60s sleep so it's mid-flight).
      drv_path = client.succeed(
          "nix-instantiate --arg busybox '...' ${cancelDrv}"  # sleepSecs=60
      ).strip()
      client.succeed(f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}")
      submit = client.succeed(
          f"grpcurl -plaintext -d '{{"
          f'"dag":{{"nodes":[{{"drv_path":"{drv_path}"}}],"edges":[]}}}}'
          f"' k3s-server:9001 rio.scheduler.SchedulerService/SubmitBuild "
          f"| head -1"  # first event has build_id
      )
      build_id = json.loads(submit)['buildId']

      # Wait for cgroup to exist (build running).
      worker_vm = k3s_agent
      cgroup_path = worker_vm.wait_until_succeeds(
          "find /sys/fs/cgroup -type d -name '*lifecycle-cancel_drv' -print -quit",
          timeout=120,
      ).strip()

      # Cancel via gRPC — the replacement for "delete Build CR".
      client.succeed(
          f"grpcurl -plaintext -d '{{\"build_id\":\"{build_id}\"}}' "
          f"k3s-server:9001 rio.scheduler.SchedulerService/CancelBuild"
      )

      # Assert cgroup gone. Same assertions as before.
      worker_vm.wait_until_fails(f"test -d {cgroup_path}", timeout=30)
      # Log signal.
      worker_vm.wait_until_succeeds(
          "journalctl -u rio-worker --since='30 seconds ago' | "
          "grep -q 'build cancelled via cgroup.kill'",
          timeout=10,
      )
'';
```

Also delete any `# r[verify ctrl.build.*]` markers from the col-0 file-header block — those markers are being removed from the spec in T4.

### T6 — `refactor:` cluster-upgrade note for live deployments

NEW section in [`docs/src/components/controller.md`](../../docs/src/components/controller.md) where the Build CRD section was:

```markdown
## Build CRD (removed)

The `Build` CRD (`rio.build/v1alpha1 Build`) was removed in P0294. It was an
alternative K8s-native build submission path that duplicated the SSH
(`ssh-ng://`) flow. No known production users.

**Cluster upgrade:** existing `Build` CRs on running clusters are orphans
after controller upgrade (the reconciler no longer watches them). They can be
safely deleted: `kubectl delete builds.rio.build --all -A`. The CRD itself
remains installed (helm `crds/` directory is install-only, not upgrade-managed);
delete it manually: `kubectl delete crd builds.rio.build`.
```

## Exit criteria

- `/nbr .#ci` green — **includes the retargeted cancel-cgroup-kill**
- `test ! -f rio-controller/src/crds/build.rs`
- `test ! -f rio-controller/src/reconcilers/build.rs`
- `test ! -f infra/helm/crds/builds.rio.build.yaml`
- `grep -c 'Build' rio-controller/src/lib.rs rio-controller/src/main.rs` → drastically reduced (only "build" as common-English-word survives)
- `grep 'r\[ctrl\.crd\.build\]\|r\[ctrl\.build\.' docs/src/ -r` → 0 hits
- `tracey query validate` → 0 errors (no dangling `r[impl ctrl.build.*]` — source files deleted, markers deleted)
- `grep watch-dedup nix/tests/scenarios/lifecycle.nix` → 0 hits
- `grep 'kubectl apply.*Build\b' nix/tests/scenarios/lifecycle.nix` → 0 hits (cancel-cgroup-kill retargeted)

## Tracey

**Removes** markers from spec (T4):
- `r[ctrl.crd.build]` — deleted from `controller.md:9`
- `r[ctrl.build.sentinel]` — deleted from `controller.md:295`
- `r[ctrl.build.watch-by-uid]` — deleted from `controller.md:298`
- `r[ctrl.build.reconnect]` — deleted from `controller.md:301`

The `r[impl ctrl.crd.build]` + `r[impl ctrl.build.sentinel]` annotations at [`reconcilers/build.rs:5-6`](../../rio-controller/src/reconcilers/build.rs) and `r[impl ctrl.build.watch-by-uid]` at [`reconcilers/mod.rs:65`](../../rio-controller/src/reconcilers/mod.rs) go with the code (T1/T2). No dangling refs remain.

References existing markers (unchanged):
- `r[worker.cancel.cgroup-kill]` — T5 retarget preserves this verify (same scheduler→worker path, different trigger)

## Files

```json files
[
  {"path": "rio-controller/src/crds/build.rs", "action": "DELETE", "note": "T1: 238 LoC — Build CRD struct, status, spec"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "DELETE", "note": "T1: 1620 LoC — reconciler, drain_stream, watch task spawn"},
  {"path": "rio-controller/src/crds/mod.rs", "action": "MODIFY", "note": "T2: delete pub mod build"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T2: delete pub mod build + watch-by-uid DashMap + Build-only Ctx fields"},
  {"path": "rio-controller/src/lib.rs", "action": "MODIFY", "note": "T2: delete Build re-exports + doc comments + watch metrics"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T2: delete Build Controller::run from tokio::join!"},
  {"path": "infra/helm/crds/builds.rio.build.yaml", "action": "DELETE", "note": "T3: 172 LoC CRD yaml"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "T3: strip builds from resources at :81, :86"},
  {"path": "infra/helm/rio-build/templates/pdb.yaml", "action": "MODIFY", "note": "T3: fix stale 'Build CRD path' comment at :32"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4: DELETE r[ctrl.crd.build] + Build YAML block + Build CRD Lifecycle section + 4 markers; T6: 'Build CRD (removed)' tombstone"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T5: DELETE watch-dedup fragment; RETARGET cancel-cgroup-kill to gRPC CancelBuild; delete r[verify ctrl.build.*] from col-0 header"}
]
```

```
rio-controller/src/
├── crds/
│   ├── build.rs                  # T1: DELETE
│   └── mod.rs                    # T2: unwire
├── reconcilers/
│   ├── build.rs                  # T1: DELETE
│   └── mod.rs                    # T2: unwire
├── lib.rs                        # T2: unwire
└── main.rs                       # T2: unwire
infra/helm/
├── crds/builds.rio.build.yaml    # T3: DELETE
└── rio-build/templates/
    ├── rbac.yaml                 # T3: strip builds
    └── pdb.yaml                  # T3: comment fix
docs/src/components/controller.md # T4: remove section + markers; T6: tombstone
nix/tests/scenarios/lifecycle.nix # T5: DELETE watch-dedup, retarget cancel
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0116 ESCALATED — discovered_from=116. User: feature not wanted. ~2030 LoC deletion. P0238/P0270 already scope-shrunk (f8b2ef10). BLOCKS P0289 (cancel-cgroup-kill retarget pattern feeds the 09-doc build-timeout port). Removes 4 tracey markers. main.rs unwire: P0285 spawn-watcher slots into TRIMMED join!."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Blocks:** [P0289](plan-0289-port-specd-unlanded-test-trio.md) — the gRPC-direct retarget pattern established here (T5) feeds P0289's `build-timeout` port.
**Scope-shrunk upstream:** [P0238](plan-0238-buildstatus-conditions-fields-deferred.md) + [P0270](plan-0270-buildstatus-critpath-workers.md) already noted their CRD-write tasks dead-after-P0294 (f8b2ef10). If either merges before this plan, their `reconcilers/build.rs` edits land in a file this plan deletes — harmless (DELETE wins); if after, their Files fence is already trimmed.
**Conflicts with:** [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) also touched by [P0285](plan-0285-drainworker-disruptiontarget-watcher.md) (spawn watcher into same `tokio::join!`). Advisory-serial: P0294 first (larger diff, removes futures from the join); P0285's new future slots into the TRIMMED join. [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) also touched by P0285 (disruption-drain fragment) + P0289 (build-timeout + sigint fragments) — P0294 first (it DELETES watch-dedup; others only ADD). [`controller.md`](../../docs/src/components/controller.md) also touched by P0285, P0292, P0296 — all are different sections.
