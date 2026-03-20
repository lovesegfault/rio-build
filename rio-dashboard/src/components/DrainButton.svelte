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
  import type { WorkerInfo } from '../gen/types_pb';
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

  async function drain() {
    // jsdom has no native confirm prompt mounted; the test stubs
    // window.confirm and we only gate on a truthy return. In a real
    // browser this is the last "are you sure" before a mutating RPC.
    if (!confirm(`Drain ${workerId}?`)) return;

    const idx = workers.findIndex((w) => w.workerId === workerId);
    if (idx < 0) return;
    const prev = workers[idx].status;
    // Optimistic: mark draining immediately. Svelte 5 $state tracks
    // deep mutations on the proxy, so in-place assignment is reactive
    // without reassigning the array.
    workers[idx].status = 'draining';
    busy = true;
    try {
      await admin.drainWorker({ workerId, force: false });
      toast.info(`draining ${workerId}`);
    } catch (e) {
      workers[idx].status = prev; // revert
      toast.error(`drain ${workerId} failed: ${e}`);
    } finally {
      busy = false;
    }
  }
</script>

<button type="button" onclick={drain} {disabled} data-testid="drain-btn">
  Drain
</button>
