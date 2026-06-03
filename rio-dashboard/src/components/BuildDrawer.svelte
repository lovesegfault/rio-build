<script lang="ts">
  import type { BuildInfo } from '../api/types';
  import { progress, fmtTsAbs } from '../lib/buildInfo';
  import { TERMINAL } from '../lib/graphLayout';
  import Graph from './Graph.svelte';
  import BuildStatePill from './BuildStatePill.svelte';
  import LogViewer from './LogViewer.svelte';

  // Svelte 5 callback prop — no createEventDispatcher churn, and the
  // parent can hand us a plain arrow that nulls out `selectedBuild`.
  // Optional because the deep-link route may mount the drawer standalone
  // (no list behind it → "close" becomes "navigate to /builds" instead,
  // which P0280's router wiring will supply).
  let {
    build,
    onclose,
  }: { build: BuildInfo; onclose?: () => void } = $props();

  let activeTab = $state<'logs' | 'graph'>('logs');

  // DrvNode click in the Graph tab focuses that derivation in the Logs
  // tab. Keeping the state here (not in Graph.svelte) so switching
  // between tabs doesn't lose the selection — Graph re-mounts on every
  // tab flip but the drawer survives.
  //
  // focusedExecId is the per-build observation of which execution this
  // build watched (`GraphNode.exec_id` ← `build_derivations.exec_id`),
  // captured at click time so the log fetch can pin the EXACT execution
  // rather than fall back to "latest." Empty for Cached / never-ran
  // terminals / non-terminal — LogViewer renders an "approximate"
  // banner for those.
  let focusedDrv = $state<string | undefined>(undefined);
  let focusedExecId = $state<string>('');
  // The focused node's status, captured at click time alongside
  // drv/execId. Snapshot semantics are fine: the {#key} below remounts
  // LogViewer on focus change, and a node that was terminal at click
  // stays terminal (TERMINAL is absorbing).
  let focusedStatus = $state<string | undefined>(undefined);

  // BuildInfo.state wire values (BuildStatePill.STATE_META): 3 =
  // succeeded, 4 = failed, 5 = cancelled — the build-level terminal
  // set. A terminal build implies no derivation will print again even
  // when the focused node's own status was captured pre-terminal.
  const buildTerminal = $derived(
    build.state === 3 || build.state === 4 || build.state === 5,
  );
  // Live terminality closure for the log stream's tail_next exit law:
  // reads the reactive build state at call time (the drawer's `build`
  // prop refreshes with the list poll), plus the focused node's
  // captured status. Whole-build view: build state alone.
  const isTerminal = () =>
    buildTerminal ||
    (focusedStatus !== undefined && TERMINAL.has(focusedStatus));
</script>

<button
  type="button"
  class="backdrop"
  data-testid="drawer-backdrop"
  aria-label="Close drawer"
  onclick={onclose}
></button>

<div
  class="drawer"
  role="dialog"
  aria-modal="true"
  aria-labelledby="drawer-title"
  data-testid="build-drawer"
>
  <header>
    <h2 id="drawer-title">
      <code>{build.buildId}</code>
      <BuildStatePill state={build.state} />
    </h2>
    {#if onclose}
      <button type="button" aria-label="Close" onclick={onclose}>✕</button>
    {/if}
  </header>

  <dl>
    <dt>Tenant</dt>
    <dd>{build.tenantId || '—'}</dd>
    <dt>Priority class</dt>
    <dd>{build.priorityClass || '—'}</dd>
    <dt>Progress</dt>
    <dd>
      <progress value={progress(build)} max="100"></progress>
      {build.completedDerivations + build.cachedDerivations} / {build.totalDerivations}
      ({build.cachedDerivations} cached)
    </dd>
    <dt>Submitted</dt>
    <dd>{fmtTsAbs(build.submittedAt)}</dd>
    <dt>Started</dt>
    <dd>{fmtTsAbs(build.startedAt)}</dd>
    <dt>Finished</dt>
    <dd>{fmtTsAbs(build.finishedAt)}</dd>
    {#if build.errorSummary}
      <dt>Error</dt>
      <dd class="error">{build.errorSummary}</dd>
    {/if}
  </dl>

  <div class="tabs" role="tablist">
    <button
      type="button"
      role="tab"
      id="tab-logs"
      aria-selected={activeTab === 'logs'}
      aria-controls="tabpanel-body"
      class:active={activeTab === 'logs'}
      onclick={() => (activeTab = 'logs')}>Logs</button
    >
    <button
      type="button"
      role="tab"
      id="tab-graph"
      aria-selected={activeTab === 'graph'}
      aria-controls="tabpanel-body"
      class:active={activeTab === 'graph'}
      onclick={() => (activeTab = 'graph')}>Graph</button
    >
  </div>

  <div
    class="tab-body"
    role="tabpanel"
    id="tabpanel-body"
    aria-labelledby="tab-{activeTab}"
  >
    {#if activeTab === 'logs'}
      <!-- Keyed on buildId so switching builds (deep-link → different
           drawer target) tears down the old stream and starts a fresh
           one. Without the key Svelte reuses the component instance and
           the IIFE inside createLogStream keeps draining the prior
           build's fetch. -->
      {#key `${build.buildId}:${focusedDrv ?? ''}:${focusedExecId}`}
        <LogViewer drvPath={focusedDrv} execId={focusedExecId} {isTerminal} />
      {/key}
    {:else}
      <!-- Keyed on buildId for the same reason as LogViewer: Graph's
           $effect kicks off a poll + (possibly) a WebWorker, and we
           want both torn down cleanly if the drawer re-opens on a
           different build rather than inheriting the old interval. -->
      {#key build.buildId}
        <Graph
          buildId={build.buildId}
          ondrvclick={(drv, execId, status) => {
            focusedDrv = drv;
            focusedExecId = execId;
            focusedStatus = status;
            activeTab = 'logs';
          }}
        />
      {/key}
    {/if}
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    width: 100%;
    border: none;
    padding: 0;
    background: rgba(0, 0, 0, 0.3);
    z-index: 10;
    cursor: default;
  }
  .drawer {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    width: min(40rem, 90vw);
    background: #fff;
    border-left: 1px solid #e5e7eb;
    box-shadow: -4px 0 12px rgba(0, 0, 0, 0.1);
    z-index: 11;
    overflow-y: auto;
    padding: 1rem;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: start;
    gap: 1rem;
  }
  header h2 {
    margin: 0;
    font-size: 1rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: center;
  }
  header code {
    font-family: monospace;
    word-break: break-all;
  }
  header button {
    border: none;
    background: transparent;
    font-size: 1.25rem;
    cursor: pointer;
  }
  dl {
    display: grid;
    grid-template-columns: 8rem 1fr;
    row-gap: 0.5rem;
    margin: 1rem 0;
  }
  dt {
    font-weight: 500;
    color: #6b7280;
  }
  dd {
    margin: 0;
  }
  dd.error {
    color: #991b1b;
    font-family: monospace;
    font-size: 0.875rem;
    white-space: pre-wrap;
  }
  dd progress {
    width: 12rem;
    vertical-align: middle;
    margin-right: 0.5rem;
  }
  .tabs {
    display: flex;
    border-bottom: 1px solid #e5e7eb;
    gap: 0.25rem;
  }
  .tabs button {
    border: none;
    background: transparent;
    padding: 0.5rem 1rem;
    cursor: pointer;
    border-bottom: 2px solid transparent;
  }
  .tabs button.active {
    border-bottom-color: #2563eb;
    font-weight: 500;
  }
  .tab-body {
    padding: 1rem 0;
  }
</style>
