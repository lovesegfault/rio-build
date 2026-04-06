<script lang="ts">
  // Second server-stream consumer (first is P0279's log tailer). The
  // AbortController pattern is the same: stash a controller, pass its
  // signal into the RPC options, and call .abort() from both the
  // teardown and the explicit Cancel button. Connect-web turns abort
  // into an HTTP/2 RST which the tonic-web layer maps to drop-of-the-
  // body-stream — scheduler sees the gRPC status as Cancelled and
  // stops the sweep (or, for dry-run, just stops reporting).
  //
  // extraRoots is intentionally left empty: it's scheduler-populated
  // when GC runs via the controller's cron, not operator-supplied
  // through the dashboard (the operator doesn't have the live-build
  // output-path set). force stays false for the same reason — the
  // empty-refs safety gate is there to prevent catastrophic sweeps,
  // and the dashboard is not the place to bypass it.
  import { admin } from '../api/admin';
  import type { GCProgress } from '../gen/types_pb';
  import { toast } from '../lib/toast';

  let dryRun = $state(true);
  let gracePeriodHours = $state(24);
  let progress = $state<GCProgress | null>(null);
  let running = $state(false);
  let ctrl: AbortController | null = null;

  async function trigger() {
    if (running) return;
    progress = null;
    running = true;
    ctrl = new AbortController();
    try {
      const stream = admin.triggerGC(
        {
          dryRun,
          gracePeriodHours,
          extraRoots: [],
          force: false,
        },
        { signal: ctrl.signal },
      );
      for await (const p of stream) {
        progress = p; // pathsScanned / pathsCollected / bytesFreed
        if (p.isComplete) break;
      }
    } catch (e) {
      // aborted-by-us is not an error surface — the user hit Cancel.
      if (!ctrl?.signal.aborted) {
        toast.error(`GC stream failed: ${e}`);
      }
    } finally {
      running = false;
      ctrl = null;
    }
  }

  function cancel() {
    ctrl?.abort();
  }

  // Teardown: if the page unmounts mid-stream, abort the RPC so the
  // scheduler doesn't keep scanning for a client that's gone.
  $effect(() => () => ctrl?.abort());

  // bigint → human. GCProgress fields are uint64 on the wire → bigint
  // in protobuf-es v2. A terabyte fits in a double, so Number() is
  // lossless in practice; the store is not petabyte-scale.
  function bytes(n: bigint): string {
    const x = Number(n);
    if (x >= 1 << 30) return `${(x / (1 << 30)).toFixed(1)} GiB`;
    if (x >= 1 << 20) return `${(x / (1 << 20)).toFixed(1)} MiB`;
    if (x >= 1 << 10) return `${(x / (1 << 10)).toFixed(1)} KiB`;
    return `${x} B`;
  }

  // progress bar ratio: collected/scanned when scanned>0, else 0.
  // When isComplete the bar fills to 100% regardless — mark sweep
  // may collect fewer than it scanned (most paths are live).
  const ratio = $derived.by(() => {
    if (!progress) return 0;
    if (progress.isComplete) return 1;
    const scanned = Number(progress.pathsScanned);
    const collected = Number(progress.pathsCollected);
    return scanned > 0 ? collected / scanned : 0;
  });
</script>

<h2>Garbage collection</h2>

<form
  onsubmit={(e) => {
    e.preventDefault();
    void trigger();
  }}
>
  <label>
    <input type="checkbox" bind:checked={dryRun} disabled={running} />
    dry-run (report only, don't delete)
  </label>
  <label>
    grace period (hours)
    <input
      type="number"
      min="0"
      bind:value={gracePeriodHours}
      disabled={running}
    />
  </label>
  <div class="actions">
    {#if running}
      <button type="button" onclick={cancel} data-testid="gc-cancel"
        >Cancel</button
      >
    {:else}
      <button type="submit" data-testid="gc-trigger"
        >{dryRun ? 'Preview GC' : 'Run GC'}</button
      >
    {/if}
  </div>
</form>

{#if progress}
  <div
    class="progress"
    role="progressbar"
    aria-valuenow={Math.round(ratio * 100)}
    aria-valuemin="0"
    aria-valuemax="100"
    data-testid="gc-progress"
  >
    <div class="progress-fill" style:width="{ratio * 100}%"></div>
  </div>
  <dl data-testid="gc-stats">
    <dt>scanned</dt>
    <dd>{progress.pathsScanned.toString()}</dd>
    <dt>collected</dt>
    <dd>{progress.pathsCollected.toString()}</dd>
    <dt>freed</dt>
    <dd>{bytes(progress.bytesFreed)}</dd>
    {#if progress.currentPath}
      <dt>current</dt>
      <dd class="path">{progress.currentPath}</dd>
    {/if}
    {#if progress.isComplete}
      <dt>status</dt>
      <dd data-testid="gc-complete">complete</dd>
    {/if}
  </dl>
{/if}

<style>
  form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    max-width: 24rem;
    margin-bottom: 1rem;
  }
  .actions {
    display: flex;
    gap: 0.5rem;
  }
  .progress {
    height: 0.75rem;
    background: #eee;
    border-radius: 4px;
    overflow: hidden;
    margin-bottom: 0.5rem;
  }
  .progress-fill {
    height: 100%;
    background: #4a8;
    transition: width 120ms linear;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.25rem 1rem;
  }
  dt {
    font-weight: bold;
  }
  .path {
    font-family: monospace;
    word-break: break-all;
  }
</style>
