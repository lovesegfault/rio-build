<script lang="ts">
  // DrainExecutor write action with optimistic UI: the button flips the
  // parent-owned ExecutorInfo[] entry to `draining` before the RPC returns,
  // and reverts on error. The `executors` array is bound from
  // Workers.svelte via `$bindable()` so the mutation propagates back
  // into the page's rune-backed state without an event roundtrip.
  //
  // The button is disabled once an executor is draining/dead — repeated
  // clicks on an already-draining row are a no-op server-side but the
  // optimistic overwrite would clobber a potential `dead` transition
  // observed by the next poll.
  import { admin } from '../api/admin';
  import type { ExecutorInfo } from '../api/types';
  import { toast } from '../lib/toast';

  let {
    executorId,
    executors = $bindable(),
  }: { executorId: string; executors: ExecutorInfo[] } = $props();

  const target = $derived(executors.find((w) => w.executorId === executorId));
  // Disabled when not-alive — the optimistic setStatus('draining') below
  // flips status before the RPC, so this clause alone covers in-flight
  // (no separate `busy` flag needed; revert-on-error restores 'alive'
  // which re-enables the button).
  const disabled = $derived(!target || target.status !== 'alive');

  // Key the mutation on executorId, not on an index captured before the
  // await: Workers.svelte's 5s refresh() reassigns `executors` wholesale,
  // so a pre-await index may point at the wrong row (or off the end) by
  // the time the catch fires. Re-find at each mutation site.
  function setStatus(s: string) {
    const i = executors.findIndex((w) => w.executorId === executorId);
    if (i >= 0) executors[i].status = s;
  }

  async function drain() {
    // jsdom has no native confirm prompt mounted; the test stubs
    // window.confirm and we only gate on a truthy return. In a real
    // browser this is the last "are you sure" before a mutating RPC.
    if (!confirm(`Drain ${executorId}?`)) return;

    // Capture `prev` by value — it's a string, safe to hold across the
    // await. The index is NOT safe to hold: see setStatus above.
    const found = executors.find((w) => w.executorId === executorId);
    if (!found) return;
    const prev = found.status;

    // Optimistic: mark draining immediately. Svelte 5 $state tracks
    // deep mutations on the proxy, so in-place assignment is reactive
    // without reassigning the array.
    setStatus('draining');
    try {
      await admin.drainExecutor({ executorId, force: false });
      toast.info(`draining ${executorId}`);
    } catch (e) {
      // Re-find post-await: `executors` may have been reassigned by the
      // parent's 5s refresh(). A pre-await idx would index into a fresh
      // array — wrong row or undefined. If findIndex returns -1 (executor
      // removed mid-await), the revert is a no-op — correct, row's gone.
      setStatus(prev);
      toast.error(`drain ${executorId} failed: ${e}`);
    }
  }
</script>

<button type="button" onclick={drain} {disabled} data-testid="drain-btn">
  Drain
</button>
