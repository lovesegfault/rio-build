# Plan 0286: Privileged-worker hardening — device plugin + hostUsers:false + default flip

**Retro P0007 finding.** Eight phases later, the `privileged: true` deferral chain **terminates in a void**:

1. `privileged: true` is still the shipped default — [`infra/helm/rio-build/values.yaml:386`](../../infra/helm/rio-build/values.yaml)
2. [`values.yaml:382`](../../infra/helm/rio-build/values.yaml) comment claims "ADR-012 defers to phase 5" — **fabricated**; [ADR-012](../../docs/src/decisions/012-privileged-worker-pods.md) never mentions phase 5
3. `phase5.md` has no device-plugin/seccomp/privileged task
4. The "ADR-012 separate track" at [`rio-worker/src/cgroup.rs:304`](../../rio-worker/src/cgroup.rs) doesn't exist — no plan doc, no phase task, no DAG entry
5. [P0284](plan-0284-dashboard-docs-sweep-markers.md) T6 would retag `TODO(phase5)` → `TODO(adr-012)` — **relabeling into a tag with no schedule**
6. `phase4c.md:59` schedules `seccomp-rio-worker.json` — but [ADR-012:10](../../docs/src/decisions/012-privileged-worker-pods.md) confirms `privileged: true` "disables seccomp profiles entirely"; the profile would be **silently inert**
7. `hostUsers: false` ([ADR-012:25-32](../../docs/src/decisions/012-privileged-worker-pods.md) recommendation) unimplemented — no grep hits in `builders.rs`
8. **The kicker:** [P0284](plan-0284-dashboard-docs-sweep-markers.md) T4 removes `docs/src/introduction.md:50`'s "Multi-tenant deployments with untrusted tenants are unsafe before Phase 5" warning, while `privileged: true` (which [ADR-012:18](../../docs/src/decisions/012-privileged-worker-pods.md) calls "Unacceptable security posture for a multi-tenant build service") remains the default with no scheduled removal

**User decision: FULL scope.** Device plugin for `/dev/fuse`, `hostUsers:false`, `privileged:false` default flip. **This plan GATES P0284 T4** — P0284's `json deps` fence already has `286` added (at [`f8b2ef10`](https://github.com/search?q=f8b2ef10&type=commits)); this plan makes the dep entry real.

ADR-012's Phase-1a spike finding ([ADR-012:38-40](../../docs/src/decisions/012-privileged-worker-pods.md)) is the implementation roadmap: `hostUsers:false` + hostPath `/dev/fuse` is **incompatible** (kernel rejects idmap mounts on device nodes). A FUSE device plugin resolves BOTH: adds `/dev/fuse` to the device cgroup allowlist AND avoids hostPath.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(infra):` deploy smarter-device-manager as a DaemonSet

NEW [`infra/helm/rio-build/templates/device-plugin.yaml`](../../infra/helm/rio-build/templates/device-plugin.yaml) — `smarter-device-manager` DaemonSet + ConfigMap. Gated on `.Values.devicePlugin.enabled` (default `true`). Configured to expose `/dev/fuse` as a K8s extended resource `smarter-devices/fuse`.

```yaml
# ConfigMap data:
- devicematch: ^fuse$
  nummaxdevices: 100  # per-node; workers pull 1 each
```

Also: NodePool/nodeSelector taint tolerance — the DS runs on `rio.build/node-role: worker` nodes only (same selector as `values.yaml:395`).

### T2 — `feat(controller):` worker podspec requests FUSE via resources.limits, not hostPath

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs):

1. **Delete** the hostPath `/dev/fuse` volume + volumeMount (grep for `CharDevice` — per [ADR-012:54](../../docs/src/decisions/012-privileged-worker-pods.md), this is the current mechanism)
2. **Add** `resources.limits["smarter-devices/fuse"] = "1"` to the worker container spec — kubelet + device plugin inject `/dev/fuse` into the container
3. **Add** `hostUsers: Some(false)` to the pod spec — K8s 1.33+ user-namespace isolation (UID remap, [ADR-012:25-32](../../docs/src/decisions/012-privileged-worker-pods.md))
4. The existing granular-caps path at [`builders.rs:633-640`](../../rio-controller/src/reconcilers/workerpool/builders.rs) (`SYS_ADMIN` + `SYS_CHROOT` when `privileged != Some(true)`) already matches ADR-012 — it becomes the ONLY path once the values.yaml default flips

Gate all three changes on `wp.spec.privileged != Some(true)` — the escape hatch stays usable for k3s/kind clusters lacking the device plugin. When `privileged==true`, keep the hostPath fallback.

