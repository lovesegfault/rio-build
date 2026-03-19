# Envoy Gateway gRPC-Web support — research (P0326)

**Date:** 2026-03-19
**Resolves:** P0273 Audit C #25 scope-change block ("Implementer: rewrite tasks after verifying Gateway API gRPC-Web support path")
**Outcome:** **A — Gateway API native.** GRPCRoute auto-enables `envoy.filters.http.grpc_web`. No EnvoyPatchPolicy escape hatch needed.

## Question matrix (primary-source answers)

### Q1: Does `GRPCRoute` handle gRPC-Web?

**YES — automatically.** When a `GRPCRoute` attaches to a listener, Envoy Gateway injects the `envoy.filters.http.grpc_web` filter into that listener's HTTP filter chain. No additional CRD, annotation, or escape-hatch configuration is required.

| Source | Evidence |
|---|---|
| [`internal/xds/translator/listener.go:424-425` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/internal/xds/translator/listener.go#L424-L425) | `if irListener.IsHTTP2 { mgr.HttpFilters = append(mgr.HttpFilters, xdsfilters.GRPCWeb, xdsfilters.GRPCStats) }` |
| [`internal/gatewayapi/route.go:1293-1295` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/internal/gatewayapi/route.go#L1293-L1295) | `if route.GetRouteType() == resource.KindGRPCRoute { irListener.IsHTTP2 = true }` |
| [`internal/xds/filters/wellknown.go:22,29-30` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/internal/xds/filters/wellknown.go#L22-L30) | `var GRPCWeb *hcm.HttpFilter` — uses `envoy.extensions.filters.http.grpc_web.v3.GrpcWeb` (the real filter) |
| [`site/content/en/latest/tasks/traffic/grpc-routing.md:78-85`](https://gateway.envoyproxy.io/docs/tasks/traffic/grpc-routing/) | **Official docs:** "Envoy Gateway also supports gRPC-Web requests for this configuration" + curl example with `Content-Type: application/grpc-web-text` |
| [Gateway API `GRPCRoute` spec](https://gateway-api.sigs.k8s.io/api-types/grpcroute/) | Upstream spec does **not** mention gRPC-Web. The filter injection is an Envoy Gateway implementation choice, not a Gateway API requirement. |

**Stability across versions:** PR [#8147](https://github.com/envoyproxy/gateway/pull/8147) (merged 2026-03-06, post-1.7.1) refactored the flag to `irListener.GRPC.EnableGRPCWeb`, but `main`'s [`route.go:1300-1302`](https://github.com/envoyproxy/gateway/blob/main/internal/gatewayapi/route.go#L1300-L1302) still auto-sets it for GRPCRoute with an explicit `// Backwards compatibility` comment. The behavior is intentionally preserved.

### Q2: Does EnvoyPatchPolicy / BackendTrafficPolicy need to inject the filter?

**NO.** Not for our use case. The escape hatch would only be needed if we wanted grpc_web on an `HTTPRoute` (not `GRPCRoute`) — that's what issue [#6934](https://github.com/envoyproxy/gateway/issues/6934) and PR [#8147](https://github.com/envoyproxy/gateway/pull/8147) address via `ClientTrafficPolicy`. Since we're routing to a single gRPC backend (`rio-scheduler`), `GRPCRoute` is the natural fit and brings grpc_web for free.

### Q3: CORS — can `SecurityPolicy` expose grpc-specific headers?

**YES — first-class.** `SecurityPolicy.spec.cors.exposeHeaders []string` takes arbitrary header names. No special-casing needed for `grpc-status`/`grpc-message`/`grpc-status-details-bin`.

| Source | Evidence |
|---|---|
| [`api/v1alpha1/cors_types.go` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/cors_types.go) | `type CORS struct { ... ExposeHeaders []string \`json:"exposeHeaders,omitempty"\` ... }` |
| [`api/v1alpha1/securitypolicy_types.go:60-63` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/securitypolicy_types.go#L60-L63) | `CORS *CORS \`json:"cors,omitempty"\`` in `SecurityPolicy` spec |

### Q4: mTLS upstream to scheduler — does BackendTLSPolicy do client-cert?

**YES — two paths.** Gateway API's standard `BackendTLSPolicy` handles **CA validation + SNI hostname** only (server-side). Envoy Gateway extends with client-cert presentation via either:

| Path | CRD | Scope | Source |
|---|---|---|---|
| Global | `EnvoyProxy.spec.backendTLS.clientCertificateRef` | All TLS backends under the GatewayClass/Gateway | [`api/v1alpha1/envoyproxy_types.go:224-232` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/envoyproxy_types.go#L224-L232) |
| Per-backend | `Backend.spec.tls` (embeds `BackendTLSConfig`) | One backend; requires `extensionApis.enableBackend: true` | [`api/v1alpha1/backend_types.go:164-227` @ v1.7.1](https://github.com/envoyproxy/gateway/blob/v1.7.1/api/v1alpha1/backend_types.go#L164-L227) |
| Docs | — | — | [`site/content/en/latest/tasks/security/backend-mtls.md`](https://gateway.envoyproxy.io/docs/tasks/security/backend-mtls/) — complete tutorial with YAML |

For rio's single-backend (scheduler only via this Gateway), the **global** `EnvoyProxy.backendTLS.clientCertificateRef` is simpler. The cert-manager `Certificate` resource already generates `rio-<component>-tls` Secrets — we add one for `rio-dashboard-envoy` and reference it.

### Q5: nixhelm chart availability?

**YES — already pinned.** nixhelm carries both charts:

| Chart | Path | Version |
|---|---|---|
| Envoy Gateway (operator + controller) | `charts.envoyproxy.gateway-helm` | 1.7.1 |
| CRDs (Gateway API + Envoy Gateway extension CRDs) | `charts.envoyproxy.gateway-crds-helm` | — |

Source: [`farcaller/nixhelm/charts/envoyproxy/gateway-helm/default.nix`](https://github.com/farcaller/nixhelm/blob/master/charts/envoyproxy/gateway-helm/default.nix) → `repo = "oci://registry-1.docker.io/envoyproxy"; chart = "gateway-helm"; version = "1.7.1";`

nixhelm is already a flake input (`flake.nix:73-74`). Wire into `nix/helm-charts.nix` via `inherit (charts.envoyproxy) gateway-helm gateway-crds-helm;`.

## Known bugs / caveats

- **[#7559](https://github.com/envoyproxy/gateway/issues/7559)** (open, v1.6.0+): When a Gateway has multiple listeners and the GRPCRoute attaches to a non-first listener, the grpc_web filter is silently **not** added. Workaround: single-listener Gateway, or attach GRPCRoute to listener index 0. For a dashboard-only Gateway, a single HTTP listener on `:8080` is correct anyway — non-issue.
- **Scheduler port:** existing `scheduler.yaml` Service exposes port **9001** (not 8443 as P0273's original sidecar config assumed). P0273's `25173c72` worktree reportedly already fixed this; the rewritten tasks use 9001.
- **`appProtocol: kubernetes.io/h2c` or `appProtocol: grpc`:** scheduler Service's port has no `appProtocol` hint today. Envoy Gateway's GRPCRoute backend doesn't strictly require it (it forces HTTP/2 upstream), but setting `appProtocol: https` (or leaving it) on the scheduler Service port helps Envoy know to speak h2-over-TLS. Verify at dispatch whether `BackendTLSPolicy` + GRPCRoute needs an explicit protocol hint.

## Decision: Outcome A — rewrite P0273 for Gateway API CRDs

**Rationale:** Every sidecar concern maps to a first-class Envoy Gateway CRD:

| Sidecar config | Gateway API equivalent | Net complexity |
|---|---|---|
| `envoy.filters.http.grpc_web` in `http_filters` | `GRPCRoute` (auto-injected) | **-1 file** (no static envoy.yaml) |
| CORS vhost config | `SecurityPolicy.spec.cors` | Same |
| `transport_socket` mTLS client cert | `EnvoyProxy.backendTLS.clientCertificateRef` + `BackendTLSPolicy` | Same, but Secret-ref instead of filesystem mount |
| `cluster.load_assignment` DNS | `GRPCRoute.spec.rules[].backendRefs[]` → Service | **-1 concern** (operator manages DNS) |
| Envoy image pull + pod spec | Envoy Gateway operator | **-1 file** (no sidecar container spec) |

The sidecar approach needs a ~50-line static envoy.yaml + ConfigMap + sidecar container spec + volume mount wiring. The Gateway API approach needs ~5 small CRD manifests (Gateway, GRPCRoute, SecurityPolicy, BackendTLSPolicy, EnvoyProxy) totaling roughly the same line count — but each piece is a well-known Kubernetes primitive with operator-managed drift recovery, not a hand-tuned xDS blob.

**Outcome B's trap doesn't apply:** no `EnvoyPatchPolicy` or JSONPatch against generated xDS names is needed. This is genuinely simpler-than-sidecar.

## Files changed by P0326

- `.claude/work/plan-0273-envoy-sidecar-grpc-web.md` — Research resolution section inserted after Audit C block; Tasks section rewritten for Gateway API CRDs
- `docs/src/components/dashboard.md` — `r[dash.envoy.grpc-web-translate]` text updated sidecar → Envoy Gateway; `tracey bump` → `+2`
- `.claude/dag.jsonl` — P0273 row: title + `note` updated to reflect Gateway API tasks
