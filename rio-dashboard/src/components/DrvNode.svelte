<script lang="ts">
  // Step 2 of the killer journey: "click yellow node". The click itself
  // is caught by SvelteFlow's onnodeclick on the parent <SvelteFlow>
  // (Graph.svelte), which forwards the drvPath up to BuildDrawer —
  // that's where the buildId lives, and the drawer flips to the Logs
  // tab with the derivation path passed through to LogViewer. This
  // component only renders the box; it doesn't navigate directly
  // because GraphNode carries no build_id field (the proto is
  // derivation-scoped), and threading it through node.data would bloat
  // the 2000-node case for a value every node shares.
  //
  // xyflow hands custom nodes a NodeProps<NodeType> — we take `id`
  // (= drvPath) and `data` (DrvNodeData). The rest of the props (drag,
  // select, connectable) are unused but still present in the type;
  // Svelte 5's `$props()` just drops them.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';
  import { hashPrefix, statusClass, type DrvNode } from '../lib/graphLayout';

  let { id, data }: NodeProps<DrvNode> = $props();

  let hash = $derived(hashPrefix(id));
  let cls = $derived(statusClass(data.status));
</script>

<Handle type="target" position={Position.Top} />
<div class={`drv ${cls}`} data-testid="drv-node" data-status={data.status} title={id}>
  <span class="pname">{data.pname || '(unnamed)'}</span>
  <span class="hash">{hash}</span>
  {#if data.workerId && (data.status === 'running' || data.status === 'assigned')}
    <span class="worker" title="assigned worker">{data.workerId}</span>
  {/if}
</div>
<Handle type="source" position={Position.Bottom} />

<style>
  .drv {
    display: flex;
    flex-direction: column;
    width: 180px;
    min-height: 44px;
    border: 2px solid;
    border-radius: 6px;
    background: #fff;
    padding: 0.25rem 0.5rem;
    box-sizing: border-box;
    font-size: 0.8125rem;
    cursor: pointer;
  }
  /* Border colour by status class — box-shadow ring gives a soft halo
     without requiring a wrapper element. */
  .drv.green {
    border-color: #10b981;
    background: #d1fae5;
  }
  .drv.yellow {
    border-color: #f59e0b;
    background: #fef3c7;
    animation: pulse 1.5s ease-in-out infinite;
  }
  .drv.red {
    border-color: #ef4444;
    background: #fee2e2;
  }
  .drv.gray {
    border-color: #9ca3af;
    background: #f3f4f6;
  }
  .pname {
    display: block;
    width: 100%;
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .hash {
    display: block;
    font-family: monospace;
    font-size: 0.6875rem;
    color: #6b7280;
  }
  .worker {
    display: block;
    width: 100%;
    font-size: 0.6875rem;
    color: #92400e;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  @keyframes pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 rgba(245, 158, 11, 0.4);
    }
    50% {
      box-shadow: 0 0 0 6px rgba(245, 158, 11, 0);
    }
  }
</style>
