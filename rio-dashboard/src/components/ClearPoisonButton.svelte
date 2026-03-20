<script lang="ts">
  // ClearPoison write action. Standalone button — the context-menu
  // embedding in DrvNode.svelte lands with P0280 (the DAG viz plan
  // owns that component). Until then it's composable from anywhere
  // that knows a `derivationHash` and a `poisoned` boolean.
  //
  // No optimistic state mutation here: the poisoned→queued transition
  // lives in the graph data P0280 polls, not a list this button owns.
  // Instead the parent gets an `onCleared` callback and can refetch.
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
