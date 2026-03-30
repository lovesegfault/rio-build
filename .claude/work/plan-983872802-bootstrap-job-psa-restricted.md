# Plan 983872802: bootstrap-job.yaml — PSA-restricted securityContext (prod deploy blocker)

[P0460](plan-0460-psa-restricted-control-plane.md) tightens `rio-system`
to PSA `restricted` and adds `rio.podSecurityContext` /
`rio.containerSecurityContext` helpers to the four control-plane
Deployments. It misses one workload:
[`bootstrap-job.yaml:54`](../../infra/helm/rio-build/templates/bootstrap-job.yaml)
— the Helm `pre-install,pre-upgrade` hook Job that generates
`rio/hmac` + `rio/signing-key` secrets in AWS Secrets Manager.

The Job's pod spec has NO `securityContext` block (pod or container).
Under PSA `restricted`, admission rejects it: missing `runAsNonRoot`,
`allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`,
`seccompProfile`. First production `helm install` with
`bootstrap.enabled=true` fails at the pre-install hook — before any
other chart resource renders.

VM tests run with `bootstrap.enabled=false` (they pre-seed secrets
directly), so this path is invisible to `.#ci`. Discovered during
P0460 review by tracing all `namespace: {{ $ns }}` workloads in
`templates/`.

Secondary: [`nix/docker.nix:390`](../../nix/docker.nix) builds the
`rio-bootstrap` image with no `config.User` — it runs as root inside
the container. With `runAsNonRoot: true` on the pod, kubelet rejects
the image unless `runAsUser` is explicitly set.

## Entry criteria

- [P0460](plan-0460-psa-restricted-control-plane.md) merged (provides
  `rio.podSecurityContext` / `rio.containerSecurityContext` helpers in
  `_helpers.tpl`, sets PSA `restricted` label on `rio-system`)

## Tasks

### T1 — `fix(helm):` bootstrap-job securityContext via P0460 helpers

MODIFY
[`infra/helm/rio-build/templates/bootstrap-job.yaml`](../../infra/helm/rio-build/templates/bootstrap-job.yaml)
at `:54` (pod spec) and `:57` (container):

```yaml
    spec:
      serviceAccountName: rio-bootstrap
      restartPolicy: Never
      securityContext:
        {{- include "rio.podSecurityContext" . | nindent 8 }}
      containers:
        - name: bootstrap
          image: {{ include "rio.image" (list . $b.image) }}
          imagePullPolicy: {{ .Values.global.image.pullPolicy }}
          securityContext:
            {{- include "rio.containerSecurityContext" . | nindent 12 }}
```

Use the same helper includes P0460 adds to the four Deployments — no
inline duplication.

### T2 — `fix(docker):` rio-bootstrap image config.User = 65532

MODIFY [`nix/docker.nix:390`](../../nix/docker.nix) `bootstrap`
derivation — add `User = "65532:65532"` to the `config` block (same
UID as P0460 T3 sets on scheduler/gateway/controller/store images):

```nix
config = {
  Entrypoint = [ "${script}" ];
  User = "65532:65532";
  Env = [ ... ];
};
```

The bootstrap script's `mktemp -d` writes to `/tmp` (line 404 comment
already notes this) — verify `/tmp` is writable by 65532. If the base
image's `/tmp` is root-owned mode 0755, add an `extraCommands = "chmod
1777 tmp"` or equivalent.

### T3 — `test(helm):` helm-lint assert — bootstrap-job renders securityContext

MODIFY [`flake.nix:660`](../../flake.nix) `helm-lint` check — after
the existing `helm template` renders, add:

```bash
helm template rio . --set bootstrap.enabled=true --set global.image.tag=test \
  | yq 'select(.kind == "Job" and .metadata.name == "rio-bootstrap")' \
  | yq -e '.spec.template.spec.securityContext.runAsNonRoot == true'
helm template rio . --set bootstrap.enabled=true --set global.image.tag=test \
  | yq 'select(.kind == "Job" and .metadata.name == "rio-bootstrap")' \
  | yq -e '.spec.template.spec.containers[0].securityContext.capabilities.drop[0] == "ALL"'
```

This catches future helper drift without running the Job.

## Exit criteria

- `nix build .#checks.x86_64-linux.helm-lint` green with T3 asserts
- `helm template rio infra/helm/rio-build --set bootstrap.enabled=true | yq 'select(.kind=="Job") | .spec.template.spec.securityContext.runAsNonRoot'` → `true`
- `nix build .#dockerImages.rio-bootstrap && skopeo inspect --config docker-archive:result | jq -r .config.User` → `65532:65532`
- `/nixbuild .#ci` green

## Tracey

References existing markers:
- `r[sec.psa.control-plane-restricted]` — T1 extends the implementation
  to the last `rio-system` workload (at
  [`security.md:79`](../../docs/src/security.md)); T3 verifies

No new markers. The existing marker's "Control-plane pods
(scheduler, gateway, controller, store)" enumeration is non-exhaustive
prose — bootstrap-job is covered by the namespace-level "MUST enforce
PSA `restricted`" clause without a text change.

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/bootstrap-job.yaml", "action": "MODIFY", "note": "T1: add pod+container securityContext via rio.* helpers"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T2: rio-bootstrap config.User=65532:65532"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T3: helm-lint assert bootstrap-job securityContext renders"}
]
```

```
infra/helm/rio-build/templates/
└── bootstrap-job.yaml     # T1: securityContext includes
nix/
└── docker.nix             # T2: bootstrap User=65532
flake.nix                  # T3: helm-lint assert
```

## Dependencies

```json deps
{"deps": [460], "soft_deps": [], "note": "needs P0460's _helpers.tpl rio.podSecurityContext/rio.containerSecurityContext + rio-system PSA label"}
```

**Depends on:** [P0460](plan-0460-psa-restricted-control-plane.md) —
provides the helper templates T1 includes, and sets the PSA label that
makes T1 load-bearing. Without P0460, the Job is admitted under
`baseline` and T1 is a no-op hardening.

**Conflicts with:** P0460 itself touches `nix/docker.nix` (T3 sets
`config.User` on four images) and `flake.nix` helm-lint. Serialize
AFTER P0460 merge — this plan is a follow-up, not a concurrent branch.
