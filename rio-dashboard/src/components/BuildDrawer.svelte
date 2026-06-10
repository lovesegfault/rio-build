<script lang="ts" module>
  import {
    createBuildGraphPoll,
    type BuildGraphPoll,
  } from '../lib/buildGraphPoll.svelte';

  /** Per-build drawer session (merged_bug_064) — the keyed-lifecycle
   * law: state whose meaning is keyed by `build.buildId` has ONE owner
   * whose lifecycle IS the key. A key change replaces the WHOLE record
   * (the `$effect` in the instance script); retaining any field across
   * a re-point is unrepresentable — a key change has no per-field
   * write path, only the constructor.
   *
   * MEMBERSHIP RULE (derived; stated here so the next field lands on
   * the right side): a field belongs to this record iff its VALUE
   * references the keyed identity — `poll` (THIS build's graph),
   * `focusedDrv`/`focusedExecId` (THIS build's drvs/execs).
   * `activeTab` ('logs' | 'graph') is EXCLUDED: its value never
   * references the keyed identity (cross-build UI preference), and
   * with focus cleared a surviving Logs tab renders the
   * unfocused-by-design panel for the new build, which is correct. */
  export type DrawerSession = {
    /** THE key itself — the record is self-keying, so keyed consumers
     * ({#key}, Graph) structurally cannot pair this record's state
     * with a different build's identity, even in the one-flush window
     * between a prop write and the replacement effect. */
    buildId: string;
    /** THE node-status source (merged_bug_134): one session-lifetime
     * GetBuildGraph poll feeding Graph's rendering AND the log
     * stream's terminality oracle. */
    poll: BuildGraphPoll;
    /** Focused derivation (Graph tab click) — this build's drv. */
    focusedDrv: string | undefined;
    /** The focused node's exec pin — this build's observation. */
    focusedExecId: string;
  };

  /** The ONE session constructor: every field starts at its
   * fresh-build value. Exported for the headless census test —
   * `Object.keys(mkDrawerSession(...))` is the machine-derived
   * membership the wholesale-replacement witness iterates. */
  export function mkDrawerSession(buildId: string): DrawerSession {
    return {
      buildId,
      poll: createBuildGraphPoll(buildId),
      focusedDrv: undefined,
      focusedExecId: '',
    };
  }
</script>

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

  // THE per-build state record (merged_bug_064): minted by the single
  // constructor, replaced WHOLESALE in this one $effect keyed on
  // build.buildId — teardown destroys the old session's poll. The old
  // shape had three independent $state cells with ONE shared lifecycle
  // and no shared owner: the $effect recreated only the poll while
  // focusedDrv/focusedExecId survived a re-point (deep-link fallback
  // race, keyboard Enter on a row behind the click-only backdrop), so
  // the {#key} below remounted LogViewer with build B's id and build
  // A's NON-EMPTY exec-pinned selector — past the api/logs.ts
  // empty-selector backstop, streaming A's attempt log under B's
  // header, while statusOf(A-drv) on B's graph read undefined and the
  // oracle degraded to B's allTerminal. The poll must outlive
  // Graph.svelte (merged_bug_134) — that component unmounts whenever
  // the Logs tab is active, which is precisely when the oracle is
  // load-bearing. (null until the mount effect runs — the established
  // poll-effect idiom this record replaces.)
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
  // session.focusedExecId is the per-build observation of which
  // execution this build watched (`GraphNode.exec_id` ←
  // `build_derivations.exec_id`), captured at click time so the log
  // fetch can pin the EXACT execution rather than fall back to
  // "latest." Empty for Cached / never-ran terminals / non-terminal —
  // LogViewer renders an "approximate" banner for those. (Pinning by
  // click-time capture is correct: the {#key} below remounts LogViewer
  // when the pin changes.)
  let session = $state<DrawerSession | null>(null);
  // $effect.pre: the replacement lands BEFORE the DOM re-renders for a
  // prop change, so the template never evaluates (new build, old
  // session) — and the self-keyed record above makes that pairing
  // unrepresentable for keyed consumers even if the timing ever
  // changed.
  $effect.pre(() => {
    const s = mkDrawerSession(build.buildId);
    session = s;
    return () => s.poll.destroy();
  });

  // The focused node's status, DERIVED LIVE from the poll
  // (merged_bug_134 — never a click-time snapshot). The pre-fix
  // capture froze the terminality oracle in both directions: a node
  // clicked while running never read terminal after completing
  // mid-build (the stream re-dialed at the pacer cap, tab stuck
  // "streaming"), and a poisoned-at-click node stayed "terminal" after
  // ClearPoison (the stream exited incomplete instead of following the
  // retry).
  const focusedStatus = $derived(
    session ? session.poll.statusOf(session.focusedDrv) : undefined,
  );

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
    (session?.poll.allTerminal ?? false) ||
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

  {#if session && session.poll.degraded}
    <!-- Data-bearing probe failure (merged_bug_081): a one-line
         staleness note that never REPLACES the retained graph or the
         tab body. poll.error stays reserved for the never-loaded
         state (Graph's banner arm). -->
    <p class="degraded" data-testid="poll-degraded">
      Live status probe failing ({session.poll.degraded}) — showing the
      last loaded graph.
    </p>
  {/if}

  <div
    class="tab-body"
    role="tabpanel"
    id="tabpanel-body"
    aria-labelledby="tab-{activeTab}"
  >
    {#if activeTab === 'logs'}
      {#if session && session.focusedDrv !== undefined}
        <!-- Keyed on buildId so switching builds (deep-link → different
             drawer target) tears down the old stream and starts a fresh
             one. Without the key Svelte reuses the component instance and
             the IIFE inside createLogStream keeps draining the prior
             build's fetch. The focus fields ride the SAME session record
             as the poll, so a re-point can never key this mount with a
             stale selector (merged_bug_064). -->
        {#key `${session.buildId}:${session.focusedDrv}:${session.focusedExecId}`}
          <LogViewer
            drvPath={session.focusedDrv}
            execId={session.focusedExecId}
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
    {:else if session}
      {@const s = session}
      <!-- Keyed on buildId so a build switch tears down Graph's layout
           state + WebWorker cleanly. The POLL is not Graph's anymore
           (merged_bug_134): it lives session-wide above, so the
           terminality oracle stays live while the Logs tab is active.
           The click no longer captures status — the oracle derives it
           from the poll. -->
      {#key s.buildId}
        <Graph
          poll={s.poll}
          ondrvclick={(drv, execId) => {
            s.focusedDrv = drv;
            s.focusedExecId = execId;
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
