<script lang="ts">
  // r[impl dash.journey.build-to-logs]
  // First step only: "load the page" prerequisite. The full
  // click-build → DAG → click-node → log-stream chain is closed by the
  // Builds list + Graph node-click + LogViewer plans; this page proves
  // the browser → Envoy → scheduler round-trip works for a unary RPC.
  import { admin } from '../api/admin';
  import type { ClusterStatusResponse } from '../gen/admin_types_pb';

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
  // return value is the teardown. No onMount/onDestroy pair to keep
  // in sync, no stale-closure trap — the rune re-runs if deps change
  // (none here), and the cleanup fires on unmount.
  $effect(() => {
    // Fire once immediately; setInterval waits the full period first.
    void refresh();
    const id = setInterval(refresh, 5000);
    return () => clearInterval(id);
  });
</script>

{#if error}
  <div role="alert">scheduler unreachable: {error}</div>
{:else if !status}
  <div>loading…</div>
{:else}
  <dl data-testid="cluster-status">
    <dt>Workers</dt>
    <dd>{status.activeWorkers} active / {status.totalWorkers} total</dd>
    <dt>Builds</dt>
    <dd>{status.pendingBuilds} pending / {status.activeBuilds} active</dd>
    <dt>Derivations</dt>
    <dd>{status.queuedDerivations} queued / {status.runningDerivations} running</dd>
  </dl>
{/if}
