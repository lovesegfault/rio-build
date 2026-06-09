<script lang="ts">
  // DAG visualization for a single build. The DATA lives in the
  // drawer-lifetime poll store (lib/buildGraphPoll.svelte.ts,
  // merged_bug_134) — this component is the LAYOUT half only: it
  // reacts to the store's node/edge sets, decides degraded-vs-flow,
  // runs dagre (main thread or worker), and patches statuses in place
  // when the structure is unchanged. Moving the poll out is what makes
  // the terminality oracle live while the Logs tab has this component
  // unmounted; it also means a tab flip back to Graph re-renders from
  // current data instead of re-fetching from scratch.
  //
  // Threshold ladder:
  //   ≤500    main-thread dagre (sub-100ms)
  //   501-2000 WebWorker dagre (1-3s, UI stays responsive)
  //   >2000   sortable table, no graph (dagre + xyflow both degrade)
  //
  // The server separately caps at 5000 (DASHBOARD_GRAPH_NODE_LIMIT);
  // truncated signals that, and we degrade immediately regardless of
  // the returned subset size.
  import { SvelteFlow, Background, Controls } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import type { BuildGraphPoll } from '../lib/buildGraphPoll.svelte';
  import {
    DEGRADE_THRESHOLD,
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
    poll,
    ondrvclick = undefined,
  }: {
    // THE single node-status source (merged_bug_134): rendering and
    // the drawer's terminality oracle read the same store.
    poll: BuildGraphPoll;
    // execId is the per-build observation of which execution this build
    // watched (`build_derivations.exec_id` via `GraphNode.exec_id`).
    // Empty for Cached / never-ran terminals / non-terminal — the log
    // fetch falls back to "latest exec" and labels itself approximate.
    // status rides along for callers that want the click-time value;
    // the drawer's oracle reads poll.statusOf instead (live).
    ondrvclick?: (drvPath: string, execId: string, status: string) => void;
  } = $props();

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

  // Structural signature of the last layout — if the store's next poll
  // delivers the same set of drv paths and edges, we patch node.data
  // (status, executorId) in place instead of re-running dagre. Status
  // colour updates should feel instant; a full relayout pauses
  // interaction.
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

  // Re-entrancy gate for the layout pipeline. The store can deliver a
  // new node set while a heavy dagre pass (1-3s in the worker for
  // 1500+ nodes) is still running; concurrent layouts could resolve
  // out-of-order (last-write-wins on `layout =`, `nodes =`). The gate
  // serializes layout passes; `rerun` coalesces every set that arrived
  // mid-pass into ONE trailing pass that re-reads the store's CURRENT
  // data (never a stale captured set).
  let layoutBusy = false;
  let rerun = false;

  function layoutInWorker(
    gn: readonly RawNode[],
    ge: readonly RawEdge[],
  ): Promise<LayoutResult> {
    return new Promise((resolve) => {
      const w = getWorker();
      const onMsg = (ev: MessageEvent<WorkerResponse>) => {
        w.removeEventListener('message', onMsg);
        if ('error' in ev.data) {
          // Worker crashed — fall back to synchronous. Slow is better
          // than blank.
          resolve(layoutGraph([...gn], [...ge]));
          return;
        }
        const pos = new Map(Object.entries(ev.data.positions));
        resolve({ degraded: false, ...toXyflow([...gn], [...ge], pos) });
      };
      w.addEventListener('message', onMsg);
      const req: WorkerRequest = { nodes: [...gn], edges: [...ge] };
      w.postMessage(req);
    });
  }

  // Thread the store's onCleared into each node's data so DrvNode's
  // ClearPoisonButton can restart a settled poll (merged_bug_134 —
  // "the next 5s poll picks up the new status" was unsatisfiable when
  // the allTerminal latch had permanently stopped the interval).
  // patchStatuses preserves it via the data spread.
  function withOncleared(ns: DrvNode[]): DrvNode[] {
    return ns.map((n) => ({
      ...n,
      data: { ...n.data, oncleared: () => poll.onCleared() },
    }));
  }

  // Patch-in-place: build a drvPath → status/executorId lookup from the
  // new response and rewrite only the .data and .class of each existing
  // xyflow node. xyflow's internal diff notices the class change and
  // re-renders just that node's DOM — no relayout, no viewport jump.
  // execId is patched alongside status because both transition once
  // (empty → set) when the drv reaches a terminal state — without the
  // patch a node clicked between polls would still pass execId="" and
  // the log fetch would fall back to "latest" with the approximate
  // banner even after the exact correlation is recorded.
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
          execId: raw.execId,
        },
      };
    });
  }

  async function relayoutOnce(): Promise<void> {
    // Read FRESH from the store — a pass queued behind a slow layout
    // must see the data that queued it, not a stale capture.
    const gn = poll.nodes;
    const ge = poll.edges;

    // Server-side truncation trumps our own threshold — the subset we
    // got back is arbitrary (first-5000 by insertion order, not
    // topological), so laying it out would lie about the graph shape.
    if (poll.truncated || gn.length > DEGRADE_THRESHOLD) {
      layout = {
        degraded: true,
        reason: poll.truncated
          ? `server truncated (${poll.totalNodes} total)`
          : `${gn.length} nodes > ${DEGRADE_THRESHOLD}`,
        nodes: sortForTable(gn),
      };
      return;
    }

    const sig = sigOf(gn, ge);
    if (sig === lastSig && layout && !layout.degraded) {
      patchStatuses(gn);
      return;
    }
    lastSig = sig;

    const result =
      gn.length > WORKER_THRESHOLD
        ? await layoutInWorker(gn, ge)
        : layoutGraph([...gn], [...ge]);

    layout = result;
    if (!result.degraded) {
      nodes = withOncleared(result.nodes);
      edges = result.edges;
    }
  }

  async function scheduleLayout(): Promise<void> {
    if (layoutBusy) {
      rerun = true;
      return;
    }
    layoutBusy = true;
    try {
      do {
        rerun = false;
        await relayoutOnce();
      } while (rerun);
    } finally {
      layoutBusy = false;
    }
  }

  $effect(() => {
    // Track the store's reactive surfaces; the layout pass re-reads
    // them fresh inside relayoutOnce.
    void poll.nodes;
    void poll.edges;
    void poll.truncated;
    if (poll.loading && poll.nodes.length === 0) return;
    void scheduleLayout();
  });

  // Worker lifecycle in its own effect: no reactive dependencies, so
  // it runs once on mount and tears down on unmount (or buildId
  // change, which the {#key} wrapper turns into an unmount anyway).
  $effect(() => {
    return () => {
      worker?.terminate();
      worker = null;
    };
  });
</script>

{#if poll.error}
  <div role="alert" class="err">graph fetch failed: {poll.error}</div>
{:else if layout === null}
  <div class="loading">loading graph…</div>
{:else if layout.degraded}
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
            onclick={() => ondrvclick?.(n.drvPath, n.execId, n.status)}
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
{:else}
  <div class="flow" data-testid="graph-flow">
    <SvelteFlow
      bind:nodes
      bind:edges
      {nodeTypes}
      fitView
      nodesDraggable={false}
      onnodeclick={({ node }) =>
        ondrvclick?.(
          node.id,
          (node.data?.execId as string) ?? '',
          (node.data?.status as string) ?? '',
        )}
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
