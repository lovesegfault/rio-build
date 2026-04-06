// WebWorker entry for dagre layout. Vite bundles this as a separate
// chunk via the `?worker` import query in Graph.svelte — the
// worker-rollup pass does NOT run the svelte plugin, so every
// transitive import here must bottom out in plain JS/TS. layoutCore.ts
// is the worker-safe slice of the layout code (dagre + types, zero
// @xyflow/svelte references).
//
// One-shot protocol: main posts { nodes, edges }, worker posts back a
// plain-object map { drvPath: {x,y}, ... } (structuredClone can't
// transfer a Map prototype). Any thrown error is caught and posted as
// { error } so the main thread falls back to synchronous layout rather
// than hanging on a message that never arrives.
import { runLayout, type RawNode, type RawEdge } from './layoutCore';

export type WorkerRequest = {
  nodes: RawNode[];
  edges: RawEdge[];
};

export type WorkerResponse =
  | { positions: Record<string, { x: number; y: number }> }
  | { error: string };

// `self` in a module worker is DedicatedWorkerGlobalScope. The cast
// keeps tsc happy under lib: ["DOM"] — adding the WebWorker lib would
// clash with jsdom's Window types in tests.
const ctx = self as unknown as {
  onmessage: ((ev: MessageEvent<WorkerRequest>) => void) | null;
  postMessage: (msg: WorkerResponse) => void;
};

ctx.onmessage = (ev: MessageEvent<WorkerRequest>) => {
  try {
    const pos = runLayout(ev.data.nodes, ev.data.edges);
    const positions: Record<string, { x: number; y: number }> = {};
    for (const [k, v] of pos) positions[k] = v;
    ctx.postMessage({ positions });
  } catch (e) {
    ctx.postMessage({ error: e instanceof Error ? e.message : String(e) });
  }
};
