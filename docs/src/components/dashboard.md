# rio-dashboard

> **Status: Preliminary design --- Phase 5 deliverable. Technology choices are not finalized.**

A TypeScript SPA providing a web-based view of the rio-build cluster.

## Architecture

- **Frontend**: React SPA compiled to static assets
- **Deployment**: Separate K8s Deployment serving static assets via nginx or a lightweight container
- **API**: Consumes `AdminService` and `SchedulerService` via gRPC-Web
- **gRPC-Web proxy**: Envoy sidecar or `tonic-web` middleware on the scheduler/admin server
- **Authentication**: OIDC integration or SSH-key-to-tenant mapping shared with the gateway

The dashboard does NOT share a process with any backend component. It is a pure frontend application.

## Key Views

| View | Data Source | Description |
|------|------------|-------------|
| Build list | `SchedulerService.QueryBuildStatus` | Status, timing, requestor, derivation counts, cache hit rate per build |
| DAG visualization | `SchedulerService.SubmitBuild` (BuildEvent stream) | Interactive derivation graph with color-coded status (queued/building/completed/cached/failed) |
| Worker utilization | `AdminService.ListWorkers` | Current load, builds/hour, local store size, resource usage per worker |
| Cache analytics | `AdminService.ClusterStatus` | Global cache hit rate, chunk dedup ratio, storage usage, transfer volumes |
| Build log viewer | `SchedulerService.SubmitBuild` (BuildEvent.BuildLog) | Real-time streamed logs via gRPC-Web server streaming |

## Technology Stack

This is a TypeScript project, not a Rust crate. It lives in the workspace root as `rio-dashboard/` with its own `package.json` and build toolchain (Node/Bun).

| Concern | Choice |
|---------|--------|
| Framework | React |
| gRPC-Web | `@improbable-eng/grpc-web` or `grpc-web` |
| Charts | Recharts, D3, or similar |
| DAG rendering | dagre-d3 or Elk.js |
| Build toolchain | Vite |

## Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rio-dashboard
spec:
  replicas: 2
  template:
    spec:
      containers:
        - name: dashboard
          image: rio-dashboard:latest
          ports:
            - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: rio-dashboard
spec:
  type: ClusterIP
  ports:
    - port: 80
      targetPort: 8080
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: rio-dashboard
spec:
  rules:
    - host: rio.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: rio-dashboard
                port:
                  number: 80
```
