<script lang="ts">
  // Executors page: listExecutors poll + kind filter. The list is the
  // open-attempt view (one row per in-flight pull-mode attempt). The
  // busy/idle pill is the one operator affordance a metrics dashboard
  // can't give you: under one-build-per-pod the load is binary (so a
  // pill, not a bar). The "pulled" column is plain relative attempt
  // age (CLI parity) — NO staleness highlight: the timestamp never
  // advances mid-build (pull-mode pods send nothing between the pull
  // and the report), so any client-side threshold re-creates the
  // inverted "every long build screams red" signal (bug_357); wedged
  // pods are owned by the OA2 alert + the controller's Job census.
  // Per-executor drain retired with the stream protocol — eviction is
  // cordon + cancel/Job-delete (cluster-side), so there is no per-row
  // action button.
  //
  // r[impl dash.executors.kind-filter]
  // The kind filter is the dashboard surface for the ADR-019 builder/
  // fetcher split — lets the operator narrow to just the airgapped
  // builders or just the open-egress fetchers when diagnosing.
  import { admin } from '../api/admin';
  import Pill from '../components/Pill.svelte';
  import type { ExecutorInfo } from '../api/types';
  import { fmtTsRel } from '../lib/buildInfo';
  import { startPoll } from '../lib/poll';

  // ExecutorKind wire values (build_types.proto). Keyed on raw numbers
  // per the BuildStatePill pattern — proto enums are const-enum-shaped
  // and svelte-check rejects value imports under isolatedModules.
  const KIND_META: Record<number, string> = {
    0: 'builder',
    1: 'fetcher',
  };

  // Executor-status colour table. Same pattern as BuildStatePill's
  // STATE_META — single source of truth for the palette, rendered via
  // the shared <Pill>. Unknown statuses fall through to neutral grey.
  const STATUS_META: Record<string, { bg: string; fg: string }> = {
    alive: { bg: '#cfe8cf', fg: '#1a5a1a' },
    draining: { bg: '#fde8c9', fg: '#7a4a00' },
    dead: { bg: '#f3cccc', fg: '#7a1a1a' },
  };
  const STATUS_FALLBACK = { bg: '#eee', fg: '#374151' };

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

  $effect(() => startPoll(refresh));

  function isBusy(e: ExecutorInfo): boolean {
    return e.busy;
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
        <th>pulled</th>
      </tr>
    </thead>
    <tbody>
      {#each filtered as e (e.executorId)}
        {@const sm = STATUS_META[e.status] ?? STATUS_FALLBACK}
        <tr>
          <td>{e.executorId}</td>
          <td data-testid="kind-cell">{KIND_META[e.kind] ?? '—'}</td>
          <td data-testid="status-pill"
            ><Pill label={e.status} bg={sm.bg} fg={sm.fg} /></td
          >
          <td>
            <span
              class="load-pill {isBusy(e) ? 'busy' : 'idle'}"
              data-testid="load-pill">{isBusy(e) ? 'busy' : 'idle'}</span
            >
          </td>
          <td data-testid="pulled-cell">{fmtTsRel(e.attemptOpened, now)}</td>
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
  .load-pill {
    display: inline-block;
    padding: 0.1rem 0.5rem;
    border-radius: 3px;
    font-size: 0.85em;
  }
  .load-pill.busy {
    background: #4a8;
    color: #fff;
  }
  .load-pill.idle {
    background: #eee;
    color: #666;
  }
</style>
