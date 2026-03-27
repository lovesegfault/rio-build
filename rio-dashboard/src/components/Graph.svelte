<script lang="ts">
  // DAG visualization for a single build. Polls GetBuildGraph every 5s
  // so node status-colours update as derivations transition; the poll
  // does NOT re-layout unless the node set changed (a cheap count+edge
  // signature check — re-running dagre on every poll would make running
  // builds unwatchable). @xyflow/svelte handles pan/zoom; dagre assigns
  // positions.
  //
  // Threshold ladder:
  //   ≤500    main-thread dagre (sub-100ms)
  //   501-2000 WebWorker dagre (1-3s, UI stays responsive)
  //   >2000   sortable table, no graph (dagre + xyflow both degrade)
  //
  // The server separately caps at 5000 (DASHBOARD_GRAPH_NODE_LIMIT);
  // GetBuildGraphResponse.truncated signals that, and we degrade
  // immediately regardless of the returned subset size.
  import { SvelteFlow, Background, Controls } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import { admin } from '../api/admin';
  import {
    DEGRADE_THRESHOLD,
    TERMINAL,
    WORKER_THRESHOLD,
    layoutGraph,
    sortForTable,
    statusClass,
    toXyflow,
    type DrvNode,
    type LayoutResult,
    type RawEdge,
    type RawNode,
  } from '../lib/graphLayout';
  import type {
    WorkerRequest,
    WorkerResponse,
  } from '../lib/graphLayout.worker';
  // Vite's `?worker` query-import emits the worker as a separate chunk
  // and returns a constructor. Using `new URL('…', import.meta.url)`
  // instead triggers vite:worker-import-meta-url, which scans the full
  // import graph BEFORE the svelte plugin transforms .svelte files —
  // it chokes on `<script lang="ts" generics="…">` in
  // @xyflow/svelte's SvelteFlow.svelte. The query form sidesteps the
  // plugin-ordering conflict entirely.
  import GraphLayoutWorker from '../lib/graphLayout.worker?worker';
  import DrvNodeComponent from './DrvNode.svelte';
  import type { Edge } from '@xyflow/svelte';

  let {
    buildId,
    ondrvclick = undefined,
  }: { buildId: string; ondrvclick?: (drvPath: string) => void } = $props();

  // Module-level const — Svelte 5 doesn't have React's "inline nodeTypes
  // remounts all custom nodes" footgun (compile-time reactivity knows the
  // object identity is stable), but keeping it const documents intent and
  // lets svelte-check prove the component matches the NodeTypes shape.
  const nodeTypes = { drvNode: DrvNodeComponent };

  // xyflow expects these as $bindable-backed arrays so it can mutate in
  // place for drag/select. $state.raw — the node positions are wholesale
  // replaced on relayout, we don't need fine-grained proxy tracking.
  let nodes = $state.raw<DrvNode[]>([]);
  let edges = $state.raw<Edge[]>([]);

  let layout = $state<LayoutResult | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);

  // Structural signature of the last layout — if the next poll returns
  // the same set of drv paths and edges, we patch node.data (status,
  // executorId) in place instead of re-running dagre. Status colour
  // updates should feel instant; a full relayout pauses interaction.
  let lastSig = '';
  function sigOf(gn: readonly RawNode[], ge: readonly RawEdge[]): string {
    return `${gn.length}|${ge.length}|${gn.map((n) => n.drvPath).join(',')}`;
  }

  // One worker per Graph mount. Lazily constructed the first time the
  // node count crosses WORKER_THRESHOLD — most builds never get that
  // large and the worker startup (parse + import dagre) is ~50ms we'd
  // rather not pay for a 20-node graph.
  let worker: Worker | null = null;
  function getWorker(): Worker {
    if (!worker) {
      worker = new GraphLayoutWorker();
    }
    return worker;
  }

  // Re-entrancy gate for fetchAndLayout. The 5s setInterval keeps
  // firing regardless of whether the last poll finished — a slow
  // network or a heavy dagre pass (1-3s in the worker for 1500+ nodes)
  // means overlapping calls. Without the gate, both calls race through
  // layoutInWorker: each installs its own {once:true}-style listener,
  // the FIRST worker response fires the FIRST listener (promise N
  // resolves correctly), the SECOND worker response fires the SECOND
  // listener — promise N+1 resolves with response N+1. That's actually
  // fine for the worker path in isolation, BUT the two fetches run
  // concurrently: response N has stale statuses relative to N+1, and if
  // N's fetch was slow but its layout was fast, N's assignment can land
  // AFTER N+1's (last-write-wins on `layout =`, `nodes =`). The gate
  // serializes the whole fetch→layout→assign pipeline — simpler than
  // seq-number correlation, and we don't want concurrent layouts anyway
  // (the poll is a single logical stream).
  let inflight = false;

  // Set once every node hits a terminal status (graphLayout.TERMINAL
  // mirrors is_terminal() in the scheduler). A finished build's drawer
  // can sit open indefinitely — no point polling GetBuildGraph every 5s
  // when nothing can change. The $effect reads this reactively and
  // tears down the interval; if the {#key buildId} wrapper remounts us
  // for a different build, $state re-initializes to false and polling
  // resumes for the new graph.
  let allTerminal = $state(false);

  function layoutInWorker(
    gn: RawNode[],
    ge: RawEdge[],
  ): Promise<LayoutResult> {
    return new Promise((resolve) => {
      const w = getWorker();
      const onMsg = (ev: MessageEvent<WorkerResponse>) => {
        w.removeEventListener('message', onMsg);
        if ('error' in ev.data) {
          // Worker crashed — fall back to synchronous. Slow is better
          // than blank.
          resolve(layoutGraph(gn, ge));
          return;
        }
        const pos = new Map(Object.entries(ev.data.positions));
        resolve({ degraded: false, ...toXyflow(gn, ge, pos) });
      };
      w.addEventListener('message', onMsg);
      const req: WorkerRequest = { nodes: gn, edges: ge };
      w.postMessage(req);
    });
  }

  // Patch-in-place: build a drvPath → status/executorId lookup from the
  // new response and rewrite only the .data and .class of each existing
  // xyflow node. xyflow's internal diff notices the class change and
  // re-renders just that node's DOM — no relayout, no viewport jump.
  function patchStatuses(gn: readonly RawNode[]) {
    const by = new Map(gn.map((n) => [n.drvPath, n]));
    nodes = nodes.map((n) => {
      const raw = by.get(n.id);
      if (!raw) return n;
      return {
        ...n,
        class: `drv-${statusClass(raw.status)}`,
        data: {
          ...n.data,
          status: raw.status,
          executorId: raw.assignedExecutorId,
        },
      };
    });
  }

  async function fetchAndLayout() {
    if (inflight) return;
    inflight = true;
    let r;
    try {
      r = await admin.getBuildGraph({ buildId });
      error = null;
    } catch (e) {
      error = String(e);
      loading = false;
      inflight = false;
      return;
    }

    try {
      // Terminal-settle check. `r.nodes.length > 0` guards the trivial
      // every([])→true — an empty response (build not yet populated, or
      // ListWatcher race) must NOT stop polling. `!r.truncated` guards
      // against stopping on a partial view: insertion-order truncation
      // means roots settle first (normal DAG progress) while the tail
      // may still be running. Keep polling when truncated.
      if (!r.truncated && r.nodes.length > 0 && r.nodes.every((n) => TERMINAL.has(n.status))) {
        allTerminal = true;
      }

      // Server-side truncation trumps our own threshold — the subset we
      // got back is arbitrary (first-5000 by insertion order, not
      // topological), so laying it out would lie about the graph shape.
      if (r.truncated || r.nodes.length > DEGRADE_THRESHOLD) {
        layout = {
          degraded: true,
          reason: r.truncated
            ? `server truncated (${r.totalNodes} total)`
            : `${r.nodes.length} nodes > ${DEGRADE_THRESHOLD}`,
          nodes: sortForTable(r.nodes),
        };
        loading = false;
        return;
      }

      const sig = sigOf(r.nodes, r.edges);
      if (sig === lastSig && layout && !layout.degraded) {
        patchStatuses(r.nodes);
        loading = false;
        return;
      }
      lastSig = sig;

      const result =
        r.nodes.length > WORKER_THRESHOLD
          ? await layoutInWorker(r.nodes, r.edges)
          : layoutGraph(r.nodes, r.edges);

      layout = result;
      if (!result.degraded) {
        nodes = result.nodes;
        edges = result.edges;
      }
      loading = false;
    } finally {
      inflight = false;
    }
  }

  $effect(() => {
    // Read buildId synchronously so the effect re-runs if BuildDrawer
    // re-mounts us with a different build (it shouldn't — the {#key}
    // wrapper tears this whole component down — but belt-and-braces).
    void buildId;
    fetchAndLayout();
    // allTerminal is reactive: when fetchAndLayout flips it true the
    // effect re-runs, the old interval is cleared by the teardown
    // closure, and this branch declines to start a new one. One last
    // fetchAndLayout fires (above) — harmless, the inflight gate or
    // the sig-match short-circuits it.
    const t = allTerminal ? null : setInterval(fetchAndLayout, 5000);
    return () => {
      if (t !== null) clearInterval(t);
    };
  });

  // Worker lifecycle split into its own effect so the allTerminal flip
  // above doesn't tear down an in-use worker — the worker should
  // survive until component unmount (or buildId change, which the
  // {#key} wrapper turns into an unmount anyway). Svelte effects with
  // no reactive dependencies run once on mount and teardown on unmount.
  $effect(() => {
    return () => {
      worker?.terminate();
      worker = null;
    };
  });
