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
// rio-scheduler/src/state/derivation.rs DerivationStatus::as_str(). The
// golden snapshot at rio-test-support/golden/derivation_statuses.json is
// the cross-language source of truth — both the Rust snapshot test
// (derivation.rs status_snapshot mod) and the vitest cross-language
// describe (graphLayout.test.ts) fail if a new variant isn't plumbed
// here. Completed is green; in-flight (assigned/running) are yellow;
// every failure flavour is red; pre-dispatch states and cancelled are
// gray. Unknown defaults to gray so a proto/DB addition doesn't crash
// the node — but the golden cross-check catches that fall-through at
// test time, not at runtime.
const STATUS_CLASS: Record<string, 'green' | 'yellow' | 'red' | 'gray'> = {
  completed: 'green',
  skipped: 'green',
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

// Terminal states: no further status transitions without external reset.
// Mirrors rio-scheduler/src/state/derivation.rs DerivationStatus::
// is_terminal() — note `failed` is NOT terminal (retriable: failed→ready
// until poison threshold). Enforced cross-language via the golden
// snapshot's per-row `terminal: bool` field (graphLayout.test.ts
// "TERMINAL matches Rust is_terminal() via golden" — a Rust-side
// reclassification drifts both the nextest snapshot and that vitest).
// Graph.svelte uses this to stop polling once every node is settled —
// a finished build's drawer left open shouldn't keep hitting
// GetBuildGraph every 5s.
export const TERMINAL = new Set([
  'completed',
  'skipped',
  'poisoned',
  'dependency_failed',
  'cancelled',
]);

// All status strings the scheduler emits (derivation.rs as_str()). Kept
// here so the same-file exhaustiveness test in graphLayout.test.ts can
// iterate against STATUS_CLASS — catches the next Skipped-style addition
// before it silently falls through to gray. GraphNode.status is a plain
// string on the wire (dag.proto), not a proto enum, so there's no
// generated type to derive this from. The cross-language golden check
// asserts this hand-list matches what Rust actually emits (so this list
// staying stale no longer creates a blind spot).
export const DERIVATION_STATUSES = [
  'created',
  'queued',
  'ready',
  'assigned',
  'running',
  'completed',
  'failed',
  'poisoned',
  'dependency_failed',
  'cancelled',
  'skipped',
] as const;

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
// rank, pname lexically. Mirrors the same DerivationStatus string set
// as STATUS_CLASS above — the golden snapshot cross-check in
// graphLayout.test.ts asserts `s in SORT_RANK` for every Rust-emitted
// status (no fall-through to the `?? 5` default rank). Exported for
// that membership check; sortForTable stays the consuming API.
export const SORT_RANK: Record<string, number> = {
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
  skipped: 4,
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
// Edge routing is handled by DrvNode.svelte's <Handle> components
// (Position.Top / Position.Bottom); toXyflow does NOT set
// sourcePosition/targetPosition on the node objects. This module
// stays type-only w.r.t. @xyflow/svelte — value imports from the
// package would defeat the worker-safety split (the package's
// index.js transitively loads .svelte files).
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
