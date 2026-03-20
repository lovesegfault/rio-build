// Dagre-only layout core. ZERO @xyflow/svelte imports — this file is
// pulled into the WebWorker bundle (graphLayout.worker.ts), and vite's
// worker-rollup pass doesn't run the svelte plugin. Any transitive
// import that reaches a .svelte file fails with "Expression expected
// (you need plugins to import files that are not JavaScript)". Keeping
// this module dependency-thin (dagre + types only) lets the worker
// chunk link without the svelte toolchain.
//
// The xyflow-consuming half (Node/Edge mapping, Position enum) lives in
// graphLayout.ts, which only the main thread imports.
//
// @dagrejs/dagre v2's ESM bundle is `export default { graphlib, layout }`;
// the d.ts declares named exports. esModuleInterop maps the default
// import onto that shape, and skipLibCheck keeps the d.ts/runtime skew
// from biting svelte-check.
import dagre from '@dagrejs/dagre';

// Structural shapes matching the generated dag_pb.ts field names. The
// worker receives these via structuredClone (postMessage) which strips
// the Message<...> prototype; vitest fixtures build them from plain
// literals. Keeping the field names identical means the main-thread RPC
// response is assignable without mapping.
export type RawNode = {
  drvPath: string;
  pname: string;
  system: string;
  status: string;
  assignedWorkerId: string;
};

export type RawEdge = {
  parentDrvPath: string;
  childDrvPath: string;
  // field 3 (is_cutoff) is RESERVED in dag.proto — P0252 moved cutoff
  // signalling to DerivationStatus::Skipped on the node, not an edge
  // flag. The plan sketch referenced isCutoff; it no longer exists.
};

export const NODE_W = 180;
export const NODE_H = 44;

// Dagre pass. Returns {id → centre-adjusted position}. The main thread
// stitches this onto xyflow nodes; the worker posts it back as a plain
// record (structuredClone can't transfer Map).
export function runLayout(
  nodes: readonly RawNode[],
  edges: readonly RawEdge[],
): Map<string, { x: number; y: number }> {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: 'TB', nodesep: 40, ranksep: 80 });
  g.setDefaultEdgeLabel(() => ({}));

  for (const n of nodes) {
    g.setNode(n.drvPath, { width: NODE_W, height: NODE_H });
  }
  // child → parent = build order (a child builds before its parent).
  // dagre places sources at the top of a TB layout, so the build's
  // final targets end up at the bottom — same orientation as
  // `nix log`'s dependency trace.
  for (const e of edges) {
    g.setEdge(e.childDrvPath, e.parentDrvPath);
  }

  dagre.layout(g);

  const out = new Map<string, { x: number; y: number }>();
  for (const id of g.nodes()) {
    const n = g.node(id);
    // dagre centres the node; xyflow positions by top-left. Subtract
    // half width/height so the centred box lands where dagre intended.
    out.set(id, { x: n.x - NODE_W / 2, y: n.y - NODE_H / 2 });
  }
  return out;
}
