# Plan 0273: Envoy sidecar — gRPC-Web → gRPC+mTLS translate (THE GATEKEEPER)

---

**Audit C #25 SCOPE CHANGE (2026-03-18): Envoy Gateway via nixhelm chart, NOT sidecar.**

Supersedes the sidecar design below. Envoy Gateway deploys envoy instances via Kubernetes Gateway API CRDs. Chart comes from nixhelm (`charts.envoyproxy.gateway` or similar — verify at dispatch) pinned by the nixhelm flake rev — single source of truth, zero drift from VM tests.

**Architecture becomes:** Envoy Gateway operator + CRDs → dashboard defines `Gateway` + `HTTPRoute` (or `GRPCRoute`) resources. Envoy instances managed by operator, not hand-configured in Pod spec.

**Unchanged:** scheduler still COMPLETELY untouched. mTLS upstream. R5 still disappears.

**Open question at dispatch:** Envoy Gateway's gRPC-Web filter support. `GRPCRoute` is for gRPC; gRPC-Web translation may need `EnvoyPatchPolicy` escape hatch or `BackendTrafficPolicy`. Verify before writing code. The curl gate (0x80 trailer-frame byte) still applies.

~~Files fence + tasks below need rewrite for Gateway API resources instead of sidecar container + static envoy.yaml.~~ **RESOLVED by P0326** (see Research resolution below).

---

## Research resolution (P0326)

**Outcome A: Gateway API native.** Envoy Gateway's `GRPCRoute` auto-injects `envoy.filters.http.grpc_web` — no `EnvoyPatchPolicy` escape hatch needed. Every sidecar concern maps to a first-class CRD. Full research provenance at [`.claude/notes/envoy-gateway-grpc-web-research.md`](../notes/envoy-gateway-grpc-web-research.md).

### Decision matrix (5 questions × cited answers)

