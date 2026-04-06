<script lang="ts">
  // Executors page: listExecutors poll + per-row DrainButton + kind
  // filter. The load bar and >30s-stale heartbeat are the two operator
  // affordances that a metrics dashboard can't give you: the bar is
  // per-executor capacity (not aggregate), the red-timestamp is the
  // "something's wrong with this node, go look at its pod" signal.
  //
  // r[impl builder.executor.kind-gate]
  // The kind filter is the dashboard surface for the ADR-019 builder/
  // fetcher split — lets the operator narrow to just the airgapped
  // builders or just the open-egress fetchers when diagnosing.
  import { admin } from '../api/admin';
  import DrainButton from '../components/DrainButton.svelte';
  import type { ExecutorInfo } from '../api/types';
  import { fmtTsRel, tsToMs } from '../lib/buildInfo';

  // 30s matches the scheduler's dead-executor threshold (heartbeat period
  // is 10s, dead after 3 misses — see scheduler spec). A heartbeat
  // older than that and still status=alive means the scheduler hasn't
  // swept yet; the operator gets the heads-up first.
  const STALE_MS = 30_000;

  // ExecutorKind wire values (build_types.proto). Keyed on raw numbers
  // per the BuildStatePill pattern — proto enums are const-enum-shaped
  // and svelte-check rejects value imports under isolatedModules.
  const KIND_META: Record<number, string> = {
    0: 'builder',
    1: 'fetcher',
  };

  let executors = $state<ExecutorInfo[]>([]);
  let error = $state<string | null>(null);
  // now is rune state so the "Xs ago" strings re-render on each poll
  // tick without re-fetching. The poll interval updates both.
  let now = $state(Date.now());
  // 'all' | '0' | '1' — string because <select> values are strings. The
  // numeric leg matches ExecutorInfo.kind after Number() coercion.
  let kindFilter = $state<string>('all');

  const filtered = $derived(
    kindFilter === 'all'
      ? executors
      : executors.filter((e) => e.kind === Number(kindFilter)),
  );

  async function refresh() {
    try {
      const resp = await admin.listExecutors({ statusFilter: '' });
      executors = resp.executors;
      now = Date.now();
      error = null;
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    void refresh();
    const id = setInterval(refresh, 5000);
    return () => clearInterval(id);
  });

  function loadPct(e: ExecutorInfo): number {
    return e.maxBuilds > 0
      ? Math.round((e.runningBuilds / e.maxBuilds) * 100)
      : 0;
  }
</script>

<h2>Executors</h2>
<label>
  kind
  <select bind:value={kindFilter} data-testid="kind-filter">
    <option value="all">all</option>
    {#each Object.entries(KIND_META) as [k, label] (k)}
      <option value={k}>{label}</option>
    {/each}
  </select>
</label>

{#if error}
  <div role="alert">listExecutors failed: {error}</div>
{:else if filtered.length === 0}
  <p>no executors</p>
{:else}
  <table data-testid="executors-table">
    <thead>
      <tr>
        <th>executor</th>
        <th>kind</th>
        <th>status</th>
        <th>load</th>
        <th>size class</th>
        <th>heartbeat</th>
        <th></th>
      </tr>
    </thead>
    <tbody>
      {#each filtered as e (e.executorId)}
        <!-- Absent heartbeat (executor registered but never beat) is
             treated as stale; display reads "—" via fmtTsRel. -->
        {@const hb = tsToMs(e.lastHeartbeat)}
        {@const stale = hb === undefined || now - hb > STALE_MS}
        <tr>
          <td>{e.executorId}</td>
          <td data-testid="kind-cell">{KIND_META[e.kind] ?? '—'}</td>
          <td
            ><span
              class="pill pill-{e.status}"
              data-testid="status-pill"
              aria-label="status: {e.status}">{e.status}</span
            ></td
          >
          <td>
            <span class="load-bar" title="{e.runningBuilds}/{e.maxBuilds}">
              <span class="load-fill" style:width="{loadPct(e)}%"></span>
            </span>
            {e.runningBuilds}/{e.maxBuilds}
          </td>
          <td>{e.sizeClass || '—'}</td>
          <td class:stale data-testid="heartbeat-cell"
            >{fmtTsRel(e.lastHeartbeat, now)}</td
          >
          <td><DrainButton executorId={e.executorId} bind:executors /></td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<style>
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th,
  td {
    text-align: left;
    padding: 0.4rem 0.75rem;
    border-bottom: 1px solid #ddd;
  }
  .pill {
    display: inline-block;
    padding: 0.1rem 0.5rem;
    border-radius: 999px;
    font-size: 0.85em;
    background: #eee;
  }
  .pill-alive {
    background: #cfe8cf;
    color: #1a5a1a;
  }
  .pill-draining {
    background: #fde8c9;
    color: #7a4a00;
  }
  .pill-dead {
    background: #f3cccc;
    color: #7a1a1a;
  }
  .load-bar {
    display: inline-block;
    width: 5rem;
    height: 0.6rem;
    background: #eee;
    border-radius: 3px;
    overflow: hidden;
    vertical-align: middle;
    margin-right: 0.4rem;
  }
  .load-fill {
    display: block;
    height: 100%;
    background: #4a8;
  }
  .stale {
    color: #c22;
    font-weight: bold;
  }
</style>
