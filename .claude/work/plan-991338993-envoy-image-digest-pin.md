# Plan 991338993: envoyImage digest-pin + helm-lint yq-loop over all third-party images

Consolidator finding (mc160). [`infra/helm/rio-build/values.yaml:407`](../../infra/helm/rio-build/values.yaml) defaults `dashboard.envoyImage` to `docker.io/envoyproxy/envoy:distroless-v1.37.1` — **bare tag, no digest**. Same `ImagePullBackOff` risk as [P0369](plan-0369-device-plugin-image-tag-fix.md)'s `devicePlugin.image` found: tag reuse / tag deletion / registry drift → EnvoyProxy Deployment pods go `ImagePullBackOff` → gRPC-Web translation dead → dashboard renders but every RPC fails.

**Digest is already known:** [`nix/docker-pulled.nix:94`](../../nix/docker-pulled.nix) pins `envoy-distroless` at `sha256:4d9226b9fd4d1449887de7cde785beb24b12e47d6e79021dec3c79e362609432` for the airgap VM-test set. Same image, same tag. T1 reuses that digest verbatim — no `crane digest` fetch needed.

**Generalization:** P0369-T2 added a helm-lint yq assert for `devicePlugin.image` digest-pinned at [`flake.nix:503-508`](../../flake.nix). That's one image. consol-mc160 suggests the yq check should loop over **all** rendered third-party images (anything not `{{ .Values.global.image.repository }}:{{ .Values.global.image.tag }}` — those are rio-build's own images, built by `nix/docker.nix`, tag = git SHA, no digest needed). Envoy, smarter-device-manager, and any future third-party dependency.

## Entry criteria

- [P0369](plan-0369-device-plugin-image-tag-fix.md) merged ([`flake.nix:494-508`](../../flake.nix) devicePlugin helm-lint digest-pin assert exists — T2 here generalizes it)

## Tasks

### T1 — `fix(infra):` values.yaml dashboard.envoyImage — add digest pin

MODIFY [`infra/helm/rio-build/values.yaml:407`](../../infra/helm/rio-build/values.yaml):

```yaml
# Before:
envoyImage: docker.io/envoyproxy/envoy:distroless-v1.37.1

# After:
envoyImage: docker.io/envoyproxy/envoy:distroless-v1.37.1@sha256:4d9226b9fd4d1449887de7cde785beb24b12e47d6e79021dec3c79e362609432
```

Digest sourced from [`nix/docker-pulled.nix:94`](../../nix/docker-pulled.nix). Update the comment at `:404-406` to match the `devicePlugin.image` comment style at `:422-424` (digest-pinned, bump via `skopeo inspect` / `crane digest`, tag cosmetic):

```yaml
# Envoy data-plane image. The operator defaults to a compiled-in
# version (v1.7.1 → envoy:distroless-v1.37.1); digest-pinned here so
# airgapped VM tests and private-registry prod can override. Bump via
# `crane digest docker.io/envoyproxy/envoy:distroless-v<new>` on
# upstream releases. Tag portion is cosmetic (kubelet pulls by digest).
# Kept in sync with nix/docker-pulled.nix:94 (same digest, airgap set).
```

### T2 — `test(infra):` helm-lint — loop digest-pin check over all third-party images

MODIFY [`flake.nix`](../../flake.nix) at the helm-lint check `:494-508` (the existing devicePlugin block from P0369-T2). Replace the single-image check with a loop that extracts all `.spec.template.spec.containers[].image` from the default-render and filters OUT rio-build's own images (they're tagged with `{{ .Values.global.image.tag }}` → set to `test` in the template invocation at `:490`):

```nix
# ── Third-party image digest-pin enforcement ────────────────────────
# Every image that isn't a rio-build image (those get `:test` from
# --set global.image.tag=test above) MUST be digest-pinned. A
# floating third-party tag that doesn't exist / gets deleted /
# gets overwritten upstream → ImagePullBackOff → component-specific
# silent brick:
#   - devicePlugin: smarter-devices/fuse never registers → worker
#     pods Pending (P0369 found v1.20.15 was never published)
#   - envoyImage: gRPC-Web translation dead → dashboard loads but
#     every RPC fails (P0NNNN)
#   - <future>: same failure mode, this loop catches it pre-merge
#
# yq drills into every container spec (DaemonSet + Deployment +
# StatefulSet), filters out :test-tagged rio images, and fails on
# any remaining bare-tag image. `@sha256:` is the pin marker.
thirdparty=$(yq eval-all '
  select(.kind=="DaemonSet" or .kind=="Deployment" or .kind=="StatefulSet")
  | .spec.template.spec.containers[].image
' /tmp/default.yaml | grep -v ':test$' | sort -u)
echo "third-party images in default render:" >&2
echo "$thirdparty" >&2
bad=$(echo "$thirdparty" | grep -v '@sha256:' || true)
if [ -n "$bad" ]; then
  echo "FAIL: third-party image(s) not digest-pinned:" >&2
  echo "$bad" >&2
  exit 1
fi
```

**CARE:** the `postgresql` subchart may inject a bitnami image — if so, either (a) exclude `bitnami/` from the `thirdparty` set (subchart is nixhelm-managed, not a rio-build values.yaml default), OR (b) add `postgresql.image.digest` to values.yaml too (stricter). Check at dispatch: `helm template ... | yq '.spec.template.spec.containers[].image'` on the default render to see the full image set. If bitnami appears, prefer (a) — subchart dependencies are a different supply-chain boundary.

