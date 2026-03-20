# Plan 0369: devicePlugin.image broken tag — v1.20.15 does not exist upstream

[P0360](plan-0360-device-plugin-vm-coverage.md) post-PASS review finding. [`infra/helm/rio-build/values.yaml:383`](../../infra/helm/rio-build/values.yaml) defaults `devicePlugin.image` to `ghcr.io/smarter-project/smarter-device-manager:v1.20.15`, but upstream has never published a `v1.20.15` tag — the latest in the `v1.20` series is `v1.20.12`. P0360's VM test (`vmtest-full-nonpriv.yaml`) works around this by pulling a digest-pinned image into the airgap set ([`nix/docker-pulled.nix`](../../nix/docker-pulled.nix)) — so the VM test never exercises the chart default.

**Production impact:** a fresh `helm install` with chart defaults → DaemonSet pods go `ImagePullBackOff` → `smarter-devices/fuse` extended resource never registers → worker pods requesting it sit `Pending` (`0/N nodes are available: 1 Insufficient smarter-devices/fuse`). Since `workerPool.privileged` defaults to `false` and the non-privileged path hard-depends on the device-plugin resource (ADR-012, `r[sec.pod.fuse-device-plugin]`), a default-values production deploy is **unschedulable** until the operator overrides the image.

**Fix:** bump to `v1.20.12` and digest-pin. The `:381-382` comment already says "Pin to a digest for production" — this plan does that in the default. The tag remains readable (`v1.20.12@sha256:…`) for operator clarity; kubelet pulls by digest.

## Entry criteria

- [P0360](plan-0360-device-plugin-vm-coverage.md) merged — `vmtest-full-nonpriv.yaml` exists with its own image override; this plan can freely change the chart default without breaking the VM fixture (the airgap set is digest-pinned independently).

## Tasks

### T1 — `fix(infra):` values.yaml devicePlugin.image — v1.20.15→v1.20.12 + digest pin

MODIFY [`infra/helm/rio-build/values.yaml:383`](../../infra/helm/rio-build/values.yaml):

```yaml
# Before:
image: ghcr.io/smarter-project/smarter-device-manager:v1.20.15

# After:
image: ghcr.io/smarter-project/smarter-device-manager:v1.20.12@sha256:<pin-at-dispatch>
```

Resolve the digest at dispatch:

```bash
# crane is in the dev shell
nix develop -c crane digest ghcr.io/smarter-project/smarter-device-manager:v1.20.12
```

Update the `:381-382` comment to reflect digest-pinned status:

```yaml
# Upstream image. Digest-pinned — bump via `crane digest <repo>:<new-tag>`
# on upstream releases. Tag portion is cosmetic (kubelet pulls by digest).
```

**Consistency check:** P0360-T1 added a `smarter-device-manager` entry to [`nix/docker-pulled.nix`](../../nix/docker-pulled.nix) with its own `imageDigest`. That pin is independent (it feeds the airgap k3s fixture, not production), but verify at dispatch that both reference compatible versions — the VM test exercises the cgroup-remount + device-injection path; a v1.20.5 airgap image proving a v1.20.12 prod default works is only valid if the device-plugin API hasn't changed between them. If in doubt, align both to the same digest.

### T2 — `test(infra):` helm-lint guard — devicePlugin.image must be digest-pinned

MODIFY [`flake.nix`](../../flake.nix) at the helm-lint check (the yq-assertion block added by [P0357](plan-0357-helm-jwt-pubkey-mount.md), `~:498-573`). Add a render-time assert that the rendered DaemonSet's image spec contains `@sha256:`:

```nix
# devicePlugin.image MUST be digest-pinned. A floating tag
# (values.yaml:383) that doesn't exist upstream yields
# ImagePullBackOff → worker pods Pending → silent cluster brick
# on default-values deploy. P0369 found v1.20.15 was never
# published. Digest-pin prevents both the broken-tag case and
# tag-reuse drift.
${yq}/bin/yq eval \
  '.spec.template.spec.containers[0].image | test("@sha256:")' \
  $out/device-plugin.yaml \
  | grep -q true \
  || { echo "FAIL: devicePlugin.image not digest-pinned"; exit 1; }
```

Adjust the output path to match how the existing helm-lint asserts address rendered manifests (grep the P0357 yq block for the actual pattern — may be `$TMPDIR/rendered/rio-build/templates/device-plugin.yaml` or a `helm template | yq` pipe).

