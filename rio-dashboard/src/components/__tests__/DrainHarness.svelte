<script lang="ts" module>
  // Module-level seed bucket. Svelte 5's `state_referenced_locally`
  // warning fires when a $props()-derived binding seeds another $state
  // (it wants a $derived). For a test harness the prop IS a one-shot
  // seed, so we side-step the whole class of warning by stashing the
  // fixture in module scope before mount and reading it there instead
  // of from the reactive props proxy.
  import type { WorkerInfo } from '../../gen/admin_types_pb';

  let seed: WorkerInfo[] = [];
  export function setSeed(ws: Partial<WorkerInfo>[]) {
    seed = ws as WorkerInfo[];
  }
</script>

<script lang="ts">
  // Test harness: owns the $state<WorkerInfo[]> that DrainButton binds
  // into. Svelte 5 $bindable() requires the parent to hold the state;
  // the test can't pass a plain array and expect mutations to survive
  // the proxy boundary. This wrapper also renders the status so the
  // test asserts on what the *parent* sees, not internal button state.
  import DrainButton from '../DrainButton.svelte';

  let { workerId }: { workerId: string } = $props();

  let workers = $state(seed);
</script>

{#each workers as w (w.workerId)}
  <span data-testid="status-{w.workerId}">{w.status}</span>
{/each}
<DrainButton {workerId} bind:workers />
