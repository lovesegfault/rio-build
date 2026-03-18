# Plan 0119: k3s pod-runtime bug-fix wave 1 — cgroup-ns/rustls/CRD-schema/imagePullPolicy/SSA/emptyDir/nixbld

## Design

P0118's first vm-phase3a run failed immediately — pod crash-looped with restart counter at 87 when logs were captured. Eleven commits fixed 8 distinct pod-runtime bugs, all documented in `phase3a.md` "Key Bugs Found." These are container-environment realities that unit tests cannot catch.

**cgroup namespace root** (`5586479`): containerd puts pods in cgroup namespaces by default. `/proc/self/cgroup` shows `0::/`; `/sys/fs/cgroup` is the namespaced view. `delegated_root()` took `.parent()` of `/sys/fs/cgroup` → `/sys/fs` (not a cgroup) → hard fail → crash-loop. Fix: detect `own_cgroup == /sys/fs/cgroup` and replicate what systemd `DelegateSubgroup=` does — create `/sys/fs/cgroup/leaf/`, move own PID there. The ns root is now empty → `subtree_control` enable-able → per-build cgroups siblings of `leaf/`. Writes stay INSIDE the container's cgroup subtree (safe — not touching host). NOT hostPath-mounting `/sys/fs/cgroup`: cgroupns is independent of mount ns; hostPath would show HOST root while `/proc/self/cgroup` still shows `0::/` — ns-root-detect would then clobber host systemd.

**rustls dual-provider panic** (`1ef1376`): kube→hyper-rustls enables `ring` feature; rio-proto→aws-sdk enables `aws-lc-rs`. Both active → rustls 0.23 can't auto-select → panic on first TLS (`kube::Client::try_default`). Fix: `install_default(aws-lc-rs)` as first line of `main`. Only rio-controller affected (only binary with both aws-sdk AND kube). **CRD schemars** (same commit): `#[schemars(with = "serde_json::Value")]` emits empty `{}` — apiserver REJECTS ("type: Required value"). All fields using k8s-openapi types (resources, tolerations, conditions) failed. Fix: `schema_with` fns emitting `type: object` + `x-kubernetes-preserve-unknown-fields: true`.

**imagePullPolicy** (`c5fe807`+`44b6135`): K8s defaults `:latest` to `Always`, which ignores locally-imported images. Airgap `ctr images import` → image IS present but kubelet ignores it → `ErrImagePull` loop. Controller-managed STS can't be kustomize-patched (operators patch the CRD, not generated STS), so `WorkerPoolSpec.image_pull_policy` field added. Also: docker tag `latest`→`dev`.

**SSA status patch** (`579ba8a`): server-side apply requires `apiVersion`+`kind` even for status subresource. `build.rs patch_status()` had them; `workerpool.rs` forgot. tower-test mock accepts any body so unit test passed; real apiserver 400s. Symptom: reconcile creates STS, then fails silently on status patch (`error_policy` logged at `debug!`, filtered by default INFO). `WorkerPool.status.readyReplicas` stays 0 forever; test hangs on `readyReplicas=1` wait. Also bumped `error_policy` log to `warn!` — silent 30s retry loop invisible at INFO.

**emptyDir for overlay** (`1a67235`): `RIO_OVERLAY_BASE_DIR=/var/rio/overlays` had no volume mount → upperdir/workdir landed on container's root fs — which is overlayfs (containerd's storage). Kernel requires upperdir to support `trusted.*` xattrs; overlayfs doesn't. Every overlay mount EINVAL. Pod went Ready (FUSE+cgroup+heartbeat fine), build dispatched, immediately failed at `OverlayMount::setup`. Fix: emptyDir volume (kubelet's disk, ext4/xfs, xattr standard).

**docker image dirs** (`5d92763`): daemon spawn's bind target `/nix/var/nix/db` + `/tmp` must exist; closure only populates `/nix/store`. Fix: `extraCommands` mkdir. **nixbld group** (`a31b304`): `getgrnam("nixbld")->gr_mem` reads explicit member list (4th `/etc/group` field), NOT primary-group cross-ref. Previous: `nixbld:x:30000:` (empty) → "no members" error. Fix: 8 `nixbld{1..8}` users + populated member list.

`e8b47d6` upgraded the test to a 2-drv DAG with real cgroup/prefetch assertions (vs. the P0118 leaf-only test). `d0a2196` added README. `c27b4c9` fixed a duplicate `cfg(test)` attr.

## Files

```json files
[
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "ns-root detection: /sys/fs/cgroup \u2192 create /leaf/, move-self; hostPath NOT the fix (cgroupns independent of mount ns)"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "rustls install_default(aws-lc-rs) first line"},
  {"path": "rio-controller/src/crds/mod.rs", "action": "MODIFY", "note": "schema_with any_object: type:object + x-kubernetes-preserve-unknown-fields"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "image_pull_policy field (controller-managed STS not kustomize-patchable)"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "status patch includes apiVersion+kind; error_policy warn not debug"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "emptyDir volume at RIO_OVERLAY_BASE_DIR; image_pull_policy threaded"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "tag dev not latest; extraCommands mkdir /nix/var/nix/db /tmp; 8 nixbld users + populated group member list"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "2-drv DAG; globalTimeout=600s; Delegate=yes on k3s service; imagePullPolicy in WorkerPool"},
  {"path": "infra/base/crds.yaml", "action": "MODIFY", "note": "regenerated for image_pull_policy + schema fixes"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[worker.cgroup.ns-root]`, `r[ctrl.rustls.install-default]`, `r[ctrl.crd.schema-preserve-unknown]`, `r[ctrl.sts.image-pull-policy]`, `r[ctrl.sts.empty-dir-overlay]`.

## Entry

- Depends on P0118: all bugs found by running the vm-phase3a test.
- Depends on P0109: cgroup-ns-root is a follow-on fix to `delegated_root()`.
- Depends on P0112: rustls/CRD-schema/SSA bugs are in the controller P0112 created.

## Exit

Merged as `c27b4c9..e8b47d6` (11 commits). `.#ci` green at merge — vm-phase3a passes end-to-end. All 8 bugs cataloged in `phase3a.md` "Key Bugs Found" with commit SHAs.
