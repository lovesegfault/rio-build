# Plan 0280: DAG visualization — @xyflow/svelte (THE DIFFERENTIATOR)

**USER A7: `@xyflow/svelte`, NOT `@xyflow/react`.** Svelte's compile-time reactivity avoids VDOM reconciliation — R6 (500+ node freeze) is LESS of a risk. But dagre layout on 1000+ nodes still freezes main thread → WebWorker above 500 nodes. `>2000` nodes → degrade to sortable table (`r[dash.graph.degrade-threshold]` is normative).

Server caps at 5000 ([P0276](plan-0276-getbuildgraph-rpc-pg-backed.md)'s `DASHBOARD_GRAPH_NODE_LIMIT`). UI additionally degrades at 2000 — xyflow can RENDER 5000, but dagre LAYOUT takes ~8s on 5000 nodes. 2000 is the interactive ceiling.

> **DISPATCH NOTE (rev-p280, docs-594354):** [P0252](plan-0252-ca-cutoff-propagate-skipped.md) landed `DerivationStatus::Skipped` on sprint-1 AFTER this plan's worktree forked. After rebase, `GetBuildGraph` returns `skipped` nodes but T2's `STATUS_CLASS`/`SORT_RANK` maps don't have an arm for it — falls through to gray (indistinguishable from queued). **Include `skipped: "status-complete"` (green) + sort-rank 3 (with completed) in T2's maps.** Also: T3's shared `Worker` + one-shot listeners race when 5s polls overlap (slow network / heavy dagre) — the N+1 promise resolves with N's stale positions. Add an inflight gate to T4's `fetchAndLayout` OR seq-number correlation to T3's WorkerRequest/Response. [P0400](plan-0400-graph-page-skipped-worker-race.md) tracks both as a post-merge sweep; absorbing them here reduces that plan to test-only.

## Entry criteria

- [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) merged (`GetBuildGraph` RPC exists)
- [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) merged (transport)

## Tasks

### T1 — `feat(dashboard):` deps

`package.json` — add `@xyflow/svelte`, `@dagrejs/dagre`. Hash bump.

### T2 — `feat(dashboard):` layout module

NEW `rio-dashboard/src/lib/graphLayout.ts`:

```typescript
import dagre from "@dagrejs/dagre";
import type { GraphNode, GraphEdge } from "../gen/types_pb";
import type { Node, Edge } from "@xyflow/svelte";

// r[impl dash.graph.degrade-threshold]
export const DEGRADE_THRESHOLD = 2000;
export const WORKER_THRESHOLD = 500;  // dagre in WebWorker above this

export type LayoutResult =
  | { degraded: false; nodes: Node[]; edges: Edge[] }
  | { degraded: true; reason: string };

export function layoutGraph(gn: GraphNode[], ge: GraphEdge[]): LayoutResult {
  if (gn.length > DEGRADE_THRESHOLD) {
    return { degraded: true, reason: `${gn.length} nodes > ${DEGRADE_THRESHOLD}` };
  }
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "TB", nodesep: 40, ranksep: 80 });
  g.setDefaultEdgeLabel(() => ({}));
  for (const n of gn) g.setNode(n.drvPath, { width: 180, height: 40 });
  for (const e of ge) g.setEdge(e.childDrvPath, e.parentDrvPath);  // child→parent = build order
  dagre.layout(g);
  return {
    degraded: false,
    nodes: gn.map(n => {
      const { x, y } = g.node(n.drvPath);
      return {
        id: n.drvPath, position: { x, y }, type: "drvNode",
        data: { pname: n.pname, status: n.status, workerId: n.assignedWorkerId },
      };
    }),
    edges: ge.map(e => ({
      id: `${e.childDrvPath}→${e.parentDrvPath}`,
      source: e.childDrvPath, target: e.parentDrvPath,
      style: e.isCutoff ? "stroke-dasharray: 5,5" : undefined,  // CA cutoff edges dashed
    })),
  };
}
```

### T3 — `feat(dashboard):` WebWorker for >500 nodes

NEW `rio-dashboard/src/lib/graphLayout.worker.ts` — `onmessage` → dagre → `postMessage({positions})`. Main thread: `new Worker(new URL('./graphLayout.worker.ts', import.meta.url))` (Vite handles).

### T4 — `feat(dashboard):` Graph page

NEW `rio-dashboard/src/pages/Graph.svelte`:
```svelte
<script lang="ts">
  import { SvelteFlow } from "@xyflow/svelte";
  import { admin } from "../api/admin";
  import { layoutGraph, DEGRADE_THRESHOLD, WORKER_THRESHOLD } from "../lib/graphLayout";
  import DrvNode from "../components/DrvNode.svelte";

  let { buildId } = $props();
  let layout = $state<LayoutResult | null>(null);

  // nodeTypes const — Svelte compiles this away; no remount trap like React inline
  const nodeTypes = { drvNode: DrvNode };

  $effect(() => {
    const id = setInterval(fetch, 5000);  // 5s poll — status colors update live
    fetch();
    return () => clearInterval(id);
    async function fetch() {
      const r = await admin.getBuildGraph({ buildId });
      if (r.truncated || r.nodes.length > DEGRADE_THRESHOLD) {
        layout = { degraded: true, reason: `${r.totalNodes} nodes` };
      } else if (r.nodes.length > WORKER_THRESHOLD) {
        layout = await layoutInWorker(r.nodes, r.edges);  // WebWorker
      } else {
        layout = layoutGraph(r.nodes, r.edges);
      }
    }
  });
</script>

{#if layout?.degraded}
  <GraphTable nodes={...} />  <!-- sortable table, failed/poisoned float to top -->
{:else if layout}
  <SvelteFlow nodes={layout.nodes} edges={layout.edges} {nodeTypes} />
{/if}
```

### T5 — `feat(dashboard):` DrvNode component

NEW `rio-dashboard/src/components/DrvNode.svelte` — `pname` top, `drvPath` hash-prefix mono below. Status → CSS class: green=completed, yellow=running (pulse), red=failed/poisoned, gray=queued. Running nodes show `workerId` on hover. Click → navigate `/builds/:id?drv=<path>` — [P0279](plan-0279-dashboard-streaming-log-viewer.md)'s LogViewer filters by it.

### T6 — `feat(dashboard):` embed in BuildDrawer Graph tab

MODIFY `rio-dashboard/src/components/BuildDrawer.svelte` — fill "Graph" tab.

### T7 — `test(dashboard):` layout unit tests

```typescript
// r[verify dash.graph.degrade-threshold]
test("layoutGraph 3 nodes/2 edges → positions non-zero");
test("layoutGraph 2001 nodes → degraded:true");
test("status → className mapping");
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule dash.graph.degrade-threshold` shows impl + verify

## Tracey

References existing markers:
- `r[dash.graph.degrade-threshold]` — T2 implements, T7 verifies (seeded by P0284)
- `r[dash.journey.build-to-logs]` — T5 node-click is step 2 ("click yellow node"). Journey impl closed here + P0279.

## Files

```json files
[
  {"path": "rio-dashboard/src/lib/graphLayout.ts", "action": "NEW", "note": "T2: dagre layout + DEGRADE_THRESHOLD"},
  {"path": "rio-dashboard/src/lib/graphLayout.worker.ts", "action": "NEW", "note": "T3: WebWorker for >500 nodes"},
  {"path": "rio-dashboard/src/pages/Graph.svelte", "action": "NEW", "note": "T4: SvelteFlow + 5s poll"},
  {"path": "rio-dashboard/src/components/DrvNode.svelte", "action": "NEW", "note": "T5: custom node (USER A7: Svelte)"},
  {"path": "rio-dashboard/src/components/BuildDrawer.svelte", "action": "MODIFY", "note": "T6: fill Graph tab (merge-trivial with P0279)"},
  {"path": "rio-dashboard/package.json", "action": "MODIFY", "note": "T1: @xyflow/svelte + dagre"},
  {"path": "nix/dashboard.nix", "action": "MODIFY", "note": "A8: pnpmDeps hash bump"}
]
```

```
rio-dashboard/src/
├── lib/
│   ├── graphLayout.ts            # T2: dagre + thresholds
│   └── graphLayout.worker.ts     # T3: WebWorker
├── pages/Graph.svelte            # T4: SvelteFlow
└── components/
    ├── DrvNode.svelte            # T5: custom node
    └── BuildDrawer.svelte        # T6: Graph tab
```

## Dependencies

```json deps
{"deps": [276, 277], "soft_deps": [278], "note": "USER A7: @xyflow/svelte NOT react. R6 less risk (compile-time reactivity). nodeTypes no remount trap. WebWorker >500 nodes. Degrade >2000."}
```

**Depends on:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — RPC exists. [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) — transport.
**Conflicts with:** `BuildDrawer.svelte` merge-trivial with P0279 (distinct tabs). `package.json` serial.
