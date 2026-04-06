<script lang="ts">
  // Workers page: listExecutors poll + per-row DrainButton. The load bar
  // and >30s-stale heartbeat are the two operator affordances that a
  // metrics dashboard can't give you: the bar is per-executor capacity
  // (not aggregate), the red-timestamp is the "something's wrong with
  // this node, go look at its pod" signal.
  import { admin } from '../api/admin';
  import DrainButton from '../components/DrainButton.svelte';
  import type { ExecutorInfo } from '../api/types';
  import { fmtTsRel, tsToMs } from '../lib/buildInfo';

  // 30s matches the scheduler's dead-executor threshold (heartbeat period
  // is 10s, dead after 3 misses — see scheduler spec). A heartbeat
  // older than that and still status=alive means the scheduler hasn't
  // swept yet; the operator gets the heads-up first.
  const STALE_MS = 30_000;

  let executors = $state<ExecutorInfo[]>([]);
  let error = $state<string | null>(null);
  // now is rune state so the "Xs ago" strings re-render on each poll
  // tick without re-fetching. The poll interval updates both.
  let now = $state(Date.now());

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

  function loadPct(w: ExecutorInfo): number {
    return w.maxBuilds > 0
      ? Math.round((w.runningBuilds / w.maxBuilds) * 100)
      : 0;
  }
</script>

{#if error}
  <div role="alert">listExecutors failed: {error}</div>
{:else if executors.length === 0}
  <p>no executors</p>
{:else}
  <table data-testid="workers-table">
    <thead>
      <tr>
        <th>executor</th>
        <th>status</th>
        <th>load</th>
        <th>size class</th>
        <th>heartbeat</th>
        <th></th>
      </tr>
    </thead>
    <tbody>
      {#each executors as w (w.executorId)}
        <!-- Absent heartbeat (executor registered but never beat) is
             treated as stale; display reads "—" via fmtTsRel. -->
        {@const hb = tsToMs(w.lastHeartbeat)}
        {@const stale = hb === undefined || now - hb > STALE_MS}
        <tr>
          <td>{w.executorId}</td>
          <td
            ><span
              class="pill pill-{w.status}"
              data-testid="status-pill"
              aria-label="status: {w.status}">{w.status}</span
            ></td
          >
          <td>
            <span class="load-bar" title="{w.runningBuilds}/{w.maxBuilds}">
              <span class="load-fill" style:width="{loadPct(w)}%"></span>
            </span>
            {w.runningBuilds}/{w.maxBuilds}
          </td>
          <td>{w.sizeClass || '—'}</td>
          <td class:stale data-testid="heartbeat-cell"
            >{fmtTsRel(w.lastHeartbeat, now)}</td
          >
          <td><DrainButton executorId={w.executorId} bind:executors /></td>
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