| Q | Question | Answer | Primary source |
|---|---|---|---|
| 1 | Does `GRPCRoute` handle gRPC-Web? | **YES — auto-injected.** When a `GRPCRoute` attaches to a listener, `envoy.filters.http.grpc_web` is added to that listener's HTTP filter chain automatically. No extra CRD or annotation. | [`listener.go:424-425` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/internal/xds/translator/listener.go#L424-L425): `if irListener.IsHTTP2 { mgr.HttpFilters = append(mgr.HttpFilters, xdsfilters.GRPCWeb, ...) }`; [`route.go:1293-1295` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/internal/gatewayapi/route.go#L1293-L1295) sets `IsHTTP2=true` on `KindGRPCRoute`. [Official docs grpc-routing task](https://gateway.envoyproxy.io/docs/tasks/traffic/grpc-routing/): "Envoy Gateway also supports gRPC-Web requests for this configuration" + curl example. Behavior preserved on `main` ([route.go:1300-1302](https://github.com/envoyproxy/gateway/blob/main/internal/gatewayapi/route.go#L1300-L1302)) with `// Backwards compatibility` comment post-PR #8147. |
| 2 | Does `EnvoyPatchPolicy` / `BackendTrafficPolicy` need to inject the filter? | **NO.** `EnvoyPatchPolicy` would only be needed to get grpc_web on an `HTTPRoute`; for `GRPCRoute` the filter is automatic. | [#6934](https://github.com/envoyproxy/gateway/issues/6934) + [#8147](https://github.com/envoyproxy/gateway/pull/8147) discuss the `HTTPRoute` case explicitly; neither is relevant when using `GRPCRoute`. |
| 3 | CORS — can `SecurityPolicy` expose grpc-specific headers? | **YES — arbitrary `exposeHeaders []string`.** `grpc-status`, `grpc-message`, `grpc-status-details-bin` are just strings. | [`api/v1alpha1/cors_types.go` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/cors_types.go): `ExposeHeaders []string`; wired to `SecurityPolicy.spec.cors` at [`securitypolicy_types.go:60-63`](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/securitypolicy_types.go#L60-L63). |
| 4 | mTLS upstream — does `BackendTLSPolicy` present a client cert? | **Standard `BackendTLSPolicy` = CA-verify + SNI only.** Envoy Gateway adds client-cert via `EnvoyProxy.spec.backendTLS.clientCertificateRef` (global) **or** `Backend.spec.tls` (per-backend, needs `extensionApis.enableBackend`). For a single-backend dashboard Gateway, the global EnvoyProxy ref is simplest. | [`envoyproxy_types.go:224-232` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/envoyproxy_types.go#L224-L232): `type BackendTLSConfig struct { ClientCertificateRef *gwapiv1.SecretObjectReference ... }`. Docs tutorial: [`backend-mtls.md`](https://gateway.envoyproxy.io/docs/tasks/security/backend-mtls/). |
| 5 | nixhelm chart availability? | **YES.** `charts.envoyproxy.gateway-helm` @ v1.7.1 + `charts.envoyproxy.gateway-crds-helm`. nixhelm is already a flake input (`flake.nix:73-74`). | [`farcaller/nixhelm/charts/envoyproxy/gateway-helm/default.nix`](https://github.com/farcaller/nixhelm/blob/master/charts/envoyproxy/gateway-helm/default.nix): `repo = "oci://registry-1.docker.io/envoyproxy"; version = "1.7.1";` |

**Rationale for A over C (sidecar):** no escape-hatch fragility — the CRDs are declarative, operator-reconciled, and nixhelm-pinned. Every sidecar concern has a first-class Envoy Gateway equivalent with equal-or-less YAML. The static `envoy-dashboard.yaml` bootstrap becomes ~5 small CRD objects; no hand-tuned xDS, no ConfigMap-mount-into-sidecar wiring.

**Known bug to dodge:** [#7559](https://github.com/envoyproxy/gateway/issues/7559) (open, v1.6.0+) — grpc_web filter is silently omitted when the `GRPCRoute` attaches to a non-first listener on a multi-listener Gateway. **Mitigation:** single-listener Gateway (dashboard needs exactly one HTTP listener anyway). The T1 Gateway below has one listener by design.

---

**USER A1: Scheduler COMPLETELY untouched.** No Rust. No `tonic-web` dep. No third listener. No `#[derive(Clone)]` on `AdminServiceImpl`. R5 (`.accept_http1`) **disappears** — the gRPC-Web filter runs inside the operator-managed Envoy. R3 (streaming) is known-good — Envoy gRPC-Web streaming is battle-tested.

**Architecture (Gateway API, Outcome A):**

```
browser ──HTTP/1.1 gRPC-Web──▶ envoy-gateway Service:8080
                               │ grpc_web filter (auto via GRPCRoute)
                               │ cors filter     (via SecurityPolicy)
                               └─mTLS client cert (EnvoyProxy.backendTLS)──▶ rio-scheduler:9001
                                                                             (scheduler UNCHANGED)
```

The dashboard pod (nginx + SPA) sits beside the Gateway; nginx proxies `/api/*` to the envoy-gateway Service's cluster DNS. Envoy instances are operator-managed — no sidecar container in the dashboard pod spec.

## Entry criteria

none — Wave 0a root. Parallel with [P0274](plan-0274-dashboard-svelte-scaffold.md), [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md).

## Tasks

### T1 — `feat(infra):` Gateway API CRDs (GatewayClass + Gateway + GRPCRoute)

NEW `infra/helm/rio-build/templates/dashboard-gateway.yaml`:

```yaml
# r[impl dash.envoy.grpc-web-translate+2]
#
# Envoy Gateway CRDs. The GRPCRoute attachment triggers grpc_web filter
# injection into the listener's filter chain (Envoy Gateway implementation
# detail — NOT in the Gateway API spec itself). Verified at v1.7.1:
# internal/xds/translator/listener.go:424-425.
#
# SINGLE LISTENER on purpose — dodges upstream bug #7559 where grpc_web
# filter is omitted for non-first listeners.
{{- if .Values.dashboard.enabled }}
---
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: rio-dashboard
spec:
  controllerName: gateway.envoyproxy.io/gatewayclass-controller
  parametersRef:
    group: gateway.envoyproxy.io
    kind: EnvoyProxy
    name: rio-dashboard-proxy
    namespace: {{ .Values.namespace.name }}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: rio-dashboard
  namespace: {{ .Values.namespace.name }}
spec:
  gatewayClassName: rio-dashboard
  listeners:
    - name: http
      protocol: HTTP
      port: 8080
      allowedRoutes:
        kinds:
          - kind: GRPCRoute
---
apiVersion: gateway.networking.k8s.io/v1
kind: GRPCRoute
metadata:
  name: rio-scheduler
  namespace: {{ .Values.namespace.name }}
spec:
  parentRefs:
    - name: rio-dashboard
  rules:
    # Match AdminService + SchedulerService — everything the dashboard
    # calls (see dashboard.md Key Views table). No method gating in MVP.
    - matches:
        - method: { service: rio.admin.AdminService }
        - method: { service: rio.scheduler.SchedulerService }
      backendRefs:
        - name: rio-scheduler
          port: 9001
{{- end }}
```

### T2 — `feat(infra):` mTLS upstream (EnvoyProxy + BackendTLSPolicy + cert)

NEW `infra/helm/rio-build/templates/dashboard-gateway-tls.yaml`:

```yaml
{{- if and .Values.dashboard.enabled .Values.tls.enabled }}
---
# Client cert for gateway → scheduler mTLS. Same issuer chain as the other
# leaf certs (cert-manager.yaml). The scheduler verifies CA-signed only —
# it doesn't check WHICH client (see cert-manager.yaml worker-cert comment).
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: rio-dashboard-envoy-tls
  namespace: {{ .Values.namespace.name }}
spec:
  secretName: rio-dashboard-envoy-tls
  duration: 2160h
  renewBefore: 720h
  privateKey: { algorithm: ECDSA, size: 256, encoding: PKCS8 }
  issuerRef: { kind: Issuer, name: rio-ca-issuer }
  dnsNames:
    - rio-dashboard-envoy
---
# EnvoyProxy: global backend TLS client-cert ref. Applies to any backend
# under this GatewayClass that has a BackendTLSPolicy. For a single-backend
# dashboard Gateway, global is simplest (vs the per-Backend resource path
# which needs extensionApis.enableBackend).
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: EnvoyProxy
metadata:
  name: rio-dashboard-proxy
  namespace: {{ .Values.namespace.name }}
spec:
  backendTLS:
    clientCertificateRef:
      kind: Secret
      name: rio-dashboard-envoy-tls
---
# BackendTLSPolicy: CA validation + SNI for the scheduler upstream. Standard
# Gateway API (not an Envoy Gateway extension). The CA is the shared rio
# root CA; hostname MUST match a SAN on rio-scheduler-tls (see cert-manager
# dnsNames: short form `rio-scheduler` is first).
apiVersion: gateway.networking.k8s.io/v1alpha3
kind: BackendTLSPolicy
metadata:
  name: rio-scheduler-mtls
  namespace: {{ .Values.namespace.name }}
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: rio-scheduler
  validation:
    caCertificateRefs:
      - group: ""
        kind: Secret
        name: rio-scheduler-tls  # ca.crt key inside the cert-manager secret
    hostname: rio-scheduler
{{- end }}
```

**Verify at dispatch:** `BackendTLSPolicy` expects the CA in a ConfigMap or Secret key named `ca.crt`. cert-manager's Certificate secrets include `ca.crt` alongside `tls.crt`/`tls.key` when the issuer is a CA issuer — confirm with `kubectl get secret rio-scheduler-tls -o jsonpath='{.data}' | jq keys`.

### T3 — `feat(infra):` CORS (SecurityPolicy) + streaming timeout

NEW `infra/helm/rio-build/templates/dashboard-gateway-policy.yaml`:

```yaml
{{- if .Values.dashboard.enabled }}
---
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: SecurityPolicy
metadata:
  name: rio-dashboard-cors
  namespace: {{ .Values.namespace.name }}
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: rio-dashboard
  cors:
    # Permissive MVP — tighten to dashboard's served origin post-MVP.
    allowOrigins:
      - "*"
    allowMethods:
      - POST
      - OPTIONS
    allowHeaders:
      - content-type
      - x-grpc-web
      - x-user-agent
      - grpc-timeout
    exposeHeaders:
      - grpc-status
      - grpc-message
      - grpc-status-details-bin
---
# Streaming timeout: GetBuildLogs / WatchBuild are long-lived server-streams.
# Default request timeout would cut the stream. ClientTrafficPolicy sets
# idle/stream timeout at the listener.
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: ClientTrafficPolicy
metadata:
  name: rio-dashboard-streaming
  namespace: {{ .Values.namespace.name }}
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: rio-dashboard
  timeout:
    http:
      # 0 = no idle timeout; streams stay open until either side closes.
      # (verify at dispatch: Envoy Gateway's ClientTrafficPolicy may use
      # requestTimeout: 0s or idleTimeout: 0s — check extension_types docs)
      idleTimeout: 0s
{{- end }}
```

### T4 — `feat(nix):` Envoy Gateway chart from nixhelm

MODIFY [`nix/helm-charts.nix`](../../nix/helm-charts.nix) — add Envoy Gateway operator + CRDs charts:

```nix
  # Envoy Gateway: operator + Gateway API / EG extension CRDs. Dashboard's
  # gRPC-Web → gRPC+mTLS translation. GRPCRoute auto-injects the
  # envoy.filters.http.grpc_web filter (v1.7.1 listener.go:424-425).
  inherit (charts.envoyproxy) gateway-helm gateway-crds-helm;
```

Wire into VM test fixtures + `just dev` install sequence (CRDs → operator → wait → rio chart's dashboard-gateway templates). Check `nix/tests/lib/derivations.nix` or equivalent for where other nixhelm charts (postgresql, rook) are pulled into the VM — follow that pattern.

### T5 — `test(vm):` curl gate — unary + streaming through Envoy Gateway

Same 0x80 trailer-grep as the sidecar plan, but the curl target is the Envoy Gateway Service (`envoy-rio-dashboard-<hash>` or a stable DNS name — check what the operator creates; Envoy Gateway names its Service `envoy-{gateway-ns}-{gateway-name}-{hash}` by default; a stable name needs `EnvoyProxy.spec.provider.kubernetes.envoyService.name`). For VM tests, query `kubectl get svc -l gateway.envoyproxy.io/owning-gateway-name=rio-dashboard` to find the port-forward target.

```python
# r[verify dash.envoy.grpc-web-translate+2]  (col-0 header comment)
# R1 de-risk: gRPC-Web framing works (unary ClusterStatus via Envoy Gateway)
svc = machine.succeed(
    "kubectl -n rio-system get svc "
    "-l gateway.envoyproxy.io/owning-gateway-name=rio-dashboard "
    "-o jsonpath='{.items[0].metadata.name}'"
).strip()
machine.succeed(
    f"curl -sf -X POST http://{svc}.rio-system:8080/rio.admin.AdminService/ClusterStatus "
    "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
    r"--data-binary $'\x00\x00\x00\x00\x00' "
    "| xxd | head -1 | grep -q '^00000000: 00'"  # DATA frame, compression=0
)
# R3 de-risk: server-streaming through Envoy Gateway BEFORE any TS exists.
# 0x80 trailer-frame prefix proves grpc_web filter doesn't buffer the stream.
# xxd -p | tail -c 50 | grep -q '^80' anchors on the trailer-frame start
# (type byte 0x80); a loose ' 80' grep matches ANY 0x80 in the last ~80 bytes.
machine.succeed(
    "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' > /tmp/req.bin && "
    f"curl -sf -X POST http://{svc}.rio-system:8080/rio.admin.AdminService/GetBuildLogs "
    "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
    "--data-binary @/tmp/req.bin | xxd -p | tail -c 50 | grep -q '^80'"
)
```

**The `0x80` grep is the single most important assertion in Wave 0** — it proves server-streaming works end-to-end through the operator-managed Envoy before a single line of Svelte.

## Exit criteria

- `/nbr .#ci` green
- Both curl assertions pass: DATA frame `0x00` prefix + trailer frame `0x80` byte
- `git diff sprint-1 -- rio-scheduler/` → EMPTY (USER A1: scheduler untouched)
- `kubectl get grpcroute,backendtlspolicy,securitypolicy -n rio-system` in VM → all Accepted/Programmed
- `egctl config envoy-proxy listener -n envoy-gateway-system` (or equivalent) → `envoy.filters.http.grpc_web` present in the listener's http_filters chain (proves the auto-inject actually fired, not just that the CRDs applied)

## Tracey

References existing marker (bumped by [P0326](plan-0326-envoy-gateway-grpc-web-research.md) to reflect Gateway API architecture):
- `r[dash.envoy.grpc-web-translate+2]` — T1 impl (GRPCRoute CRDs), T5 verify (curl gate)

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/dashboard-gateway.yaml", "action": "NEW", "note": "T1: GatewayClass + Gateway + GRPCRoute (grpc_web auto-injected)"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway-tls.yaml", "action": "NEW", "note": "T2: cert-manager Certificate + EnvoyProxy.backendTLS.clientCertificateRef + BackendTLSPolicy"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway-policy.yaml", "action": "NEW", "note": "T3: SecurityPolicy (CORS) + ClientTrafficPolicy (streaming timeout)"},
  {"path": "nix/helm-charts.nix", "action": "MODIFY", "note": "T4: inherit envoyproxy gateway-helm + gateway-crds-helm from nixhelm"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T5: curl gate — unary + 0x80 trailer grep via Envoy Gateway Service"}
]
```

```
infra/helm/rio-build/templates/
├── dashboard-gateway.yaml        # T1: NEW — GatewayClass + Gateway + GRPCRoute
├── dashboard-gateway-tls.yaml    # T2: NEW — Certificate + EnvoyProxy + BackendTLSPolicy
└── dashboard-gateway-policy.yaml # T3: NEW — SecurityPolicy + ClientTrafficPolicy
nix/
├── helm-charts.nix               # T4: envoy-gateway charts from nixhelm
└── tests/scenarios/cli.nix       # T5: curl gate
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [], "note": "Envoy Gateway via nixhelm (P0326 research: Outcome A — GRPCRoute native grpc_web, no EnvoyPatchPolicy). Scheduler UNTOUCHED. Deps on P0245: T5's r[verify dash.envoy.grpc-web-translate+2] needs the marker seeded. Parallel with P0274/P0276 — zero file overlap. R3/R5 de-risked by Envoy. EXIT: git diff rio-scheduler/ = EMPTY. Known bug dodge: single-listener Gateway avoids #7559."}
```

**Depends on:** P0245 (tracey marker seed). Wave 0a root after P0245.
**Conflicts with:** none. All NEW files or low-collision (`nix/helm-charts.nix` additive inherit; `cli.nix` additive test fragment). Zero scheduler touch per USER A1.