</script>

{#if error}
  <div role="alert" class="err">graph fetch failed: {error}</div>
{:else if loading}
  <div class="loading">loading graph…</div>
{:else if layout?.degraded}
  <div class="degraded" data-testid="graph-degraded">
    <p class="reason">
      Graph too large for interactive view: {layout.reason}. Showing sortable
      table instead (failed/poisoned first).
    </p>
    <table>
      <thead>
        <tr>
          <th>pname</th>
          <th>status</th>
          <th>worker</th>
          <th>drv</th>
        </tr>
      </thead>
      <tbody>
        {#each layout.nodes as n (n.drvPath)}
          <tr
            data-testid="graph-table-row"
            onclick={() => ondrvclick?.(n.drvPath)}
          >
            <td>{n.pname}</td>
            <td><span class={`pill ${statusClass(n.status)}`}>{n.status}</span></td>
            <td>{n.assignedExecutorId || '—'}</td>
            <td><code>{n.drvPath}</code></td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{:else if layout}
  <div class="flow" data-testid="graph-flow">
    <SvelteFlow
      bind:nodes
      bind:edges
      {nodeTypes}
      fitView
      nodesDraggable={false}
      onnodeclick={({ node }) => ondrvclick?.(node.id)}
    >
      <Background />
      <Controls />
    </SvelteFlow>
  </div>
{/if}

<style>
  .flow {
    /* Fill the drawer's tab body. xyflow requires an explicit height on
       its container or it collapses to 0px and nothing renders. */
    height: 32rem;
    width: 100%;
    border: 1px solid #e5e7eb;
    border-radius: 4px;
  }
  .loading {
    padding: 2rem;
    text-align: center;
    color: #9ca3af;
    font-style: italic;
  }
  .err {
    padding: 0.75rem;
    background: #fee2e2;
    color: #991b1b;
    border-radius: 4px;
  }
  .degraded {
    max-height: 32rem;
    overflow-y: auto;
  }
  .degraded .reason {
    margin: 0 0 0.75rem;
    padding: 0.5rem;
    background: #fef3c7;
    border-left: 3px solid #f59e0b;
    font-size: 0.875rem;
  }
  .degraded table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.8125rem;
  }
  .degraded th,
  .degraded td {
    text-align: left;
    padding: 0.375rem 0.5rem;
    border-bottom: 1px solid #e5e7eb;
  }
  .degraded tbody tr {
    cursor: pointer;
  }
  .degraded tbody tr:hover {
    background: #f9fafb;
  }
  .degraded code {
    font-family: monospace;
    font-size: 0.75rem;
    color: #6b7280;
    word-break: break-all;
  }
  .pill {
    display: inline-block;
    padding: 0.0625rem 0.5rem;
    border-radius: 9999px;
    font-size: 0.75rem;
  }
  .pill.green {
    background: #d1fae5;
    color: #065f46;
  }
  .pill.yellow {
    background: #fef3c7;
    color: #92400e;
  }
  .pill.red {
    background: #fee2e2;
    color: #991b1b;
  }
  .pill.gray {
    background: #f3f4f6;
    color: #6b7280;
  }
</style>
