# Plan 0357: Helm JWT pubkey ConfigMap mount — scheduler+store deployments

Bughunter mc=112 T4. [`jwt-pubkey-configmap.yaml`](../../infra/helm/rio-build/templates/jwt-pubkey-configmap.yaml) creates the `rio-jwt-pubkey` ConfigMap when `.Values.jwt.enabled=true`, but **nothing mounts it**. [`scheduler.yaml:67-140`](../../infra/helm/rio-build/templates/scheduler.yaml) and [`store.yaml:40-116`](../../infra/helm/rio-build/templates/store.yaml) have no `RIO_JWT__KEY_PATH` env var, no `jwt-pubkey` volumeMount, no `jwt-pubkey` volume. The gateway similarly lacks a mount for `rio-jwt-signing` Secret — [`jwt-signing-secret.yaml`](../../infra/helm/rio-build/templates/jwt-signing-secret.yaml) creates it, [`gateway.yaml:37-113`](../../infra/helm/rio-build/templates/gateway.yaml) never mounts it.

[P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) wired `spawn_pubkey_reload` in scheduler+store `main.rs` ([`scheduler/main.rs:667`](../../rio-scheduler/src/main.rs), [`store/main.rs:575`](../../rio-store/src/main.rs)) but its premise at `:9` — "The ConfigMap mount is already in helm templates" — was **wrong**. The ConfigMap object exists; the mount doesn't. With `jwt.enabled=true`, operators get a Helm release that renders the ConfigMap+Secret but leaves `cfg.jwt.key_path = None` in every pod → [`main.rs:667`](../../rio-scheduler/src/main.rs) takes the `None` branch → interceptor inert → **every JWT passes unverified**. Silent fail-open.

[`r[gw.jwt.verify]`](../../docs/src/components/gateway.md) at `:493-495` states the interceptor MUST reject invalid tokens. The spec is correct, the Rust is correct (P0260+P0349), the Helm wiring is missing. This is the same orphan pattern as [P0272](plan-0272-narinfo-tenant-visibility-filter.md)/[P0338](plan-0338-tenant-signer-wiring-putpath.md): helper exists, K8s resource exists, nothing connects them.

## Entry criteria

- [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) merged — scheduler+store main.rs read `cfg.jwt.key_path` and spawn reload task
- [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) merged — `JwtConfig` with `key_path: Option<PathBuf>` exists; `RIO_JWT__KEY_PATH` figment key works

## Tasks

### T1 — `fix(helm):` _helpers.tpl — rio.jwtVerifyEnv + rio.jwtVerifyVolumeMount + rio.jwtVerifyVolume

MODIFY [`infra/helm/rio-build/templates/_helpers.tpl`](../../infra/helm/rio-build/templates/_helpers.tpl). Add three new named templates after the `rio.tls*` block at `:64` — same triplet pattern (env / volumeMount / volume), self-guarded on `.Values.jwt.enabled` like `rio.cov*`:

```gotmpl
{{/*
JWT pubkey mount — SCHEDULER + STORE. Self-guarded on .Values.jwt.enabled
(renders nothing when disabled, so callers include unconditionally with
`| nindent 12`). ConfigMap is PUBLIC (ed25519 verifying key) — mounted
read-only, no Secret perms needed. key_path matches what P0349's main.rs
wiring reads.

File-key-mapping: ConfigMap data key `ed25519_pubkey` → file
`/etc/rio/jwt/ed25519_pubkey`. scheduler/main.rs:55 doc-comment already
references this path — this mount makes it real.
*/}}
{{- define "rio.jwtVerifyEnv" -}}
{{- if .Values.jwt.enabled }}
- name: RIO_JWT__KEY_PATH
  value: /etc/rio/jwt/ed25519_pubkey
{{- end }}
{{- end -}}

{{- define "rio.jwtVerifyVolumeMount" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-pubkey
  mountPath: /etc/rio/jwt
  readOnly: true
{{- end }}
{{- end -}}

{{- define "rio.jwtVerifyVolume" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-pubkey
  configMap:
    name: rio-jwt-pubkey
{{- end }}
{{- end -}}

{{/*
JWT signing seed mount — GATEWAY ONLY. Secret (private ed25519 seed).
Same self-guard pattern; gateway main.rs reads RIO_JWT__KEY_PATH for the
SIGNING seed path (JwtConfig is shared type, both sides use key_path).
Gateway decodes Secret's base64 layer → 32 raw bytes → SigningKey::from_bytes.
*/}}
{{- define "rio.jwtSignEnv" -}}
{{- if .Values.jwt.enabled }}
- name: RIO_JWT__KEY_PATH
  value: /etc/rio/jwt/ed25519_seed
{{- end }}
{{- end -}}

{{- define "rio.jwtSignVolumeMount" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-signing
  mountPath: /etc/rio/jwt
  readOnly: true
{{- end }}
{{- end -}}

{{- define "rio.jwtSignVolume" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-signing
  secret:
    secretName: rio-jwt-signing
{{- end }}
{{- end -}}
```