### T3 — `docs(infra):` values.yaml — cross-reference digest-sync with docker-pulled.nix

The `:404-406` comment already mentions "pin here so airgapped VM tests and private-registry prod can override". T1's updated comment adds the explicit cross-reference to `docker-pulled.nix:94`. No separate task — T1 covers this.

Instead, T3: MODIFY [`nix/docker-pulled.nix:87-100`](../../nix/docker-pulled.nix) to add a back-pointer comment at the `envoy-distroless` entry:

```nix
# Envoy data-plane (distroless). v1.37.1 is the compiled-in default
# for gateway-helm v1.7.1 (api/v1alpha1/shared_types.go
# DefaultEnvoyProxyImage). The rio chart's dashboard.envoyImage pins
# the SAME digest (infra/helm/rio-build/values.yaml:407) — bump both
# together. The helm-lint check enforces the digest-pin; this file
# feeds the airgap VM fixture.
```

Bidirectional cross-reference means an implementer bumping one site sees the pointer to the other.

## Exit criteria

- `/nbr .#ci` green
- `grep '@sha256:4d9226b' infra/helm/rio-build/values.yaml` → ≥1 hit at the `envoyImage` line (T1: digest-pinned, same digest as docker-pulled.nix:94)
- `grep 'docker-pulled.nix' infra/helm/rio-build/values.yaml` → ≥1 hit (T1: cross-reference in comment)
- `grep 'values.yaml' nix/docker-pulled.nix` → ≥1 hit (T3: back-pointer comment)
- T2 helm-lint loop: `/nixbuild .#checks.x86_64-linux.helm-lint` → PASS
- T2 negative: temporarily revert T1 (`envoyImage: docker.io/envoyproxy/envoy:distroless-v1.37.1` no digest), `/nixbuild .#checks.x86_64-linux.helm-lint` → FAIL with "not digest-pinned" mentioning envoy; revert
- T2 scope: run the `yq eval-all ... | grep -v ':test$'` pipe manually on `/tmp/default.yaml` — output lists ONLY third-party images (devicePlugin, envoy, and any others); no `rio-*` images leak through

## Tracey

References existing markers:
- `r[dash.envoy.grpc-web-translate]` — T1 restores this spec requirement's deployability under chart defaults (same shape as P0369 serving `r[sec.pod.fuse-device-plugin]`). No `r[impl]` on a YAML value; the helm-lint assert (T2) is a guard, not a `r[verify]` annotation (nix build scripts aren't tracey-scanned).

No new markers. Digest-pin correctness + supply-chain enforcement, not new behavior.

## Files

```json files
[
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T1: :407 envoyImage +@sha256:4d9226b... digest (sourced from docker-pulled.nix:94); :404-406 comment update with cross-ref"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: :494-508 single-image devicePlugin check → generalized yq loop over all third-party images (DaemonSet+Deployment+StatefulSet, filter out :test rio images)"},
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T3: :87-90 comment +back-pointer to values.yaml:407 envoyImage (bump-both-together)"}
]
```

```
infra/helm/rio-build/
└── values.yaml               # T1: :407 envoyImage digest-pin
flake.nix                     # T2: :494-508 helm-lint yq-loop generalization
nix/docker-pulled.nix         # T3: :87-90 back-pointer comment
```

## Dependencies

```json deps
{"deps": [369], "soft_deps": [273, 282, 304, 371], "note": "discovered_from=consol-mc160. P0369 (DONE) added the helm-lint single-image devicePlugin digest check at flake.nix:494-508 — T2 generalizes it. values.yaml:407 envoyImage arrived with P0273 (DONE — dashboard-gateway templates) + P0282 (DONE — nginx docker image, EnvoyProxy CR config). docker-pulled.nix:94 envoy-distroless entry arrived with P0282/P0273 airgap set. T1 digest is SOURCED from docker-pulled.nix:94 — no crane/skopeo fetch needed. T2 soft-conflicts P0304-T86 (/tmp→$TMPDIR in helm-lint) — T86 is a sed over the existing block, T2 rewrites :494-508; if T86 lands first, T2 rebases onto the $TMPDIR'd paths; if T2 lands first, T86's sed finds fewer /tmp instances (T2's block uses /tmp/default.yaml already, which T86 would change). Sequence-independent, both apply cleanly. T2 also soft-conflicts P0304-T119 (devicePlugin image-tag lockstep assert) — that's a DIFFERENT check (values.yaml tag vs airgap-set tag), not the digest-pin; T2's loop and T119's lockstep are adjacent but independent yq blocks. T1 touches values.yaml — P0295-T69 also adds envoyGateway.namespace to values.yaml (different key, additive, non-overlapping with :404-407 envoyImage)."}
```

**Depends on:** [P0369](plan-0369-device-plugin-image-tag-fix.md) — helm-lint digest-pin pattern at [`flake.nix:494-508`](../../flake.nix) exists.
**Soft-deps:** [P0273](plan-0273-envoy-sidecar-grpcweb-mtls.md) + [P0282](plan-0282-dashboard-nginx-docker-fod-airgap.md) (DONE — envoyImage default + docker-pulled.nix entry arrived there).
**Conflicts with:** [`flake.nix`](../../flake.nix) count=33 — T2 rewrites `:494-508`; P0304-T86/T119 also touch helm-lint block (adjacent yq asserts, non-overlapping). [`values.yaml`](../../infra/helm/rio-build/values.yaml) — P0295-T69 adds `envoyGateway.namespace` key (different section); rebase-clean.