```rust
// r[impl sec.pod.host-users-false]
// r[impl sec.pod.fuse-device-plugin]
```

### T3 — `fix(infra):` flip values.yaml default + fix the fabricated comment

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml):

```yaml
  # Device cgroup: /dev/fuse is provided by smarter-device-manager
  # (templates/device-plugin.yaml) via resources.limits. With the
  # device plugin, granular SYS_ADMIN+SYS_CHROOT caps suffice and
  # hostUsers:false works (hostPath /dev/fuse breaks idmap mounts).
  #
  # privileged:true is an ESCAPE HATCH for k3s/kind without the
  # device plugin. It disables seccomp entirely (ADR-012:10) and
  # is unacceptable for multi-tenant (ADR-012:18). Production on
  # EKS/GKE with the device plugin deployed should NOT need this.
  privileged: false
```

The old comment's "ADR-012 defers to phase 5" fabrication dies here.

### T4 — `feat(infra):` custom seccomp profile (now that it won't be inert)

NEW [`infra/seccomp/rio-worker.json`](../../infra/seccomp/rio-worker.json) — custom profile allowing `mount`, `unshare`, `pivot_root`, `clone` with namespace flags, `setns` (the namespace syscalls the FUSE mount + overlayfs + nix-daemon sandbox need) plus the RuntimeDefault allowlist. [ADR-012:14](../../docs/src/decisions/012-privileged-worker-pods.md) specifies this shape.

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) at [`~:257`](../../rio-controller/src/reconcilers/workerpool/builders.rs) — swap `SeccompProfile { type_: "RuntimeDefault" }` → `SeccompProfile { type_: "Localhost", localhost_profile: Some("rio-worker.json") }`. The profile file must be present on the node at `/var/lib/kubelet/seccomp/profiles/rio-worker.json` — document in the helm `NOTES.txt` as a prereq.

**NOTE:** `phase4c.md:59` had this scheduled but it would have been inert. This is the real landing.

### T5 — `test:` VM verification — worker runs without privileged, FUSE works, seccomp active

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) — new fragment `privileged-hardening`:

```python
# r[verify sec.pod.host-users-false]
# r[verify sec.pod.fuse-device-plugin]
with subtest("privileged-hardening: worker runs non-privileged + hostUsers:false + seccomp active"):
    # 1. Pod spec: privileged absent/false, hostUsers=false
    spec = k3s_server.succeed(
        "k3s kubectl -n ${ns} get pod default-workers-0 -o json"
    )
    import json
    pod = json.loads(spec)
    sc = pod['spec']['containers'][0].get('securityContext', {})
    assert not sc.get('privileged', False), f"privileged still true: {sc}"
    assert pod['spec'].get('hostUsers') is False, "hostUsers not set to false"
    # 2. FUSE actually works (a build completes — the FUSE mount is the
    #    lower layer of every per-build overlay)
    #    — piggyback on an existing build fragment if possible
    # 3. Seccomp is Localhost/rio-worker.json, not RuntimeDefault
    assert sc.get('seccompProfile', {}).get('type') == 'Localhost'
    # 4. /proc/self/status in the worker shows Seccomp: 2 (filter mode)
    worker_vm.succeed(
        "k3s kubectl -n ${ns} exec default-workers-0 -- "
        "grep -q 'Seccomp:.*2' /proc/1/status"
    )
```

**k3s fixture caveat:** smarter-device-manager needs to be in the airgap image set. Add to `nix/tests/common.nix` image bundle.

### T6 — `docs:` update ADR-012 Implementation Note + P0284 T6 becomes no-op

MODIFY [`docs/src/decisions/012-privileged-worker-pods.md`](../../docs/src/decisions/012-privileged-worker-pods.md) — replace the `## Implementation Note (Phase 3a)` section ([`:50-57`](../../docs/src/decisions/012-privileged-worker-pods.md)) with a current-state note: device plugin deployed, `hostUsers:false` set, privileged default is `false`, escape hatch remains for k3s/kind.

MODIFY [`rio-worker/src/cgroup.rs`](../../rio-worker/src/cgroup.rs) at `:301` — delete the `TODO(phase5)` entirely (the "separate track" it referenced IS this plan). P0284 T6's retag-to-adr-012 becomes unnecessary — coordinate with P0284 implementer if both in flight.

## Exit criteria

- `/nbr .#ci` green — **includes** the security.nix VM fragment proving non-privileged FUSE works
- `grep privileged infra/helm/rio-build/values.yaml | grep -v '^#'` → `privileged: false`
- `grep hostUsers rio-controller/src/reconcilers/workerpool/builders.rs` → ≥1 hit setting `Some(false)`
- `grep hostPath rio-controller/src/reconcilers/workerpool/builders.rs | grep -i fuse` → 0 hits (except the privileged-escape-hatch branch if kept)
- `tracey query rule sec.pod.host-users-false` shows `impl` + `verify`
- [P0284](plan-0284-dashboard-docs-sweep-markers.md) T4 precondition satisfied — the `introduction.md:50` warning can come down

