# rio-dashboard

> **Phase 5:** Web dashboard for operational visibility. Svelte 5 SPA,
> Envoy-sidecar gRPC-Web translation, DAG visualization via @xyflow/svelte.

## Architecture

See `infra/helm/rio-build/templates/dashboard-*.yaml`.

The dashboard does NOT share a process with any backend component. It is
a pure frontend application consuming `AdminService` and `SchedulerService`
via gRPC-Web through an Envoy sidecar.

## Key Views

| View | Data Source | Description |
|------|------------|-------------|
| Build list | `SchedulerService.QueryBuildStatus` | Status, timing, requestor, derivation counts, cache hit rate per build |
| DAG visualization | `SchedulerService.WatchBuild` (BuildEvent stream) | Interactive derivation graph with color-coded status |
| Worker utilization | `AdminService.ListWorkers` | Current load, builds/hour, local store size, resource usage per worker |
| Cache analytics | `AdminService.ClusterStatus` | Global cache hit rate, chunk dedup ratio, storage usage, transfer volumes |
| Build log viewer | `SchedulerService.GetBuildLogs` | Real-time streamed logs via gRPC-Web server streaming |

## Normative requirements

r[dash.envoy.grpc-web-translate]

The dashboard pod's Envoy sidecar translates gRPC-Web (HTTP/1.1 POST from browser fetch) to gRPC over HTTP/2 with mTLS client cert presented to the scheduler. The scheduler is never aware of gRPC-Web — it sees a normal mTLS client. CORS preflight and the `grpc-web` filter are Envoy-side.

r[dash.journey.build-to-logs]

The killer journey: click build (Builds page) → DAG renders (Graph page) → click running node (DrvNode) → log stream renders (LogViewer). The nginx→Envoy→scheduler chain MUST support server-streaming end-to-end (verified by the 0x80 trailer-frame byte in curl).

r[dash.graph.degrade-threshold]

Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre layout on >2000 nodes freezes the main thread. Above 500 nodes, dagre runs in a Web Worker. The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`).

r[dash.stream.log-tail]

`GetBuildLogs` server-stream consumption MUST use `TextDecoder('utf-8', {fatal: false})` — build output can contain non-UTF-8 bytes (compiler locale garbage). Lossy decode to `U+FFFD`, never throw. nginx `proxy_buffering off` is required or the stream buffers entirely before reaching the browser.