**CHECK AT DISPATCH:** Gateway's signing-side config field name. Grep `rio-gateway/src/main.rs` for `jwt.key_path` vs `signing_seed_path` vs similar — if the gateway uses a different figment key than scheduler/store, adjust the env var name in `rio.jwtSignEnv`. The shared `JwtConfig` in [`rio-common/src/config.rs:112`](../../rio-common/src/config.rs) suggests `key_path` is reused, but gateway may have a dedicated signing struct.

### T2 — `fix(helm):` scheduler.yaml — mount jwt pubkey ConfigMap

MODIFY [`infra/helm/rio-build/templates/scheduler.yaml`](../../infra/helm/rio-build/templates/scheduler.yaml). Three edits:

1. After `rio.covEnv` at `:69`, add:
   ```yaml
   {{- include "rio.jwtVerifyEnv" . | nindent 12 }}
   ```
2. Change the `volumeMounts:` gate at `:99` from `{{- if or .Values.tls.enabled .Values.coverage.enabled }}` to `{{- if or .Values.tls.enabled .Values.coverage.enabled .Values.jwt.enabled }}`, and add after `rio.covVolumeMount` at `:104`:
   ```yaml
   {{- include "rio.jwtVerifyVolumeMount" . | nindent 12 }}
   ```
3. Same gate-extension at `:134` (volumes block), add after `rio.covVolume` at `:139`:
   ```yaml
   {{- include "rio.jwtVerifyVolume" . | nindent 8 }}
   ```

The `or`-gate extension is necessary: `volumeMounts:` / `volumes:` keys must not render empty (Helm template produces `volumeMounts:` with nothing under it → kubectl rejects). The self-guarded templates handle the per-item gating; the outer `or` handles the key-presence gating.

### T3 — `fix(helm):` store.yaml — mount jwt pubkey ConfigMap

MODIFY [`infra/helm/rio-build/templates/store.yaml`](../../infra/helm/rio-build/templates/store.yaml). Same three edits as T2:

1. After `rio.covEnv` at `:42`: `{{- include "rio.jwtVerifyEnv" . | nindent 12 }}`
2. Extend `:82` or-gate with `.Values.jwt.enabled`; add after `:87`: `{{- include "rio.jwtVerifyVolumeMount" . | nindent 12 }}`
3. Extend `:110` or-gate with `.Values.jwt.enabled`; add after `:115`: `{{- include "rio.jwtVerifyVolume" . | nindent 8 }}`

### T4 — `fix(helm):` gateway.yaml — mount jwt signing Secret

MODIFY [`infra/helm/rio-build/templates/gateway.yaml`](../../infra/helm/rio-build/templates/gateway.yaml). The signing-side mirror:

1. After `rio.covEnv` at `:39`: `{{- include "rio.jwtSignEnv" . | nindent 12 }}`
2. Extend `:63` or-gate with `.Values.jwt.enabled`; add after `:75`: `{{- include "rio.jwtSignVolumeMount" . | nindent 12 }}`
3. Extend `:89` or-gate with `.Values.jwt.enabled`; add after `:112`: `{{- include "rio.jwtSignVolume" . | nindent 8 }}`

**CONDITIONAL:** If the gateway already has a jwt mount via a different mechanism (grep `jwt` in gateway.yaml at dispatch — current sprint-1 has zero hits), skip T4 and note the existing path.

### T5 — `test(helm):` helm-lint assertion — jwt.enabled renders mounts

MODIFY the `helm-lint` check. Grep at dispatch for the existing helm-lint derivation (likely `nix/helm-lint.nix` or inlined in `flake.nix` near `cargoChecks.helm-lint`). Add a second template-render pass with `--set jwt.enabled=true,jwt.publicKey=dGVzdA==,jwt.signingSeed=dGVzdA==` and assert the rendered output contains the mount:

