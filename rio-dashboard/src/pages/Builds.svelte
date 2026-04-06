<script lang="ts">
  // r[impl dash.journey.build-to-logs]
  // Step 1 of the killer journey: "click build". This page renders the
  // paginated list and opens the detail drawer on row click. The Graph
  // tab inside the drawer (P0280) and the LogViewer it hosts (P0279)
  // close the remaining steps — this file owns only the entry point.
  import { untrack } from 'svelte';
  import { admin } from '../api/admin';
  import BuildDrawer from '../components/BuildDrawer.svelte';
  import BuildStatePill, {
    FILTERABLE_STATES,
    STATE_META,
  } from '../components/BuildStatePill.svelte';
  import type { BuildInfo } from '../api/types';
  import { progress, fmtTsRel, fmtDuration } from '../lib/buildInfo';

  // svelte-routing hands route params as props. `/builds/:id` populates
  // `id`; `/builds` leaves it undefined. The stub version kept this as
  // a plain `export let` for Svelte-4 interop, but everything else on
  // this page is runes-mode — mixing the two triggers a compile warning,
  // so we take the prop via $props() instead.
  let { id = undefined }: { id?: string } = $props();

  const PAGE_SIZE = 100;

  let builds = $state<BuildInfo[]>([]);
  let total = $state(0);
  let selected = $state<BuildInfo | null>(null);
  let statusFilter = $state('');
  // Cursor-chained paging (P0271's keyset path — first real consumer).
  // cursors[i] is the opaque token that fetches page i. cursors[0] is
  // undefined by construction: the scheduler treats an absent cursor as
  // "offset mode, page 1" and still returns next_cursor in the response,
  // so page 2 onward chain off the returned token rather than offset
  // arithmetic. The stack lets "Previous" re-use the already-known
  // cursor for page i-1 instead of re-deriving it (which keyset can't
  // do — cursors are forward-only).
  let cursors = $state<(string | undefined)[]>([undefined]);
  let pageIdx = $state(0);
  let error = $state<string | null>(null);

  // Deep-link fallback state: when the user lands on /builds/:id directly
  // (bookmark, shared URL), the list fetch above hasn't run yet and may
  // never contain the target build (wrong page, different filter). We
  // issue a one-shot unfiltered fetch with the scheduler's max clamp and
  // .find() the row. If the build is past the first 1000 this silently
  // doesn't find it — acceptable until a GetBuild(id) RPC lands (no
  // owner plan — current workaround: broad listBuilds+find).
  let deepLinkTried = $state(false);

  // The list effect tracks { statusFilter, pageIdx }. $effect re-runs
  // when either changes, so clicking a filter pill or paging triggers a
  // fresh RPC without an explicit watch wire-up. Filter reset is handled
  // in setFilter() (not here — keeps the dependency set minimal).
  //
  // `cursors` is read via untrack(): the async body WRITES back to it
  // (stashing next_cursor), and a tracked read would re-trigger the
  // effect on every stash — an extra wasted RPC per page. Safe because
  // the Next button only enables once cursors[pageIdx+1] is populated,
  // so by the time pageIdx++ fires, the cursor we need is already there.
  $effect(() => {
    // Capture deps synchronously — the async callback body wouldn't
    // otherwise establish them as tracking dependencies.
    const sf = statusFilter;
    const idx = pageIdx;
    const cur = untrack(() => cursors[idx]);
    (async () => {
      try {
        const r = await admin.listBuilds({
          statusFilter: sf,
          limit: PAGE_SIZE,
          // offset stays 0 — cursor wins when set (admin/builds.rs:93),
          // and on the first page (cursor undefined) we want offset 0
          // anyway. The offset-arithmetic path is gone.
          offset: 0,
          cursor: cur,
          tenantFilter: '',
        });
        builds = r.builds;
        // total_count is a full-table COUNT — capture it once on the
        // first page and hold it for the "page N / M" footer. Later
        // pages don't re-query (P0403-T3 option-(a): keyset path omits
        // the count to keep later-page latency flat).
        if (idx === 0) total = r.totalCount;
        // Stash the cursor for the NEXT page iff (a) the server sent
        // one (page was full) and (b) we haven't already stashed it
        // (re-fetch of the current page via filter wobble shouldn't
        // append a duplicate). Immutable-push so the Next button's
        // `cursors[pageIdx + 1]` dep sees the change.
        if (r.nextCursor && cursors.length === idx + 1) {
          cursors = [...cursors, r.nextCursor];
        }
        error = null;
      } catch (e) {
        error = String(e);
      }
    })();
  });

  // Deep-link resolver. Runs once per unique `id` (deepLinkTried gate
  // prevents re-entry on unrelated state churn). Kept separate from the
  // list $effect so pagination doesn't race it.
  $effect(() => {
    const target = id;
    if (!target || deepLinkTried) return;
    deepLinkTried = true;
    // Check the already-loaded page first — common case when navigating
    // from within the SPA (link on the Cluster page, say).
    const hit = builds.find((b) => b.buildId === target);
    if (hit) {
      selected = hit;
      return;
    }
    // Fallback: broad fetch. Scheduler clamps at 1000 (admin/builds.rs:33).
    admin
      .listBuilds({ statusFilter: '', limit: 1000, offset: 0, tenantFilter: '' })
      .then((r) => {
        const found = r.builds.find((b) => b.buildId === target);
        if (found) selected = found;
      })
      .catch(() => {
        // Swallow — the list effect above will surface transport errors.
        // A miss here just means the drawer stays closed.
      });
  });

  // Click-to-copy the full build_id. navigator.clipboard is stubbed in
  // jsdom (undefined), hence the optional chain — in the browser the
  // Permissions API gates it but the promise rejection is harmless.
  function copyId(buildId: string, ev: MouseEvent) {
    ev.stopPropagation(); // don't open the drawer when copying
    void navigator.clipboard?.writeText(buildId);
  }

  function setFilter(f: string) {
    statusFilter = f;
    // New filter → new result set → cursors from the old set are
    // meaningless. Reset the stack and start from page 0.
    cursors = [undefined];
    pageIdx = 0;
  }

  // Rough page count from the total captured on page 1. Shown for UX
  // orientation only — the Next button's actual gate is "do we have a
  // cursor for page i+1", which is exact.
  let lastPage = $derived(
    total === 0 ? 0 : Math.floor((total - 1) / PAGE_SIZE),
  );
