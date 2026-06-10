<script lang="ts">
  import type { BuildInfo } from '../api/types';
  import { progress, fmtTsAbs } from '../lib/buildInfo';
  import {
    createBuildGraphPoll,
    type BuildGraphPoll,
  } from '../lib/buildGraphPoll.svelte';
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

  // THE node-status source (merged_bug_134): one drawer-lifetime
  // GetBuildGraph poll feeding Graph's rendering AND the log stream's
  // terminality oracle. The poll must outlive Graph.svelte — that
  // component unmounts whenever the Logs tab is active, which is
  // precisely when the oracle is load-bearing. Recreated (and the old
  // one destroyed) if the parent re-points the drawer at a different
  // build without remounting it.
  let poll = $state<BuildGraphPoll | null>(null);
  $effect(() => {
    const p = createBuildGraphPoll(build.buildId);
    poll = p;
    return () => p.destroy();
  });

  // DrvNode click in the Graph tab focuses that derivation in the Logs
  // tab. Keeping the state here (not in Graph.svelte) so switching
  // between tabs doesn't lose the selection — Graph re-mounts on every
  // tab flip but the drawer survives.
  //
  // SCOPE LAW (bug_090, signed Q4): the Logs tab is the PER-ATTEMPT
  // surface — a stream mounts ONLY when a derivation is focused. The
  // TailLog selector alphabet is closed (pinned exec | non-empty
  // derivation, api/logs.ts); the empty form has no resolver, so the
  // pre-fix unfocused mount was a guaranteed-NotFound dial loop.
  // Unfocused renders the static unavailable-by-design panel below;
  // whole-build aggregation is an explicit non-goal (no server
  // aggregation contract).
  //
  // focusedExecId is the per-build observation of which execution this
  // build watched (`GraphNode.exec_id` ← `build_derivations.exec_id`),
  // captured at click time so the log fetch can pin the EXACT execution
  // rather than fall back to "latest." Empty for Cached / never-ran
  // terminals / non-terminal — LogViewer renders an "approximate"
  // banner for those. (Pinning by click-time capture is correct: the
  // {#key} below remounts LogViewer when the pin changes.)
  let focusedDrv = $state<string | undefined>(undefined);
  let focusedExecId = $state<string>('');

  // The focused node's status, DERIVED LIVE from the poll
  // (merged_bug_134 — never a click-time snapshot). The pre-fix
  // capture froze the terminality oracle in both directions: a node
  // clicked while running never read terminal after completing
  // mid-build (the stream re-dialed at the pacer cap, tab stuck
  // "streaming"), and a poisoned-at-click node stayed "terminal" after
  // ClearPoison (the stream exited incomplete instead of following the
  // retry).
  const focusedStatus = $derived(poll?.statusOf(focusedDrv));

  // Live terminality closure for the log stream's tail_next exit law
  // (merged_bug_074): EVERY oracle input reads the drawer-lifetime
  // graph poll at call time. The `build` prop is render-only snapshot
  // data — it is captured at click and never refreshes (Builds.svelte's
  // $effect tracks only statusFilter/pageIdx, and `selected` is never
  // re-pointed when a list refresh replaces the array) — so a prop leg
  // would freeze the oracle in both directions: a dead build's focused
  // node followed forever, and — the retry-follow killer — a
  // terminal-at-click build whose node is poison-cleared out of band
  // would pin the oracle terminal and exit the stream instead of
  // following the retry (the log-tail rule's retry-follow clause: the
  // shared row flips queued/running when the cleared drv re-runs under
  // another build).
  //
  // poll.allTerminal is the build-level leg: subsumed by the node leg
  // whenever the focused drv is in the served set, load-bearing exactly
  // when statusOf returns undefined (degenerate/absent-row views), and
  // kept live across purged/empty probes by the latch law's retention.
  // Accepted residuals, by design: a sole-interest node left
  // non-terminal by a dead build (the sub-second Failed transient in
  // cancel_build_derivations) or a stuck node in a >5000-node truncated
  // view follows until tab close — following a stuck node is the
  // idle-timeout rule's own posture (an hour-quiet stream means the
  // build is stuck).
  const isTerminal = () =>
    (poll?.allTerminal ?? false) ||
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

  {#if poll?.degraded}
    <!-- Data-bearing probe failure (merged_bug_081): a one-line
         staleness note that never REPLACES the retained graph or the
         tab body. poll.error stays reserved for the never-loaded
         state (Graph's banner arm). -->
    <p class="degraded" data-testid="poll-degraded">
      Live status probe failing ({poll.degraded}) — showing the last
      loaded graph.
    </p>
  {/if}

  <div
    class="tab-body"
    role="tabpanel"
    id="tabpanel-body"
    aria-labelledby="tab-{activeTab}"
  >
    {#if activeTab === 'logs'}
      {#if focusedDrv !== undefined}
        <!-- Keyed on buildId so switching builds (deep-link → different
             drawer target) tears down the old stream and starts a fresh
             one. Without the key Svelte reuses the component instance and
             the IIFE inside createLogStream keeps draining the prior
             build's fetch. -->
        {#key `${build.buildId}:${focusedDrv}:${focusedExecId}`}
          <LogViewer
            drvPath={focusedDrv}
            execId={focusedExecId}
            {isTerminal}
          />
        {/key}
      {:else}
        <!-- The unfocused Logs tab (bug_090): no stream object is
             constructed — there is nothing servable to stream. The
             empty TailLog selector is additionally refused at the
             api/logs.ts boundary as the backstop. No auto-focus: Q4
             fixes the mode SCOPE, not a selection policy. -->
        <div class="logs-unfocused" data-testid="logs-unfocused">
          <p>Logs are per-attempt. Pick a derivation in the Graph tab.</p>
          <p class="dim">
            Whole-build log aggregation is unavailable by design — no
            server-side aggregation contract exists.
          </p>
        </div>
      {/if}
    {:else if poll}
      <!-- Keyed on buildId so a build switch tears down Graph's layout
           state + WebWorker cleanly. The POLL is not Graph's anymore
           (merged_bug_134): it lives drawer-wide above, so the
           terminality oracle stays live while the Logs tab is active.
           The click no longer captures status — the oracle derives it
           from the poll. -->
      {#key build.buildId}
        <Graph
          {poll}
          ondrvclick={(drv, execId) => {
            focusedDrv = drv;
            focusedExecId = execId;
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
  .degraded {
    margin: 0.5rem 0 0;
    font-size: 0.8125rem;
    color: #92400e;
  }
  .logs-unfocused {
    padding: 2rem 0;
    color: #6b7280;
    text-align: center;
  }
  .logs-unfocused .dim {
    font-size: 0.8125rem;
  }
  .tab-body {
    padding: 1rem 0;
  }
</style>