## Tracey

References existing markers:
- `r[ctrl.pdb.workers]` — T2 modifies the same `builders.rs` file; PDB logic unchanged
- `r[ctrl.crd.workerpool]` — T2 keeps `privileged: bool` CRD field (escape hatch); default changes downstream

Adds new markers to component specs:
- `r[sec.pod.host-users-false]` → `docs/src/security.md` (see ## Spec additions below)
- `r[sec.pod.fuse-device-plugin]` → `docs/src/security.md`

## Spec additions

New section in [`docs/src/security.md`](../../docs/src/security.md), inserted after the existing `r[sec.boundary.grpc-hmac]` marker:

```markdown
## Worker Pod Security

r[sec.pod.host-users-false]

Worker pods MUST set `hostUsers: false` to activate Kubernetes user-namespace
isolation (K8s 1.33+). Container UIDs are remapped to unprivileged host UIDs;
`CAP_SYS_ADMIN` applies only within the user namespace. A container escape
gaining `CAP_SYS_ADMIN` cannot affect the host or other pods. See ADR-012.

r[sec.pod.fuse-device-plugin]

Worker pods MUST obtain `/dev/fuse` via a device-plugin resource request
(`resources.limits["smarter-devices/fuse"]`), NOT a hostPath volume. The
hostPath mechanism is incompatible with `hostUsers: false` (kernel rejects
idmap mounts on device nodes — ADR-012 Phase 1a spike finding). The device
plugin adds `/dev/fuse` to the container's device cgroup allowlist without
the hostPath volume, enabling both `hostUsers: false` and the non-privileged
security context. `privileged: true` remains an escape hatch for k3s/kind
clusters lacking the device plugin; it MUST NOT be the production default.
```

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/device-plugin.yaml", "action": "NEW", "note": "T1: smarter-device-manager DaemonSet + ConfigMap"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T3: privileged default false, fix fabricated comment; T1: devicePlugin.enabled flag"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T2: delete hostPath /dev/fuse, add resources.limits fuse, add hostUsers:false; T4: seccomp Localhost"},
  {"path": "infra/seccomp/rio-worker.json", "action": "NEW", "note": "T4: custom seccomp profile (mount/unshare/pivot_root/clone+ns)"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T5: privileged-hardening VM fragment"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T5: add smarter-device-manager to airgap image bundle"},
  {"path": "docs/src/decisions/012-privileged-worker-pods.md", "action": "MODIFY", "note": "T6: replace Implementation Note section with current state"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "T6: delete TODO(phase5) at :301 — this plan IS the track"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "Spec additions: r[sec.pod.host-users-false] + r[sec.pod.fuse-device-plugin]"}
]
```

```
infra/
├── helm/rio-build/
│   ├── templates/device-plugin.yaml  # T1: NEW DaemonSet
│   └── values.yaml                    # T3: default flip
└── seccomp/rio-worker.json            # T4: NEW profile
rio-controller/src/reconcilers/workerpool/
└── builders.rs                        # T2+T4: hostUsers + device plugin + seccomp
rio-worker/src/cgroup.rs               # T6: delete TODO
nix/tests/
├── scenarios/security.nix             # T5: fragment
└── common.nix                         # T5: airgap image
docs/src/
├── decisions/012-privileged-worker-pods.md  # T6
└── security.md                        # spec additions
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0007 — discovered_from=7. ADR-012 track never scheduled; values.yaml:382 fabricates phase-5 deferral. GATES P0284 T4 — don't remove 'unsafe for multi-tenant' while privileged:true stays default. phase4c.md:59 seccomp JSON would be inert without this (privileged disables seccomp). builders.rs heavy: also touched by P0285 (annotation-only), P0296 (Job-mode new fn) — advisory-serial, each touches different sections."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Blocks:** [P0284](plan-0284-dashboard-docs-sweep-markers.md) T4 — `introduction.md:50` warning removal is gated on this plan's merge.
**Conflicts with:** [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) is touched by [P0285](plan-0285-drainworker-disruptiontarget-watcher.md) (T5 annotation-only, PDB docstring) and [P0296](plan-0296-ephemeral-builders-opt-in.md) (new Job-builder function, different section). Advisory-serial: P0286 first (largest diff in the shared securityContext block). [`security.nix`](../../nix/tests/scenarios/security.nix) low-traffic.
