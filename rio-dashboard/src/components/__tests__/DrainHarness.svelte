<script lang="ts" module>
  // Module-level seed bucket. Svelte 5's `state_referenced_locally`
  // warning fires when a $props()-derived binding seeds another $state
  // (it wants a $derived). For a test harness the prop IS a one-shot
  // seed, so we side-step the whole class of warning by stashing the
  // fixture in module scope before mount and reading it there instead
  // of from the reactive props proxy.
  import type { ExecutorInfo } from '../../api/types';

  let seed: ExecutorInfo[] = [];
  // After mount, `live` holds the same $state proxy that DrainButton
  // binds into — tests can call `reassign()` to simulate the parent
  // Workers.svelte refresh() reassigning `executors` mid-RPC.
  let live: ExecutorInfo[] = [];

  export function setSeed(ws: Partial<ExecutorInfo>[]) {
    seed = ws as ExecutorInfo[];
  }
  export function reassign(ws: Partial<ExecutorInfo>[]) {
    // Splice in place: Svelte 5 $state is a deep proxy, so in-place
    // mutation is reactive. This mirrors what a poll-tick reassignment
    // looks like from DrainButton's perspective — the array identity
    // under `executors` now has different rows at different indices.
    live.splice(0, live.length, ...(ws as ExecutorInfo[]));
  }
  export function getWorkers(): ExecutorInfo[] {
    return live;
  }
</script>

<script lang="ts">
  // Test harness: owns the $state<ExecutorInfo[]> that DrainButton binds
  // into. Svelte 5 $bindable() requires the parent to hold the state;
  // the test can't pass a plain array and expect mutations to survive
  // the proxy boundary. This wrapper also renders the status so the
  // test asserts on what the *parent* sees, not internal button state.
  import DrainButton from '../DrainButton.svelte';

  let { executorId }: { executorId: string } = $props();

  let executors = $state(seed);
  // Expose the proxy to module scope so tests can mutate it mid-await.
  // DrainButton only mutates entries (executors[i].status = ...), never
  // reassigns `executors` wholesale, so holding the initial proxy is
  // correct here — the warning is a false positive for this pattern.
  // svelte-ignore state_referenced_locally
  live = executors;
</script>

{#each executors as w (w.executorId)}
  <span data-testid="status-{w.executorId}">{w.status}</span>
{/each}
<DrainButton {executorId} bind:executors />
