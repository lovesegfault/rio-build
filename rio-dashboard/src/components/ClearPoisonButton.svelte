<script lang="ts">
  // ClearPoison write action. Embedded in DrvNode.svelte's right-click
  // context menu; composable from anywhere that knows a
  // `derivationHash` and a `poisoned` boolean.
  //
  // No optimistic state mutation here: the poisoned→queued transition
  // lives in the graph data Graph.svelte polls, not a list this button
  // owns. Instead the parent gets an `onCleared` callback — DrvNode
  // closes its menu, and the next 5s poll picks up the new status.
  import { admin } from '../api/admin';
  import { toast } from '../lib/toast';

  let {
    derivationHash,
    poisoned,
    onCleared,
  }: {
    derivationHash: string;
    poisoned: boolean;
    onCleared?: () => void;
  } = $props();

  let busy = $state(false);

  async function clear() {
    if (!confirm(`Clear poison on ${derivationHash}?`)) return;
    busy = true;
    try {
      const resp = await admin.clearPoison({ derivationHash });
      if (resp.cleared) {
        toast.info(`cleared poison: ${derivationHash}`);
        onCleared?.();
      } else {
        // Server returned cleared=false: the derivation wasn't poisoned
        // by the time the RPC landed (race with a retry that succeeded,
        // or stale UI state). Not an error, but tell the operator.
        toast.info(`${derivationHash} was not poisoned`);
      }
    } catch (e) {
      toast.error(`clear poison failed: ${e}`);
    } finally {
      busy = false;
    }
  }
</script>

{#if poisoned}
  <button
    type="button"
    onclick={clear}
    disabled={busy}
    data-testid="clear-poison-btn"
  >
    Clear poison
  </button>
{/if}
