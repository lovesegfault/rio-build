<script lang="ts">
  // Workers page: listWorkers poll + per-row DrainButton. The load bar
  // and >30s-stale heartbeat are the two operator affordances that a
  // metrics dashboard can't give you: the bar is per-worker capacity
  // (not aggregate), the red-timestamp is the "something's wrong with
  // this node, go look at its pod" signal.
  import { timestampMs } from '@bufbuild/protobuf/wkt';
  import { admin } from '../api/admin';
  import DrainButton from '../components/DrainButton.svelte';
  import type { WorkerInfo } from '../gen/types_pb';

  // 30s matches the scheduler's dead-worker threshold (heartbeat period
  // is 10s, dead after 3 misses — see scheduler spec). A heartbeat
  // older than that and still status=alive means the scheduler hasn't
  // swept yet; the operator gets the heads-up first.
  const STALE_MS = 30_000;

  let workers = $state<WorkerInfo[]>([]);
  let error = $state<string | null>(null);
  // now is rune state so the "Xs ago" strings re-render on each poll
  // tick without re-fetching. The poll interval updates both.
  let now = $state(Date.now());

  async function refresh() {
    try {
      const resp = await admin.listWorkers({ statusFilter: '' });
      workers = resp.workers;
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

  // google.protobuf.Timestamp → ms since epoch. Absent timestamp (worker
  // registered but never heartbeated) is treated as "infinitely stale".
  function ageMs(w: WorkerInfo): number {
    return w.lastHeartbeat ? now - timestampMs(w.lastHeartbeat) : Infinity;
  }

  function rel(ms: number): string {
    if (!Number.isFinite(ms)) return 'never';
    if (ms < 1000) return 'now';
    const s = Math.floor(ms / 1000);
    if (s < 60) return `${s}s ago`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m}m ago`;
    return `${Math.floor(m / 60)}h ago`;
  }

  function loadPct(w: WorkerInfo): number {
    return w.maxBuilds > 0
      ? Math.round((w.runningBuilds / w.maxBuilds) * 100)
      : 0;
  }
</script>

{#if error}
  <div role="alert">listWorkers failed: {error}</div>
{:else if workers.length === 0}
  <p>no workers</p>
{:else}
  <table data-testid="workers-table">
    <thead>
      <tr>
        <th>worker</th>
        <th>status</th>
        <th>load</th>
        <th>size class</th>
        <th>heartbeat</th>
        <th></th>
      </tr>
    </thead>
    <tbody>
      {#each workers as w (w.workerId)}
        {@const age = ageMs(w)}
        <tr>
          <td>{w.workerId}</td>
          <td
            ><span class="pill pill-{w.status}" data-testid="status-pill"
              >{w.status}</span
            ></td
          >
          <td>
            <span class="load-bar" title="{w.runningBuilds}/{w.maxBuilds}">
              <span class="load-fill" style:width="{loadPct(w)}%"></span>
            </span>
            {w.runningBuilds}/{w.maxBuilds}
          </td>
          <td>{w.sizeClass || '—'}</td>
          <td
            class:stale={age > STALE_MS}
            data-testid="heartbeat-cell"
            >{rel(age)}</td
          >
          <td><DrainButton workerId={w.workerId} bind:workers /></td>
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