```bash
helm template rio-build infra/helm/rio-build \
  --set jwt.enabled=true \
  --set jwt.publicKey=dGVzdA== \
  --set jwt.signingSeed=dGVzdA== \
  --set global.image.tag=test \
  > /tmp/jwt-enabled.yaml
# Mount rendered in scheduler Deployment:
grep -A2 'name: rio-scheduler' /tmp/jwt-enabled.yaml | grep -q Deployment
yq 'select(.kind=="Deployment" and .metadata.name=="rio-scheduler")
    | .spec.template.spec.volumes[] | select(.name=="jwt-pubkey")' \
  /tmp/jwt-enabled.yaml | grep -q 'rio-jwt-pubkey' \
  || { echo "FAIL: scheduler missing jwt-pubkey volume"; exit 1; }
yq 'select(.kind=="Deployment" and .metadata.name=="rio-scheduler")
    | .spec.template.spec.containers[0].env[] | select(.name=="RIO_JWT__KEY_PATH")' \
  /tmp/jwt-enabled.yaml | grep -q '/etc/rio/jwt/ed25519_pubkey' \
  || { echo "FAIL: scheduler missing RIO_JWT__KEY_PATH"; exit 1; }
# Repeat for rio-store (jwt-pubkey) and rio-gateway (jwt-signing).
# Negative: jwt.enabled=false renders NO mount (the or-gate correctly elides).
helm template rio-build infra/helm/rio-build --set global.image.tag=test \
  > /tmp/jwt-disabled.yaml
! grep -q 'jwt-pubkey\|RIO_JWT__KEY_PATH' /tmp/jwt-disabled.yaml \
  || { echo "FAIL: jwt mount rendered with jwt.enabled=false"; exit 1; }
```

**CHECK AT DISPATCH:** Whether `yq` is available in the helm-lint derivation's buildInputs — add it if not (`pkgs.yq-go`). The helm-lint check is already in `cargoChecks.helm-lint` per [`flake.nix:872`](../../flake.nix) — find its definition and extend the checkPhase.

### T6 — `test(vm):` security.nix jwt-mount-present subtest

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix). The existing `jwt-dual-mode` subtest at `:832` proves the fallback branch (gateway has NO JWT config, metric stays 0). Add a new subtest (or precondition) that proves the mount renders when enabled:

```python
# r[verify sec.jwt.pubkey-mount]
# Proves: with jwt.enabled=true, the scheduler pod has the ConfigMap
# mounted at /etc/rio/jwt/ed25519_pubkey AND RIO_JWT__KEY_PATH env set.
# This is the gap P0349 assumed closed but wasn't — P0357 closes it.
with subtest("jwt-mount-present: scheduler+store see pubkey at key_path"):
    # k3s renders the helm chart with jwt.enabled=true for this VM
    # (values/vmtest-jwt.yaml or inline --set in the k3s-full fixture).
    sched_env = vm.succeed(
        "kubectl exec -n rio deploy/rio-scheduler -- env | grep RIO_JWT__KEY_PATH"
    )
    assert "/etc/rio/jwt/ed25519_pubkey" in sched_env, (
        f"scheduler missing RIO_JWT__KEY_PATH env: {sched_env!r}"
    )
    pubkey_content = vm.succeed(
        "kubectl exec -n rio deploy/rio-scheduler -- "
        "cat /etc/rio/jwt/ed25519_pubkey"
    )
    assert len(pubkey_content.strip()) > 0, "pubkey file empty"
    # Same check for store:
    store_env = vm.succeed(
        "kubectl exec -n rio deploy/rio-store -- env | grep RIO_JWT__KEY_PATH"
    )
    assert "/etc/rio/jwt/ed25519_pubkey" in store_env
```

