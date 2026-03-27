# Plan 965363002: PSA restricted for rio-store + rio-system — runAsNonRoot, drop-ALL, seccomp

[P0454](plan-0454-four-namespace-helm-netpol-karpenter.md) set `rio-system` and `rio-store` to PSA `baseline` (down from `privileged`), narrowing the privileged label to the two executor namespaces. Review at [`values.yaml:45`](../../infra/helm/rio-build/values.yaml) flagged that `baseline` is still permissive — it allows root UID, unrestricted capabilities (except the ~8 that baseline drops), and no seccomp requirement. The control-plane pods (scheduler, gateway, controller, store) don't need any of that: they're vanilla gRPC servers with no FUSE, no mount, no raw sockets.

Moving both namespaces to PSA `restricted` means every pod must set `runAsNonRoot: true`, `capabilities.drop: [ALL]`, `allowPrivilegeEscalation: false`, and `seccompProfile: RuntimeDefault`. This is the K8s-recommended default for workloads that don't need privilege — the ADR-019 rationale ("control plane drops to baseline") stops short of the actual floor.

## Entry criteria

- [P0454](plan-0454-four-namespace-helm-netpol-karpenter.md) merged — `rio-system` + `rio-store` exist at PSA `baseline`

## Tasks

### T1 — `feat(helm):` namespaces PSA baseline→restricted for system+store

At [`values.yaml:44-45`](../../infra/helm/rio-build/values.yaml):

```yaml
namespaces:
  system:   {name: rio-system,   psa: restricted}   # was: baseline
  store:    {name: rio-store,    psa: restricted}   # was: baseline
  builders: {name: rio-builders, psa: privileged}
  fetchers: {name: rio-fetchers, psa: privileged}
```

Update the ADR-019 comment block at `:38-42` — "control plane + store drop to baseline" → "drop to restricted".

### T2 — `feat(helm):` securityContext on scheduler/gateway/controller/store Deployments

PSA `restricted` enforcement rejects pods that don't set the required fields. At each Deployment template:

- [`templates/scheduler.yaml`](../../infra/helm/rio-build/templates/scheduler.yaml)
- [`templates/gateway.yaml`](../../infra/helm/rio-build/templates/gateway.yaml)
- [`templates/controller.yaml`](../../infra/helm/rio-build/templates/controller.yaml)
- [`templates/store.yaml`](../../infra/helm/rio-build/templates/store.yaml)

Add pod-level `securityContext` + container-level `securityContext`:

```yaml
spec:
  template:
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532       # distroless nonroot UID
        runAsGroup: 65532
        fsGroup: 65532
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: ...
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
```

`runAsUser: 65532` is the distroless `nonroot` UID — verify each `rio-*` image's Dockerfile (or `nix/docker.nix` derivation) has a nonroot user. If images currently run as root, add a `USER 65532` directive or equivalent `config.User` in the nix derivation.

`readOnlyRootFilesystem: true` may need `emptyDir` volumes for any path the binary writes to (e.g., `/tmp`). Grep each crate's `main.rs` for `tempfile::` / `std::env::temp_dir` usage — if found, add an `emptyDir` at `/tmp`.

### T3 — `feat(nix):` docker.nix — set nonroot USER in control-plane images

At [`nix/docker.nix`](../../nix/docker.nix), the `rio-scheduler`/`rio-gateway`/`rio-controller`/`rio-store` image derivations: set `config.User = "65532:65532"` (or whatever the crate2nix/dockerTools pattern uses). Leave `rio-builder` image as-is — it runs in privileged namespaces and needs root for FUSE/overlay setup.

If the images use `pkgs.dockerTools.buildLayeredImage`, add:

```nix
config = {
  User = "65532:65532";
  # ... existing Cmd/Entrypoint
};
```

Verify the binary's working directories (config mounts, TLS cert mounts) are readable by UID 65532 — the ConfigMap/Secret volume mounts default to mode 0644 owned by root, which is fine for read. Writable paths (if any) need `fsGroup: 65532` on the pod spec (T2 already sets this).

### T4 — `test(vm):` PSA-restricted enforcement — security.nix subtest

At [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) (or wherever the existing PSA test lives — grep `pod-security` in `nix/tests/`), add a subtest that:

1. `kubectl get ns rio-system -o jsonpath='{.metadata.labels.pod-security\.kubernetes\.io/enforce}'` → `restricted`
2. Same for `rio-store`
3. Apply a probe pod to `rio-system` WITHOUT `runAsNonRoot: true` → MUST be **rejected** by admission with `violates PodSecurity "restricted:latest"` in the error
4. All four control-plane pods are `Running` (proves the securityContext satisfies restricted)

