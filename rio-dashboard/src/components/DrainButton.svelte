<script lang="ts">
  // DrainWorker write action with optimistic UI: the button flips the
  // parent-owned WorkerInfo[] entry to `draining` before the RPC returns,
  // and reverts on error. The `workers` array is bound from
  // Workers.svelte via `$bindable()` so the mutation propagates back
  // into the page's rune-backed state without an event roundtrip.
  //
  // The button is disabled once a worker is draining/dead — repeated
  // clicks on an already-draining row are a no-op server-side but the
  // optimistic overwrite would clobber a potential `dead` transition
  // observed by the next poll.
  import { admin } from '../api/admin';
  import type { WorkerInfo } from '../api/types';
  import { toast } from '../lib/toast';

  let {
    workerId,
    workers = $bindable(),
  }: { workerId: string; workers: WorkerInfo[] } = $props();

  let busy = $state(false);

  const target = $derived(workers.find((w) => w.workerId === workerId));
  const disabled = $derived(
    busy || !target || target.status !== 'alive',
  );

  // Key the mutation on workerId, not on an index captured before the
  // await: Workers.svelte's 5s refresh() reassigns `workers` wholesale,
  // so a pre-await index may point at the wrong row (or off the end) by
  // the time the catch fires. Re-find at each mutation site.
  function setStatus(s: string) {
    const i = workers.findIndex((w) => w.workerId === workerId);
    if (i >= 0) workers[i].status = s;
  }

  async function drain() {
    // jsdom has no native confirm prompt mounted; the test stubs
    // window.confirm and we only gate on a truthy return. In a real
    // browser this is the last "are you sure" before a mutating RPC.
    if (!confirm(`Drain ${workerId}?`)) return;

    // Capture `prev` by value — it's a string, safe to hold across the
    // await. The index is NOT safe to hold: see setStatus above.
    const found = workers.find((w) => w.workerId === workerId);
    if (!found) return;
    const prev = found.status;

    // Optimistic: mark draining immediately. Svelte 5 $state tracks
    // deep mutations on the proxy, so in-place assignment is reactive
    // without reassigning the array.
    setStatus('draining');
    busy = true;
    try {
      await admin.drainWorker({ workerId, force: false });
      toast.info(`draining ${workerId}`);
    } catch (e) {
      // Re-find post-await: `workers` may have been reassigned by the
      // parent's 5s refresh(). A pre-await idx would index into a fresh
      // array — wrong row or undefined. If findIndex returns -1 (worker
      // removed mid-await), the revert is a no-op — correct, row's gone.
      setStatus(prev);
      toast.error(`drain ${workerId} failed: ${e}`);
    } finally {
      busy = false;
    }
  }
</script>

<button type="button" onclick={drain} {disabled} data-testid="drain-btn">
  Drain
</button>
