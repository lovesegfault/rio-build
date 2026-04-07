# rio-dashboard

> Web dashboard for operational visibility. Svelte 5 SPA,
> Envoy Gateway gRPC-Web translation, DAG visualization via @xyflow/svelte.

## Architecture

The dashboard is a **Svelte 5** single-page application (`rio-dashboard/`, built
by `nix/dashboard.nix` via `fetchPnpmDeps` + Vite). It does NOT share a process
with any backend component — it is a pure frontend consuming `AdminService` and
`SchedulerService` via gRPC-Web.

**Transport chain (browser → scheduler):**

```
browser (connect-web, gRPC-Web framing)
    │  HTTP/1.1 POST /rio.admin.AdminService/ListBuilds
    ▼
  nginx (baked into the dashboard image, nix/docker.nix)
    │  proxy_buffering off; proxies /rio.* to the Envoy Gateway Service
    ▼
  Envoy Gateway (operator-managed data plane, Gateway API + GRPCRoute)
    │  grpc_web filter auto-injected on GRPCRoute attachment:
    │  HTTP/1.1 gRPC-Web → HTTP/2 gRPC, BackendTLSPolicy presents mTLS client cert
    ▼
  rio-scheduler:9001 (sees a normal mTLS gRPC client — no gRPC-Web awareness)
```

The Envoy data plane is **NOT a sidecar** — it is an operator-managed Deployment
reconciled from `GatewayClass`/`Gateway`/`GRPCRoute` CRDs
(`infra/helm/rio-build/templates/dashboard-gateway*.yaml`). The Envoy Gateway
operator itself is deployed via the nixhelm `gateway-helm` chart. nginx is a thin
HTTP/1.1 proxy that serves the SPA static assets and forwards `/rio.*` to the
operator-generated Envoy Service (`rio-dashboard-envoy.envoy-gateway-system`).

**No Ingress.** Access is via `kubectl port-forward svc/rio-dashboard 8080:80` —
the dashboard is an operator-facing tool (matches the Grafana model, not a public
endpoint). CORS `allowOrigins` defaults to the in-cluster nginx Service hostname.

**Frontend stack:** Svelte 5 runes mode (`$state`/`$effect`/`$props`),
`svelte-routing` for client-side routing, `@connectrpc/connect-web` with
`createGrpcWebTransport` + binary framing, `@xyflow/svelte` for DAG visualization,
`@dagrejs/dagre` for layout (falls back to a Web Worker above 500 nodes).

## Key Views

| Page | Data Source | Description |
|------|-------------|-------------|
| Cluster | `AdminService.ClusterStatus` | Executor/build/derivation counts, entry point to GC |
| Builds | `AdminService.ListBuilds` | Paginated list with status filter + per-build drawer; entry point to the killer journey |
| Graph | `AdminService.GetBuildGraph` | Interactive DAG visualization (`@xyflow/svelte`), color-coded status, degrades to table >2000 nodes |
| Executors | `AdminService.ListExecutors` | Busy/idle pill (one-build-per-pod ⇒ binary), kind filter (builder/fetcher), stale-heartbeat highlight, drain button |
| GC | `AdminService.TriggerGC` (server stream) | Dry-run toggle, grace-period slider, live sweep progress |
| Log viewer | `AdminService.GetBuildLogs` (server stream) | Live-tail build output, UTF-8-lossy decode, embedded in the build drawer |

Executor utilization time-series and cache hit-rate analytics are **NOT** dashboard
scope — they live in the Grafana dashboards (`infra/helm/grafana/`). The
rio-dashboard focuses on interactive per-build detail (DAG, logs, management
actions) that a Prometheus/Grafana stack can't give you.

## Normative requirements

r[dash.envoy.grpc-web-translate+2]
Envoy Gateway (deployed via nixhelm `gateway-helm` chart) translates gRPC-Web (HTTP/1.1 POST from browser fetch) to gRPC over HTTP/2 with mTLS client cert presented to the scheduler. The scheduler is never aware of gRPC-Web — it sees a normal mTLS client. A `GRPCRoute` CRD routes `rio.admin.AdminService` and `rio.scheduler.SchedulerService` methods to `rio-scheduler:9001`; attaching a `GRPCRoute` to a listener automatically injects the `envoy.filters.http.grpc_web` filter into that listener's filter chain (no `EnvoyPatchPolicy` escape hatch). `SecurityPolicy` configures CORS with `grpc-status`/`grpc-message`/`grpc-status-details-bin` in `exposeHeaders`. `BackendTLSPolicy` + `EnvoyProxy.spec.backendTLS.clientCertificateRef` provide upstream mTLS.

r[dash.auth.method-gate+2]
The `GRPCRoute` splits `AdminService` methods by impact: read-only methods (`ClusterStatus`, `ListExecutors`, `ListPoisoned`, `ListBuilds`, `GetBuildLogs`, `ListTenants`, `GetBuildGraph`, `GetSizeClassStatus`) route unconditionally; mutating methods (`ClearPoison`, `DrainExecutor`, `CreateTenant`, `TriggerGC`) route only when `dashboard.enableMutatingMethods` is true (default false). Until dashboard-native authz lands, mutating operations go through `rio-cli` with an mTLS client certificate. CORS `allowOrigins` defaults to the in-cluster nginx Service hostname, not wildcard.

r[dash.journey.build-to-logs]
The killer journey: click build (Builds page) → DAG renders (Graph page) → click running node (DrvNode) → log stream renders (LogViewer). The nginx→Envoy Gateway→scheduler chain MUST support server-streaming end-to-end (verified by the 0x80 trailer-frame byte in curl).

r[dash.graph.degrade-threshold]
Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre layout on >2000 nodes freezes the main thread. Above 500 nodes, dagre runs in a Web Worker. The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`).

r[dash.stream.log-tail]
`GetBuildLogs` server-stream consumption MUST use `TextDecoder('utf-8', {fatal: false})` — build output can contain non-UTF-8 bytes (compiler locale garbage). Lossy decode to `U+FFFD`, never throw. nginx `proxy_buffering off` is required or the stream buffers entirely before reaching the browser.
