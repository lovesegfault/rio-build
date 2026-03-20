# Plan 0277: App shell + ClusterStatus (INTEGRATION PROOF)

**USER A7: Svelte.** First plan that makes a real network call from the browser. [P0273](plan-0273-envoy-sidecar-grpc-web.md)'s curl proved Envoy→scheduler works. Curl speaks h2c and doesn't care about CORS. **Browsers use HTTP/1.1 `fetch()` and DO preflight OPTIONS.** Envoy handles both natively (USER A1) — R5 is already de-risked, but this is the first browser-verification point.

The killer journey starts here: `r[dash.journey.build-to-logs]` — "click build → DAG shows → click node → log stream." This plan is the "load the page" prerequisite.

## Entry criteria

- [P0273](plan-0273-envoy-sidecar-grpc-web.md) merged (Envoy proxies gRPC-Web)
- [P0275](plan-0275-proto-ts-codegen-buf.md) merged (`src/gen/admin_connect.ts` exists)

## Tasks

### T1 — `feat(dashboard):` transport + client setup

NEW `rio-dashboard/src/api/transport.ts`:
```typescript
// Audit B2 #20: createGrpcWebTransport, NOT createConnectTransport.
// Envoy's grpc_web filter speaks gRPC-Web protocol (application/grpc-web+proto),
// not Connect protocol. P0273:98 validates with that content-type.
import { createGrpcWebTransport } from "@connectrpc/connect-web";
// baseUrl '/' — prod: nginx proxies /rio.admin.AdminService/* → envoy:8080.
// dev: vite.config.ts server.proxy forwards to Envoy. ONE build artifact.
export const transport = createGrpcWebTransport({ baseUrl: "/", useBinaryFormat: true });
```

NEW `rio-dashboard/src/api/admin.ts`:
```typescript
import { createClient } from "@connectrpc/connect";
import { AdminService } from "../gen/admin_connect";
import { transport } from "./transport";
export const admin = createClient(AdminService, transport);
```

### T2 — `feat(dashboard):` routing (Svelte 5)

`package.json` — add `svelte-routing` or a lightweight SPA router. `pnpmDeps` hash bump.

Rewrite `src/App.svelte`:
```svelte
<script lang="ts">
  import { Router, Route } from "svelte-routing";
  import Cluster from "./pages/Cluster.svelte";
  import Builds from "./pages/Builds.svelte";
  import Workers from "./pages/Workers.svelte";
</script>
<Router>
  <nav><!-- sidebar links --></nav>
  <main>
    <Route path="/" component={Cluster} />
    <Route path="/builds" component={Builds} />
    <Route path="/builds/:id" component={Builds} />
    <Route path="/workers" component={Workers} />
  </main>
</Router>
```

### T3 — `feat(dashboard):` Cluster page — first real RPC

NEW `rio-dashboard/src/pages/Cluster.svelte`:
```svelte
<script lang="ts">
  import { admin } from "../api/admin";
  import type { ClusterStatusResponse } from "../gen/types_pb";

  let status = $state<ClusterStatusResponse | null>(null);
  let error = $state<string | null>(null);

  $effect(() => {
    const id = setInterval(async () => {
      try {
        status = await admin.clusterStatus({});
        error = null;
      } catch (e) {
        error = String(e);
      }
    }, 5000);
    // initial fetch
    admin.clusterStatus({}).then(r => status = r).catch(e => error = String(e));
    return () => clearInterval(id);
  });
</script>

{#if error}
  <div role="alert">scheduler unreachable: {error}</div>
{:else if !status}
  <div>loading…</div>
{:else}
  <dl data-testid="cluster-status">
    <dt>Workers</dt><dd>{status.activeWorkers} active / {status.totalWorkers} total</dd>
    <dt>Builds</dt><dd>{status.pendingBuilds} pending / {status.activeBuilds} active</dd>
    <dt>Derivations</dt><dd>{status.queuedDerivations} queued / {status.runningDerivations} running</dd>
  </dl>
{/if}
```

### T4 — `test(dashboard):` mock clusterStatus

NEW `rio-dashboard/src/pages/__tests__/Cluster.test.ts`:
```typescript
import { vi } from "vitest";
vi.mock("../../api/admin");
// Mock admin.clusterStatus → {activeWorkers: 3, ...}. render(Cluster). assert "3 active" in DOM.
// Mock reject → assert role="alert".
```

## Exit criteria

- `/nbr .#ci` green
- **Manual (documented, not gating):** `kubectl port-forward pod/rio-dashboard-... 8080` + `pnpm dev` → `http://localhost:5173` shows cluster counts. Browser devtools Network tab: OPTIONS preflight 200 with `access-control-allow-*`, POST 200 with `grpc-status: 0` in exposed response headers.

## Tracey

References existing markers:
- `r[dash.journey.build-to-logs]` — T3 is the first step (partial impl; full journey closed by P0280's node-click → P0279's log stream). Seeded by P0284.

## Files

```json files
[
  {"path": "rio-dashboard/src/api/transport.ts", "action": "NEW", "note": "T1: createGrpcWebTransport (Audit B2 #20)"},
  {"path": "rio-dashboard/src/api/admin.ts", "action": "NEW", "note": "T1: createClient(AdminService)"},
  {"path": "rio-dashboard/src/App.svelte", "action": "MODIFY", "note": "T2: router + nav (USER A7: Svelte)"},
  {"path": "rio-dashboard/src/pages/Cluster.svelte", "action": "NEW", "note": "T3: $state/$effect runes, 5s poll"},
  {"path": "rio-dashboard/src/pages/__tests__/Cluster.test.ts", "action": "NEW", "note": "T4: mock test"},
  {"path": "rio-dashboard/package.json", "action": "MODIFY", "note": "T2: router dep (pnpmDeps hash bump)"},
  {"path": "nix/dashboard.nix", "action": "MODIFY", "note": "A8: pnpmDeps hash bump"}
]
```

```
rio-dashboard/src/
├── api/
│   ├── transport.ts              # T1
│   └── admin.ts                  # T1
├── App.svelte                    # T2: router (Svelte)
└── pages/
    ├── Cluster.svelte            # T3: first RPC
    └── __tests__/Cluster.test.ts # T4
```

## Dependencies

```json deps
{"deps": [273, 275], "soft_deps": [], "note": "USER A7: Svelte 5 runes ($state/$effect). Y-join: needs both Envoy (P0273) AND codegen (P0275). First browser verify — Envoy handles CORS+HTTP1.1 natively (R5 de-risked by A1)."}
```

**Depends on:** [P0273](plan-0273-envoy-sidecar-grpc-web.md) — Envoy proxies. [P0275](plan-0275-proto-ts-codegen-buf.md) — `admin_connect.ts` exists.
**Conflicts with:** `package.json`/`nix/dashboard.nix` serial via dep chain. All other files NEW.