**Fixture dependency:** This subtest requires the VM fixture to render with `jwt.enabled=true`. Check at dispatch whether [`nix/tests/fixtures/k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) or the security-scenario fixture has a `jwt.enabled` knob. If not, either (a) add a `jwtEnabled ? false` argument to the fixture and set it `true` for this subtest's VM, or (b) write an inline `helm upgrade --set jwt.enabled=true` step before the assertion. Option (a) is cleaner — fixture-level gating matches how `tls.enabled` is already handled.

**ADD TO default.nix:** Per [P0341](plan-0341-tracey-verify-reachability-convention.md) convention, the `r[verify sec.jwt.pubkey-mount]` marker goes in [`nix/tests/default.nix`](../../nix/tests/default.nix) at the `subtests` entry for security scenarios, NOT only in the scenario file. Grep `"jwt-mount-present"` in default.nix after adding the subtest.

### T7 — `docs:` P0349 plan-doc erratum — premise at :9 was wrong

MODIFY [`.claude/work/plan-0349-wire-spawn-pubkey-reload-main-rs.md`](plan-0349-wire-spawn-pubkey-reload-main-rs.md) at `:9`. Current text: "The ConfigMap mount is already in helm templates (`jwt-pubkey-configmap.yaml` — grep `infra/helm/` at dispatch). The only missing piece is the main.rs wiring." This is **false** — the ConfigMap OBJECT exists; the MOUNT doesn't. Add erratum bracket:

```
> **[ERRATUM — bughunter mc=112]:** The ConfigMap OBJECT exists in
> helm; the volumeMount/volume/env-var do NOT. scheduler.yaml + store.yaml
> + gateway.yaml had zero `jwt` mounts. [P0357](plan-0357-helm-jwt-pubkey-mount.md)
> closes the gap. P0349's main.rs wiring is correct; the Helm half was
> orphaned. Same class as P0272/P0338 helper-exists-nobody-calls-it.
```

## Exit criteria

- `helm template rio-build infra/helm/rio-build --set jwt.enabled=true --set jwt.publicKey=x --set jwt.signingSeed=x --set global.image.tag=t | grep -c RIO_JWT__KEY_PATH` → ≥3 (scheduler + store + gateway)
- Same template render `| yq 'select(.metadata.name=="rio-scheduler" and .kind=="Deployment") | .spec.template.spec.volumes[].name'` includes `jwt-pubkey`
- Same for `rio-store` → `jwt-pubkey`; `rio-gateway` → `jwt-signing`
- `helm template ... --set jwt.enabled=false ... | grep -c 'jwt-pubkey\|RIO_JWT__KEY_PATH'` → 0 (negative: or-gate correctly elides)
- `nix build .#checks.x86_64-linux.helm-lint` → green with T5 assertions
- `grep '"jwt-mount-present"' nix/tests/default.nix` → ≥1 hit (T6 subtest wired, P0341 convention)
- VM security test `jwt-mount-present` subtest passes (if jwt fixture knob added)
- `grep 'ERRATUM.*mc=112' .claude/work/plan-0349-*.md` → 1 hit (T7)

## Tracey

References existing markers:
- `r[gw.jwt.verify]` — [`gateway.md:493`](../../docs/src/components/gateway.md). T2/T3 make the mount that lets scheduler/store load the pubkey the interceptor verifies against. No `r[impl]` annotation (Helm YAML isn't Rust); the main.rs wiring already has it from P0349.
- `r[gw.jwt.issue]` — [`gateway.md:489`](../../docs/src/components/gateway.md). T4 makes the gateway signing-seed mount.

Adds new marker to component specs:
- `r[sec.jwt.pubkey-mount]` → [`docs/src/security.md`](../../docs/src/security.md) (see Spec additions below). The T6 VM subtest gets `r[verify sec.jwt.pubkey-mount]`.

## Spec additions

**New `r[sec.jwt.pubkey-mount]`** (goes to [`docs/src/security.md`](../../docs/src/security.md) in the Boundary 2 gRPC section, after the HMAC assignment-token block at `:43`, standalone paragraph, blank line before, col 0):

```
r[sec.jwt.pubkey-mount]
When `jwt.enabled=true`, scheduler and store pods MUST have the `rio-jwt-pubkey` ConfigMap mounted at `/etc/rio/jwt/ed25519_pubkey` and `RIO_JWT__KEY_PATH` set to that path. Without the mount, `cfg.jwt.key_path` remains `None` and the interceptor falls through to inert mode (every RPC passes, no `Claims` attached) — a silent fail-open. The gateway correspondingly mounts the `rio-jwt-signing` Secret at `/etc/rio/jwt/ed25519_seed`. Helm `_helpers.tpl` provides `rio.jwtVerifyEnv`/`VolumeMount`/`Volume` and `rio.jwtSignEnv`/`VolumeMount`/`Volume` triplets, self-guarded on `.Values.jwt.enabled`.
```

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/_helpers.tpl", "action": "MODIFY", "note": "T1: add rio.jwtVerify{Env,VolumeMount,Volume} + rio.jwtSign{Env,VolumeMount,Volume} named templates after :64 tls block"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "MODIFY", "note": "T2: include jwtVerifyEnv at :69, extend or-gates at :99/:134 with .Values.jwt.enabled, include jwtVerifyVolumeMount/Volume"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "T3: include jwtVerifyEnv at :42, extend or-gates at :82/:110, include jwtVerifyVolumeMount/Volume"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "MODIFY", "note": "T4: include jwtSignEnv at :39, extend or-gates at :63/:89, include jwtSignVolumeMount/Volume"},
  {"path": "nix/helm-lint.nix", "action": "MODIFY", "note": "T5: add jwt.enabled=true render + yq mount assertions + negative jwt.enabled=false no-mount assert (CHECK: actual helm-lint file location at dispatch)"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T6: +jwt-mount-present subtest with r[verify sec.jwt.pubkey-mount] — kubectl exec env/cat assertions"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T6: add jwt-mount-present to security subtests list (P0341 marker-at-wiring-point convention)"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T6: add jwtEnabled ? false fixture arg → helm --set jwt.enabled (CHECK: fixture knob pattern)"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T1 spec: +r[sec.jwt.pubkey-mount] marker after :43 in Boundary 2 gRPC section"},
  {"path": ".claude/work/plan-0349-wire-spawn-pubkey-reload-main-rs.md", "action": "MODIFY", "note": "T7: erratum bracket at :9 — premise 'mount already in helm' was wrong"}
]
```

```
infra/helm/rio-build/templates/
├── _helpers.tpl            # T1: +6 named templates
├── scheduler.yaml          # T2: +jwtVerify includes
├── store.yaml              # T3: +jwtVerify includes
└── gateway.yaml            # T4: +jwtSign includes
nix/
├── helm-lint.nix           # T5: jwt render assertions
└── tests/
    ├── scenarios/security.nix  # T6: jwt-mount-present subtest
    ├── default.nix             # T6: subtest wired
    └── fixtures/k3s-full.nix   # T6: jwtEnabled fixture knob
docs/src/security.md        # spec: r[sec.jwt.pubkey-mount]
```

## Dependencies

```json deps
{"deps": [349, 260], "soft_deps": [355, 341], "note": "P0349 wired main.rs to read cfg.jwt.key_path — this plan provides the key_path via mount. P0260 added JwtConfig+key_path field+spawn_pubkey_reload helper. Soft-dep P0355 (extract load_and_wire_jwt helper): P0355 T2 consolidates the 21L key_path match block that P0349 wrote; if P0355 lands first, the main.rs wiring lives in rio-common/src/jwt_interceptor.rs::load_and_wire_jwt instead of each main.rs — this plan's Helm mounts are independent of where the Rust reads the path. Soft-dep P0341 (tracey nix verify reachability): T6 follows its marker-at-default.nix-subtests convention. discovered_from=bughunter(mc112)."}
```

**Depends on:** [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) — scheduler/store main.rs read `cfg.jwt.key_path`, spawn reload. [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) — `JwtConfig.key_path`, `spawn_pubkey_reload`, `parse_jwt_pubkey`.

**Soft-deps:** [P0355](plan-0355-extract-drain-jwt-load-helpers.md) — consolidates the `key_path` match; sequence-independent (Helm mount is orthogonal to which Rust file reads it). [P0341](plan-0341-tracey-verify-reachability-convention.md) — T6 places `r[verify]` at `default.nix` subtests entry.

**Conflicts with:** `_helpers.tpl` has zero open-plan collision (grep at dispatch). [`scheduler.yaml`](../../infra/helm/rio-build/templates/scheduler.yaml), [`store.yaml`](../../infra/helm/rio-build/templates/store.yaml), [`gateway.yaml`](../../infra/helm/rio-build/templates/gateway.yaml) are low-collision infra files. [`security.nix`](../../nix/tests/scenarios/security.nix) is touched by P0304-T53 (`:698` TODO re-tag — non-overlapping with new subtest at tail). [`default.nix`](../../nix/tests/default.nix) touched by P0341 (marker migration — additive, non-overlapping). [`security.md`](../../docs/src/security.md) touched by P0295-T5 (`:69` Bearer strike — different section from `:43` insert).
