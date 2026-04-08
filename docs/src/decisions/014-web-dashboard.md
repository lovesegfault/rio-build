# ADR-014: Web Dashboard

## Status
Accepted

## Status update
The implemented dashboard uses **Svelte 5** (runes mode), not React. The
gRPC-Web transport, separate-Deployment model, and Phase-5 scope below
are unchanged. See [components/dashboard.md](../components/dashboard.md).

## Context
Operators and developers need visibility into the build system: what is building, what failed, how long builds take, cache hit rates, and worker health. CLI tools provide point-in-time queries but lack the interactive exploration and visualization needed for operational dashboards.

## Decision
A TypeScript SPA built with React, consuming the AdminService gRPC-Web API. Deployed as a separate Kubernetes Deployment (`rio-dashboard`) serving static assets via nginx or a lightweight HTTP server.

Dashboard features include:
- Real-time build status and log viewer.
- DAG visualization showing derivation dependencies and build progress.
- Worker utilization and health metrics.
- Cache hit rate analytics.
- Build history with filtering and search.

This is a Phase 5 deliverable, built after the core backend is stable.

## Alternatives Considered
- **Grafana dashboards only**: Leverage existing observability infrastructure. Good for metrics but cannot display build logs, DAG visualizations, or provide interactive build management. Also requires Grafana expertise to maintain.
- **Terminal UI (TUI)**: A `rio-cli` tool with a rich terminal interface (ratatui or similar). Good for power users but limited for team-wide visibility, shareable links, and mobile access. Does not replace a web dashboard for operational use.
- **Server-rendered web app (e.g., htmx, Leptos)**: Avoids the SPA complexity. However, real-time build log streaming and interactive DAG visualization benefit from a rich client-side runtime. gRPC-Web integration is more natural in a JavaScript/TypeScript SPA.
- **Embed UI in the gateway binary (single binary)**: Serve the dashboard from the same binary as the gRPC gateway. Simpler deployment but couples dashboard release cycles to the backend. Static asset serving does not justify the complexity of embedding in a Rust binary.
- **Vue.js or Svelte instead of React**: All are viable. React was chosen for ecosystem breadth (gRPC-Web libraries, DAG visualization libraries like dagre/elkjs, charting) and hiring familiarity.

## Consequences
- **Positive**: Rich interactive UI for build monitoring, log viewing, and DAG visualization.
- **Positive**: Separate deployment allows independent scaling and release cadence.
- **Positive**: gRPC-Web provides type-safe API integration with the backend.
- **Negative**: Adds a frontend technology stack (TypeScript, React, npm) to a primarily Rust/Nix project.
- **Negative**: Phase 5 timeline means operators rely on CLI tools and Grafana for visibility during early phases.
- **Negative**: gRPC-Web requires a proxy (Envoy or grpc-web-proxy) between the browser and the gRPC backend.
