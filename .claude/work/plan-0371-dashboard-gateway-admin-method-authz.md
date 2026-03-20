# Plan 0371: Dashboard-gateway admin method-authz — gate ClearPoison + CORS tighten

[`dashboard-gateway.yaml:77`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml) says *"No method gating in MVP; a malicious browser client can call ClearPoison etc. — phase 5b adds per-method authz."* — an orphaned deferral with no `TODO(PNNNN)` tag and no owning plan. Combined with [`dashboard-gateway-policy.yaml:27`](../../infra/helm/rio-build/templates/dashboard-gateway-policy.yaml) `allowOrigins: ["*"]`, any origin can issue a gRPC-Web POST to `rio.admin.AdminService/ClearPoison`, `DrainWorker`, `CreateTenant`, `TriggerGC`. The [`allowOrigins:10-12`](../../infra/helm/rio-build/templates/dashboard-gateway-policy.yaml) comment says "tighten allowOrigins to the dashboard's served origin post-MVP (once [P0274](plan-0274-dashboard-svelte-scaffold.md) lands)" — P0274 has now landed the nginx Deployment, so the blocker is gone.

Envoy Gateway's `GRPCRoute` CRD supports per-method matching ([`GRPCRouteMatch.method.method`](https://gateway-api.sigs.k8s.io/reference/spec/#gateway.networking.k8s.io%2fv1.GRPCMethodMatch)). `SecurityPolicy` can target a specific `GRPCRoute` rather than the parent `Gateway`, and supports a `jwt` authenticator block — but the scheduler doesn't issue dashboard JWTs yet. The minimal fix: split the `GRPCRoute` into **read-only** (reachable, CORS-wildcard-ok) and **mutating** (unreachable from the dashboard until a dashboard auth mechanism lands). Tighten CORS to the nginx Service origin once P0274's Service hostname is known.

discovered_from=273-review. Severity: security (orphaned deferral on admin-RPC exposure).

## Entry criteria

- [P0273](plan-0273-envoy-sidecar-grpc-web.md) merged (dashboard-gateway templates exist)
- [P0274](plan-0274-dashboard-svelte-scaffold.md) merged (nginx Deployment + Service hostname known for CORS origin)

## Tasks

### T1 — `fix(infra):` split GRPCRoute — read-only vs mutating methods

MODIFY [`infra/helm/rio-build/templates/dashboard-gateway.yaml:63-86`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml). Replace the single `GRPCRoute` matching `rio.admin.AdminService` + `rio.scheduler.SchedulerService` with two routes:

```yaml
---
apiVersion: gateway.networking.k8s.io/v1
kind: GRPCRoute
metadata:
  name: rio-scheduler-readonly
  namespace: {{ $ns }}
spec:
  parentRefs:
    - name: rio-dashboard
  rules:
    - matches:
        # Read-only AdminService methods (status, lists, logs, graph).
        # Dashboard calls these on every page load — safe to expose.
        - method: { service: rio.admin.AdminService, method: ClusterStatus }
        - method: { service: rio.admin.AdminService, method: ListWorkers }
        - method: { service: rio.admin.AdminService, method: ListBuilds }
        - method: { service: rio.admin.AdminService, method: GetBuildLogs }
        - method: { service: rio.admin.AdminService, method: ListTenants }
        - method: { service: rio.admin.AdminService, method: GetBuildGraph }
        - method: { service: rio.admin.AdminService, method: GetSizeClassStatus }
        # SchedulerService: SubmitBuild/CancelBuild are mutating but
        # ssh-ng is the production entrypoint; dashboard doesn't call them.
        # WatchBuild is read-only streaming. Leave SchedulerService
        # ungated for now — route only the methods the dashboard actually
        # uses (WatchBuild).
        - method: { service: rio.scheduler.SchedulerService, method: WatchBuild }
      backendRefs:
        - name: rio-scheduler
          port: 9001
---
{{- if .Values.dashboard.enableMutatingMethods }}
apiVersion: gateway.networking.k8s.io/v1
kind: GRPCRoute
metadata:
  name: rio-scheduler-mutating
  namespace: {{ $ns }}
spec:
  parentRefs:
    - name: rio-dashboard
  rules:
    - matches:
        # Mutating AdminService methods. Gated behind a values flag until
        # per-method dashboard authz lands (future plan — see T4).
        # ClearPoison: clears poisoned derivation state — can un-brake a
        # deliberately blocked build.
        # DrainWorker: marks a worker as draining — can stall the fleet.
        # CreateTenant: creates PG rows — resource creation.
        # TriggerGC: deletes store paths — data loss potential.
        - method: { service: rio.admin.AdminService, method: ClearPoison }
        - method: { service: rio.admin.AdminService, method: DrainWorker }
        - method: { service: rio.admin.AdminService, method: CreateTenant }
        - method: { service: rio.admin.AdminService, method: TriggerGC }
      backendRefs:
        - name: rio-scheduler
          port: 9001
{{- end }}
```

