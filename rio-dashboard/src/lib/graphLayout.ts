// Main-thread layout glue: wraps layoutCore's pure dagre pass with the
// @xyflow/svelte-aware bits (Node/Edge mapping, Position handles, status
// → CSS class). The dagre call itself lives in layoutCore.ts so the
// WebWorker can import it without transitively reaching into
// @xyflow/svelte (whose package ships .svelte files that vite's worker
// rollup can't process — it doesn't run the svelte plugin).
import type { Node, Edge } from '@xyflow/svelte';
import { runLayout, type RawEdge, type RawNode } from './layoutCore';

export { runLayout } from './layoutCore';
export type { RawNode, RawEdge } from './layoutCore';

export type DrvNodeData = {
  pname: string;
  drvPath: string;
  status: string;
  workerId: string;
};

// xyflow's Node generic needs Record<string, unknown>-compatible data;
// the index-signature spread keeps DrvNodeData assignable without
// widening every field to `unknown`.
export type DrvNode = Node<DrvNodeData & Record<string, unknown>, 'drvNode'>;

// r[impl dash.graph.degrade-threshold]
// Hard ceiling for interactive xyflow rendering. Above this, the Graph
// page falls back to a sortable table — dagre layout on 2001+ nodes
// takes seconds and xyflow's own DOM churn degrades pan/zoom past the
// point of usability. The server separately caps at 5000
// (DASHBOARD_GRAPH_NODE_LIMIT), which always wins.
export const DEGRADE_THRESHOLD = 2000;

// Above this, Graph.svelte punts dagre to a WebWorker so the 5s poll
// doesn't stall the UI mid-layout. Below it, synchronous layout is
// sub-100ms and the worker round-trip isn't worth the complexity.
export const WORKER_THRESHOLD = 500;

// DerivationStatus string → CSS class. Mirrors
// rio-scheduler/src/state/derivation.rs:119-132 (as_str). Completed is
// green; in-flight (assigned/running) are yellow; every failure flavour
// is red; pre-dispatch states and cancelled are gray. Unknown defaults
// to gray so a proto/DB addition doesn't crash the node.
const STATUS_CLASS: Record<string, 'green' | 'yellow' | 'red' | 'gray'> = {
  completed: 'green',
  assigned: 'yellow',
  running: 'yellow',
  failed: 'red',
  poisoned: 'red',
  dependency_failed: 'red',
  created: 'gray',
  queued: 'gray',
  ready: 'gray',
  cancelled: 'gray',
};

export function statusClass(status: string): 'green' | 'yellow' | 'red' | 'gray' {
  return STATUS_CLASS[status] ?? 'gray';
}

// /nix/store/<32-char-hash>-<name>.drv → <8-char-prefix>. Falls back to
// the last path segment when the shape doesn't match (tests, mocks).
export function hashPrefix(drvPath: string): string {
  const m = /\/nix\/store\/([a-z0-9]{32})-/.exec(drvPath);
  if (m) return m[1].slice(0, 8);
  const base = drvPath.split('/').pop() ?? drvPath;
  return base.slice(0, 8);
}

// Failed/poisoned sorts first (top of the degraded table — "what's
// broken"), then in-flight, then pre-dispatch, then completed. Within a
// rank, pname lexically.
const SORT_RANK: Record<string, number> = {
  failed: 0,
  poisoned: 0,
  dependency_failed: 0,
  running: 1,
  assigned: 1,
  ready: 2,
  queued: 2,
  created: 2,
  cancelled: 3,
  completed: 4,
};

export function sortForTable(nodes: readonly RawNode[]): RawNode[] {
  return [...nodes].sort((a, b) => {
    const ra = SORT_RANK[a.status] ?? 5;
    const rb = SORT_RANK[b.status] ?? 5;
    if (ra !== rb) return ra - rb;
    return a.pname.localeCompare(b.pname);
  });
}

export type LayoutOk = { degraded: false; nodes: DrvNode[]; edges: Edge[] };
export type LayoutDegraded = { degraded: true; reason: string; nodes: RawNode[] };
export type LayoutResult = LayoutOk | LayoutDegraded;

// Synchronous main-thread path. Graph.svelte only calls this for
// ≤ WORKER_THRESHOLD; the degrade guard is belt-and-braces for direct
// library consumers and makes the unit tests self-contained.
export function layoutGraph(
  gn: readonly RawNode[],
  ge: readonly RawEdge[],
): LayoutResult {
  if (gn.length > DEGRADE_THRESHOLD) {
    return {
      degraded: true,
      reason: `${gn.length} nodes > ${DEGRADE_THRESHOLD}`,
      nodes: sortForTable(gn),
    };
  }
  const pos = runLayout(gn, ge);
  return { degraded: false, ...toXyflow(gn, ge, pos) };
}

// Shared stitch step — synchronous layoutGraph() and Graph.svelte's
// worker-completion handler both call this once positions are known.
// sourcePosition/targetPosition use string literals ('bottom'/'top')
// rather than the Position enum so this module stays type-only w.r.t.
// @xyflow/svelte — value imports from the package would defeat the
// worker-safety split (the enum is re-exported through index.js which
// transitively loads .svelte files).
export function toXyflow(
  gn: readonly RawNode[],
  ge: readonly RawEdge[],
  pos: ReadonlyMap<string, { x: number; y: number }>,
): { nodes: DrvNode[]; edges: Edge[] } {
  const nodes: DrvNode[] = gn.map((n) => ({
    id: n.drvPath,
    position: pos.get(n.drvPath) ?? { x: 0, y: 0 },
    type: 'drvNode',
    data: {
      pname: n.pname,
      drvPath: n.drvPath,
      status: n.status,
      workerId: n.assignedWorkerId,
    },
    class: `drv-${statusClass(n.status)}`,
  }));

  const edges: Edge[] = ge.map((e) => ({
    id: `${e.childDrvPath}→${e.parentDrvPath}`,
    source: e.childDrvPath,
    target: e.parentDrvPath,
  }));

  return { nodes, edges };
}
