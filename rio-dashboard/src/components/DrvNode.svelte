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
  import ClearPoisonButton from './ClearPoisonButton.svelte';

  let { id, data }: NodeProps<DrvNode> = $props();

  let hash = $derived(hashPrefix(id));
  let cls = $derived(statusClass(data.status));

  // ClearPoison RPC keys on the 32-char store-path hash, not the full
  // drvPath. Same regex as hashPrefix but captures the whole thing.
  // Falls back to the full id for non-standard paths (tests, mocks).
  let drvHash = $derived(
    /\/nix\/store\/([a-z0-9]{32})-/.exec(id)?.[1] ?? id,
  );

  // Context-menu state. Right-click opens; any click (inside or out)
  // closes via the window listener below. stopPropagation on the menu
  // body keeps a click on ClearPoisonButton from dismissing before the
  // confirm() dialog pops.
  let menuOpen = $state(false);
  let menuPos = $state({ x: 0, y: 0 });

  function onContextMenu(e: MouseEvent) {
    e.preventDefault();
    menuPos = { x: e.clientX, y: e.clientY };
    menuOpen = true;
  }

  $effect(() => {
    if (!menuOpen) return;
    const close = () => (menuOpen = false);
    // Defer registration one tick so the contextmenu's own click
    // bubble doesn't immediately fire `close`.
    const id = setTimeout(() => window.addEventListener('click', close), 0);
    return () => {
      clearTimeout(id);
      window.removeEventListener('click', close);
    };
  });
</script>

<Handle type="target" position={Position.Top} />
<div
  class={`drv ${cls}`}
  data-testid="drv-node"
  data-status={data.status}
  title={id}
  oncontextmenu={onContextMenu}
  role="button"
  tabindex="-1"
>
  <span class="pname">{data.pname || '(unnamed)'}</span>
  <span class="hash">{hash}</span>
  {#if data.executorId && (data.status === 'running' || data.status === 'assigned')}
    <span class="worker" title="assigned executor">{data.executorId}</span>
  {/if}
</div>
<Handle type="source" position={Position.Bottom} />

{#if menuOpen}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="ctx-menu"
    data-testid="drv-ctx-menu"
    style:left="{menuPos.x}px"
    style:top="{menuPos.y}px"
    onclick={(e) => e.stopPropagation()}
  >
    <ClearPoisonButton
      derivationHash={drvHash}
      poisoned={data.status === 'poisoned'}
      onCleared={() => (menuOpen = false)}
    />
    {#if data.status !== 'poisoned'}
      <span class="ctx-empty">no actions</span>
    {/if}
  </div>
{/if}

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
  .ctx-menu {
    position: fixed;
    z-index: 20;
    background: #fff;
    border: 1px solid #e5e7eb;
    border-radius: 4px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.15);
    padding: 0.5rem;
    min-width: 10rem;
  }
  .ctx-empty {
    color: #9ca3af;
    font-size: 0.8125rem;
    font-style: italic;
  }
</style>