</script>

<section data-testid="builds-page">
  <header class="filters">
    <button
      type="button"
      class:active={statusFilter === ''}
      onclick={() => setFilter('')}>all</button
    >
    {#each FILTERABLE_STATES as s (s)}
      <button
        type="button"
        class:active={statusFilter === STATE_META[s].filter}
        onclick={() => setFilter(STATE_META[s].filter)}
      >
        {STATE_META[s].label}
      </button>
    {/each}
  </header>

  {#if error}
    <div role="alert">scheduler unreachable: {error}</div>
  {:else}
    <table>
      <thead>
        <tr>
          <th>Build</th>
          <th>State</th>
          <th>Tenant</th>
          <th>Progress</th>
          <th>Submitted</th>
          <th>Duration</th>
        </tr>
      </thead>
      <tbody>
        {#each builds as b (b.buildId)}
          <!-- svelte-ignore a11y_interactive_supports_focus -->
          <tr
            data-testid="build-row"
            role="button"
            tabindex="0"
            onclick={() => (selected = b)}
            onkeydown={(e) => e.key === 'Enter' && (selected = b)}
          >
            <td>
              <code class="build-id" title={b.buildId}>
                {b.buildId.slice(0, 8)}…
              </code>
              <button
                type="button"
                class="copy"
                aria-label="Copy build ID"
                onclick={(e) => copyId(b.buildId, e)}>⧉</button
              >
            </td>
            <td><BuildStatePill state={b.state} /></td>
            <td>{b.tenantId || '—'}</td>
            <td>
              <progress value={progress(b)} max="100"></progress>
              <span class="pct">{progress(b)}%</span>
            </td>
            <td>{fmtTsRel(b.submittedAt)}</td>
            <td>{fmtDuration(b)}</td>
          </tr>
        {:else}
          <tr><td colspan="6" class="empty">no builds</td></tr>
        {/each}
      </tbody>
    </table>

    <footer class="pager">
      <button
        type="button"
        disabled={pageIdx === 0}
        onclick={() => (pageIdx = Math.max(0, pageIdx - 1))}>prev</button
      >
      <span>page {pageIdx + 1} / {lastPage + 1} ({total} total)</span>
      <button
        type="button"
        disabled={cursors[pageIdx + 1] === undefined}
        onclick={() => (pageIdx = pageIdx + 1)}>next</button
      >
    </footer>
  {/if}
</section>

{#if selected}
  <BuildDrawer build={selected} onclose={() => (selected = null)} />
{/if}

<style>
  .filters {
    display: flex;
    gap: 0.5rem;
    margin-bottom: 1rem;
  }
  .filters button {
    border: 1px solid #d1d5db;
    background: #fff;
    padding: 0.25rem 0.75rem;
    border-radius: 9999px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .filters button.active {
    background: #2563eb;
    border-color: #2563eb;
    color: #fff;
  }
  table {
    width: 100%;
    border-collapse: collapse;
  }
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid #e5e7eb;
  }
  tbody tr {
    cursor: pointer;
  }
  tbody tr:hover {
    background: #f9fafb;
  }
  .build-id {
    font-family: monospace;
  }
  .copy {
    border: none;
    background: transparent;
    cursor: pointer;
    font-size: 0.875rem;
    padding: 0 0.25rem;
  }
  td progress {
    width: 8rem;
    vertical-align: middle;
  }
  .pct {
    font-size: 0.75rem;
    color: #6b7280;
    margin-left: 0.25rem;
  }
  .empty {
    text-align: center;
    color: #9ca3af;
    font-style: italic;
  }
  .pager {
    display: flex;
    justify-content: center;
    align-items: center;
    gap: 1rem;
    margin-top: 1rem;
    font-size: 0.875rem;
  }
  .pager button {
    border: 1px solid #d1d5db;
    background: #fff;
    padding: 0.25rem 0.75rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .pager button:disabled {
    opacity: 0.4;
    cursor: default;
  }
</style>
