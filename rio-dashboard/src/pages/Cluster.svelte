<script lang="ts">
  // r[impl dash.journey.build-to-logs]
  // First step only: "load the page" prerequisite. The full
  // click-build → DAG → click-node → log-stream chain is closed by the
  // Builds list + Graph node-click + LogViewer plans; this page proves
  // the browser → Envoy → scheduler round-trip works for a unary RPC.
  import { navigate } from 'svelte-routing';
  import { admin } from '../api/admin';
  import type { ClusterStatusResponse } from '../api/types';
  import { startPoll } from '../lib/poll';

  let status = $state<ClusterStatusResponse | null>(null);
  let error = $state<string | null>(null);

  async function refresh() {
    try {
      status = await admin.clusterStatus({});
      error = null;
    } catch (e) {
      // ConnectError carries Code.Unavailable when Envoy can't reach the
      // scheduler; surface the raw string — ops can tell 503 from CORS
      // preflight failure at a glance.
      error = String(e);
    }
  }

  // $effect mirrors the component lifetime: one interval per mount,
  // teardown on unmount. startPoll bakes in the fire-immediately +
  // document.hidden gate; see lib/poll.ts.
  $effect(() => startPoll(refresh));
</script>

{#if error}
  <div role="alert">scheduler unreachable: {error}</div>
{:else if !status}
  <div>loading…</div>
{:else}
  <dl data-testid="cluster-status">
    <dt>Executors</dt>
    <dd>{status.activeExecutors} active / {status.totalExecutors} total</dd>
    <dt>Builds</dt>
    <dd>{status.pendingBuilds} pending / {status.activeBuilds} active</dd>
    <dt>Derivations</dt>
    <dd
      >{status.queuedDerivations} queued / {status.runningDerivations} running / {status.substitutingDerivations}
      substituting</dd
    >
  </dl>
  <!-- Management actions. GC is on its own page because TriggerGC is a
       server-stream — the progress UI needs space and unmount-cancel.
       navigate() (not <Link>) so Cluster.svelte stays testable in
       isolation without a Router context wrapper. -->
  <p>
    <button type="button" onclick={() => navigate('/gc')}>Run GC…</button>
  </p>
{/if}