### T3 — `docs(infra):` ADR-012 — note the digest-pin + upgrade path

MODIFY [`docs/src/decisions/012-privileged-worker-pods.md:54`](../../docs/src/decisions/012-privileged-worker-pods.md). The existing bullet says "`smarter-device-manager` DaemonSet (`infra/helm/rio-build/templates/device-plugin.yaml`)". Add a parenthetical after the Helm value reference at `:56`:

```markdown
The Helm chart default is `workerPool.privileged: false`. The device
plugin is enabled by default (`devicePlugin.enabled: true`), digest-pinned
to a known-good upstream release (`values.yaml:383` — bump via
`crane digest` on upstream updates; the helm-lint check enforces the pin).
```

## Exit criteria

- `/nbr .#ci` green
- `grep '@sha256:' infra/helm/rio-build/values.yaml` → ≥1 hit at the `devicePlugin.image` line
- `grep 'v1.20.15' infra/helm/rio-build/values.yaml` → 0 hits (broken tag gone)
- T2: helm-lint check fails when `devicePlugin.image` is a bare tag — at dispatch, temporarily set `image: foo:latest`, run `/nixbuild .#checks.x86_64-linux.helm-lint`, confirm FAIL with "not digest-pinned" message, revert
- `nix develop -c crane manifest ghcr.io/smarter-project/smarter-device-manager@<pinned-digest>` — manifest resolves (digest is real, image pullable)
- T3: `grep 'digest-pinned\|crane digest' docs/src/decisions/012-privileged-worker-pods.md` → ≥1 hit

## Tracey

References existing markers:
- `r[sec.pod.fuse-device-plugin]` — T1 restores this spec requirement's deployability under chart defaults. No `r[impl]` annotation on a YAML value, but the helm-lint assert (T2) functions as a `r[verify]`-equivalent guard. The marker's `r[verify]` annotation at the P0360 security.nix fragment remains the actual end-to-end test.

No new markers. This is a dependency-pin correctness fix, not new behavior.

## Files

```json files
[
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T1: :383 v1.20.15→v1.20.12@sha256:<digest>; :381-382 comment updated"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: helm-lint yq assert — devicePlugin.image must contain @sha256:"},
  {"path": "docs/src/decisions/012-privileged-worker-pods.md", "action": "MODIFY", "note": "T3: :56 digest-pin + crane-bump note"}
]
```

```
infra/helm/rio-build/
└── values.yaml              # T1: :383 tag fix + digest-pin
flake.nix                    # T2: helm-lint digest-pin assert
docs/src/decisions/
└── 012-privileged-worker-pods.md  # T3: upgrade-path note
```

## Dependencies

```json deps
{"deps": [360], "soft_deps": [286, 357], "note": "discovered_from=360-review. Hard-dep P0360: vmtest-full-nonpriv.yaml + docker-pulled.nix smarter-device-manager entry arrive with it. This plan changes the chart default; P0360's airgap pin is independent. Soft-dep P0286 (DONE): device-plugin.yaml template exists; values.yaml:383 default arrived with it. Soft-dep P0357 (DONE): helm-lint yq assert block at flake.nix:~498-573 exists — T2 extends it. T2 touches flake.nix (collision=33, 3rd highest); the helm-lint block is a dedicated check, additive yq line, low semantic conflict. P0304-T86 also edits helm-lint (:498-573 /tmp→$TMPDIR) — both additive, non-overlapping concerns, rebase-clean either order."}
```

**Depends on:** [P0360](plan-0360-device-plugin-vm-coverage.md) — `vmtest-full-nonpriv.yaml` override + docker-pulled.nix airgap pin exist.
**Soft-deps:** [P0286](plan-0286-privileged-hardening-device-plugin.md) (DONE — the broken default arrived here), [P0357](plan-0357-helm-jwt-pubkey-mount.md) (DONE — helm-lint yq block exists).
**Conflicts with:** [`flake.nix`](../../flake.nix) count=33 — T2 is additive in the helm-lint check, non-overlapping with P0304-T86 (`/tmp→$TMPDIR`), P0304-T29 (tracey fileset), P0304-T74/T76/T88/T89 (mutants/golden/cov/sqlx-prepare — all different check blocks). Rebase-clean.