Add `# r[verify sec.psa.control-plane-restricted]` at the subtests wiring in [`nix/tests/default.nix`](../../nix/tests/default.nix).

## Exit criteria

- `/nixbuild .#ci` green
- `helm template infra/helm/rio-build | yq 'select(.kind=="Namespace" and .metadata.name=="rio-system") | .metadata.labels."pod-security.kubernetes.io/enforce"'` → `restricted`
- `helm template infra/helm/rio-build | yq 'select(.kind=="Namespace" and .metadata.name=="rio-store") | .metadata.labels."pod-security.kubernetes.io/enforce"'` → `restricted`
- `helm template infra/helm/rio-build | yq 'select(.kind=="Deployment" and .metadata.namespace=="rio-system") | .spec.template.spec.securityContext.runAsNonRoot'` → `true` for all (scheduler, gateway, controller)
- `helm template infra/helm/rio-build | yq 'select(.kind=="Deployment") | select(.spec.template.spec.containers[].securityContext.capabilities.drop[] == "ALL") | .metadata.name'` → includes all four control-plane deployments
- `nix build .#dockerImages.rio-scheduler && skopeo inspect --config docker-archive:result | jq -r .config.User` → `65532:65532` (or equivalent nonroot)
- On a deployed cluster: all four control-plane pods `Running`; `kubectl -n rio-system run root-probe --image=busybox --restart=Never` → rejected by admission

## Tracey

References existing markers:
- `r[sec.pod.host-users-false]` — adjacent security posture; this plan extends the K8s-level hardening to control-plane (which doesn't need hostUsers since it's not FUSE)

Adds new marker to security.md:
- `r[sec.psa.control-plane-restricted]` → [`docs/src/security.md`](../../docs/src/security.md) (see ## Spec additions below)

## Spec additions

Add to [`docs/src/security.md`](../../docs/src/security.md) after `r[sec.pod.fuse-device-plugin]` (`:77`):

```
r[sec.psa.control-plane-restricted]

The `rio-system` and `rio-store` namespaces MUST enforce Pod Security Admission `restricted`. Control-plane pods (scheduler, gateway, controller, store) set `runAsNonRoot: true`, `capabilities.drop: [ALL]`, `allowPrivilegeEscalation: false`, `seccompProfile: RuntimeDefault`, and `readOnlyRootFilesystem: true`. These are gRPC servers with no FUSE, no mount, no raw-socket requirements — `restricted` is the correct floor. The executor namespaces (`rio-builders`, `rio-fetchers`) stay at `privileged` per ADR-019; they need `CAP_SYS_ADMIN` for FUSE.
```

## Files

```json files
[
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T1: namespaces.system/store psa baseline→restricted + comment update"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "MODIFY", "note": "T2: pod+container securityContext (runAsNonRoot, drop-ALL, seccomp, readOnlyRootFilesystem)"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "MODIFY", "note": "T2: pod+container securityContext"},
  {"path": "infra/helm/rio-build/templates/controller.yaml", "action": "MODIFY", "note": "T2: pod+container securityContext"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "T2: pod+container securityContext"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T3: config.User=65532:65532 on scheduler/gateway/controller/store images"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T4: PSA-restricted enforcement subtest"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T4: r[verify sec.psa.control-plane-restricted] at subtests wiring"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "Spec additions: r[sec.psa.control-plane-restricted] marker"}
]
```

```
infra/helm/rio-build/
├── templates/
│   ├── scheduler.yaml            # T2: securityContext
│   ├── gateway.yaml              # T2: securityContext
│   ├── controller.yaml           # T2: securityContext
│   └── store.yaml                # T2: securityContext
└── values.yaml                   # T1: psa baseline→restricted
nix/
├── docker.nix                    # T3: User=65532
└── tests/
    ├── scenarios/security.nix    # T4: PSA-restricted subtest
    └── default.nix               # T4: r[verify] wiring
docs/src/
└── security.md                   # spec: r[sec.psa.control-plane-restricted]
```

## Dependencies

```json deps
{"deps": [454], "soft_deps": [965363001], "note": "P0454 set system+store to baseline; this tightens to restricted. Soft-dep P965363001: both touch values.yaml but non-overlapping sections (P965363001 at :496/:525 DS scheduling, this at :44-45 namespaces map). Sequence either way. discovered_from=454."}
```

**Depends on:** [P0454](plan-0454-four-namespace-helm-netpol-karpenter.md) — `rio-system`/`rio-store` exist at PSA `baseline`.
**Conflicts with:** [P965363001](plan-965363001-adr019-netpol-scheduling-hardening.md) touches `values.yaml` at `:496`/`:525` (DS scheduling) — non-overlapping with T1's `:44-45`. Both touch `nix/tests/default.nix` (subtests wiring) — additive `# r[verify]` lines, different markers.