Delete the `:74-77` "No method gating in MVP" comment — this plan IS the gate. Default `enableMutatingMethods: false` in values.yaml (T3).

### T2 — `fix(infra):` CORS allowOrigins — wildcard → values-driven origin list

MODIFY [`infra/helm/rio-build/templates/dashboard-gateway-policy.yaml:25-39`](../../infra/helm/rio-build/templates/dashboard-gateway-policy.yaml):

```yaml
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: GRPCRoute
      name: rio-scheduler-readonly
  cors:
    allowOrigins:
      {{- toYaml .Values.dashboard.cors.allowOrigins | nindent 6 }}
```

Retarget the `SecurityPolicy` from `Gateway/rio-dashboard` to `GRPCRoute/rio-scheduler-readonly` so the CORS policy scopes to the read-only methods only. If `enableMutatingMethods: true`, a SECOND `SecurityPolicy` targets `rio-scheduler-mutating` with a tighter origin (operator's choice — template both, let values drive).

Delete the `:10-12` "Permissive MVP — tighten allowOrigins post-MVP once P0274 lands" comment — P0274 landed, this plan tightens.

### T3 — `feat(infra):` values.yaml dashboard.cors + dashboard.enableMutatingMethods

MODIFY [`infra/helm/rio-build/values.yaml:379-395`](../../infra/helm/rio-build/values.yaml) `dashboard:` block:

```yaml
dashboard:
  enabled: false
  # ... existing envoyServiceName/Type/Image ...

  # CORS allowed origins for gRPC-Web. The dashboard nginx Service
  # hostname is the only legitimate origin in production. Wildcard
  # (the previous default) allows any website to call admin RPCs.
  cors:
    allowOrigins:
      - "http://rio-dashboard.{{ .Values.namespace.name }}.svc.cluster.local"
      # Add external hostnames (Ingress, LoadBalancer) for prod.

  # Expose ClearPoison/DrainWorker/CreateTenant/TriggerGC via the
  # gateway. Default false — use rio-cli with mTLS client cert for
  # admin operations until dashboard auth lands.
  enableMutatingMethods: false
```

### T4 — `docs(dashboard):` tombstone phase-5b-authz deferral + spec marker

MODIFY [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) — add a paragraph after `:27` (the gRPC-Web translate paragraph) describing the read-only vs mutating split. Add new marker to `## Spec additions` below.

Also: delete the orphaned "phase 5b adds per-method authz" text from [`dashboard-gateway.yaml:77`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml) — T1 removes the whole comment block; T4 records WHY in the spec.

### T5 — `test(infra):` helm-lint assert — mutating-route absent when flag false

MODIFY [`flake.nix`](../../flake.nix) helm-lint `checkPhase` (grep for `helm-lint` or `helm template` in flake.nix). Add yq assertions:

```bash
# T5: dashboard-gateway method split — default values must NOT render
# the mutating route. Proves the default is fail-closed.
if yq -e 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-mutating")' rendered/*.yaml >/dev/null 2>&1; then
  echo "FAIL: rio-scheduler-mutating GRPCRoute rendered with default values (should be gated behind enableMutatingMethods)" >&2
  exit 1
fi
# Readonly route MUST render and MUST contain ClusterStatus.
yq -e 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly") | .spec.rules[].matches[].method | select(.method=="ClusterStatus")' rendered/*.yaml >/dev/null
# ClearPoison must NOT appear in the readonly route.
if yq -e 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly") | .spec.rules[].matches[].method | select(.method=="ClearPoison")' rendered/*.yaml >/dev/null 2>&1; then
  echo "FAIL: ClearPoison leaked into readonly GRPCRoute" >&2
  exit 1
fi
```

### T6 — `test(vm):` cli.nix — grpcurl ClearPoison via gateway → UNIMPLEMENTED (not routed)

MODIFY [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix) — extend the existing curl-gate subtest (P0273 T5's `0x80` trailer grep block) with a negative assertion:

```python
with subtest("dashboard-gateway refuses ClearPoison (unrouted)"):
    # curl gRPC-Web POST to ClearPoison through the envoy Service.
    # Expect: 404 or UNIMPLEMENTED — the GRPCRoute doesn't match it.
    rc, out = host.execute(
        f"curl -sS -o /dev/null -w '%{{http_code}}' "
        f"-X POST http://{envoy_svc_ip}:8080"
        f"/rio.admin.AdminService/ClearPoison "
        f"-H 'Content-Type: application/grpc-web+proto' "
        f"-d ''"
    )
    # Envoy returns 200 with grpc-status:12 (UNIMPLEMENTED) on unrouted
    # gRPC-Web, OR 404 if the listener returns no route match. Accept
    # either; reject 200+grpc-status:0 (that's a SUCCESS — leaked).
    assert "404" in out or rc != 0, \
        f"ClearPoison was routed through the gateway: rc={rc} out={out}"
```

Check the exact Envoy Gateway behavior at dispatch — unrouted GRPCRoute match returns either 404 or an RST; the assert above may need tightening once the VM run shows the actual response shape.

## Exit criteria

- `/nbr .#ci` green
- `grep 'phase 5b adds per-method authz' infra/helm/rio-build/templates/dashboard-gateway.yaml` → 0 hits (T1: orphaned deferral removed)
- `grep 'rio-scheduler-readonly\|rio-scheduler-mutating' infra/helm/rio-build/templates/dashboard-gateway.yaml` → ≥2 hits (T1: both route names present)
- `grep 'enableMutatingMethods' infra/helm/rio-build/templates/dashboard-gateway.yaml infra/helm/rio-build/values.yaml` → ≥2 hits (T1+T3)
- `grep 'allowOrigins:' infra/helm/rio-build/templates/dashboard-gateway-policy.yaml | grep '\*'` → 0 hits (T2: wildcard gone)
- `grep 'dashboard\.cors\|toYaml.*allowOrigins' infra/helm/rio-build/templates/dashboard-gateway-policy.yaml` → ≥1 hit (T2: values-driven)
- `helm template --set dashboard.enabled=true infra/helm/rio-build | yq -e 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-mutating")'` → exit 1 (T3: default false → not rendered)
- `helm template --set dashboard.enabled=true --set dashboard.enableMutatingMethods=true infra/helm/rio-build | yq -e 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-mutating")'` → exit 0 (T3: flag works)
- T5: helm-lint check passes with default values (mutating route absent) and fails if ClearPoison leaks into readonly
- T6: VM cli.nix subtest passes — ClearPoison curl through gateway gets 404/UNIMPLEMENTED

## Tracey

References existing markers:
- `r[dash.envoy.grpc-web-translate+2]` — T1/T2 modify the envoy translation layer the marker describes (method split is a routing change inside the same translate pipeline). No bump — routing is implementation detail, not spec contract.
- `r[sched.admin.clear-poison]` — T6's negative test proves ClearPoison is NOT reachable via the dashboard route (the RPC itself is unchanged — handler still works via mTLS rio-cli)

Adds new markers to component specs:
- `r[dash.auth.method-gate]` → `docs/src/components/dashboard.md` (see ## Spec additions below)

## Spec additions

New paragraph in [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md), inserted after the `r[dash.envoy.grpc-web-translate+2]` paragraph (after `:27`):

```
r[dash.auth.method-gate]
The `GRPCRoute` splits `AdminService` methods by impact: read-only methods (`ClusterStatus`, `ListWorkers`, `ListBuilds`, `GetBuildLogs`, `ListTenants`, `GetBuildGraph`, `GetSizeClassStatus`) route unconditionally; mutating methods (`ClearPoison`, `DrainWorker`, `CreateTenant`, `TriggerGC`) route only when `dashboard.enableMutatingMethods` is true (default false). Until dashboard-native authz lands, mutating operations go through `rio-cli` with an mTLS client certificate. CORS `allowOrigins` defaults to the in-cluster nginx Service hostname, not wildcard.
```

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/dashboard-gateway.yaml", "action": "MODIFY", "note": "T1: split GRPCRoute readonly+mutating, delete :74-77 orphaned-deferral comment"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway-policy.yaml", "action": "MODIFY", "note": "T2: allowOrigins wildcard → values-driven; retarget SecurityPolicy to readonly GRPCRoute; delete :10-12 tighten-later comment"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T3: +dashboard.cors.allowOrigins + dashboard.enableMutatingMethods (default false)"},
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T4: +r[dash.auth.method-gate] paragraph after :27"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T5: helm-lint yq asserts — mutating route absent when flag false, ClearPoison not in readonly"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T6: dashboard-gateway ClearPoison curl → 404/UNIMPLEMENTED negative assert"}
]
```

```
infra/helm/rio-build/
├── templates/
│   ├── dashboard-gateway.yaml        # T1: GRPCRoute split
│   └── dashboard-gateway-policy.yaml # T2: CORS tighten
└── values.yaml                       # T3: cors.allowOrigins + enableMutatingMethods
docs/src/components/dashboard.md      # T4: r[dash.auth.method-gate]
flake.nix                             # T5: helm-lint yq asserts
nix/tests/scenarios/cli.nix           # T6: ClearPoison negative curl
```

## Dependencies

```json deps
{"deps": [273, 274], "soft_deps": [304, 357], "note": "discovered_from=273-review. dashboard-gateway.yaml:77 'phase 5b adds per-method authz' is an orphaned deferral — no TODO tag, no plan. Combined with CORS wildcard at dashboard-gateway-policy.yaml:27, ClearPoison/DrainWorker/CreateTenant/TriggerGC are callable from any browser origin. P0273 shipped the GRPCRoute; P0274 shipped the nginx Deployment (unblocking CORS origin tighten). Soft-dep P0304-T105 (adds TODO(P0371) tags as a breadcrumb pointing here — can land before or after; if after, T1 here removes them). Soft-dep P0357 (helm-lint yq pattern at flake.nix:498-575 — T5 follows that shape). flake.nix count=32 (HOT) — T5 is additive yq-assert block in checkPhase, non-overlapping with other flake.nix tasks. dashboard-gateway*.yaml count=2 (low). cli.nix touched by P0273 T5 (trailer grep) + P0304-T43 (trailer grep tighten) — T6 here is additive subtest, non-overlapping."}
```

**Depends on:** [P0273](plan-0273-envoy-sidecar-grpc-web.md) — merged (dashboard-gateway templates exist). [P0274](plan-0274-dashboard-svelte-scaffold.md) — nginx Service hostname needed for CORS origin default.

**Conflicts with:** [P0304](plan-0304-trivial-batch-p0222-harness.md)-T105 adds TODO tags to the same comment lines T1+T2 delete here — sequence either way (T105's tags become moot once T1 lands; T105-first = T1 deletes the tags along with the comment). [P0295](plan-0295-doc-rot-batch-sweep.md)-T62 fixes the `r[impl dash.envoy.grpc-web-translate]` version mismatch in dashboard-gateway.yaml:3 — different line, non-overlapping. [`flake.nix`](../../flake.nix) count=32 — T5 is additive in helm-lint checkPhase; other P0304/P0295 flake.nix tasks touch tracey-validate/mutants blocks, non-overlapping.
