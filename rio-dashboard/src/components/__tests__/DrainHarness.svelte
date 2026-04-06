<script lang="ts" module>
  // Module-level seed bucket. Svelte 5's `state_referenced_locally`
  // warning fires when a $props()-derived binding seeds another $state
  // (it wants a $derived). For a test harness the prop IS a one-shot
  // seed, so we side-step the whole class of warning by stashing the
  // fixture in module scope before mount and reading it there instead
  // of from the reactive props proxy.
  import type { WorkerInfo } from '../../api/types';

  let seed: WorkerInfo[] = [];
  // After mount, `live` holds the same $state proxy that DrainButton
  // binds into — tests can call `reassign()` to simulate the parent
  // Workers.svelte refresh() reassigning `workers` mid-RPC.
  let live: WorkerInfo[] = [];

  export function setSeed(ws: Partial<WorkerInfo>[]) {
    seed = ws as WorkerInfo[];
  }
  export function reassign(ws: Partial<WorkerInfo>[]) {
    // Splice in place: Svelte 5 $state is a deep proxy, so in-place
    // mutation is reactive. This mirrors what a poll-tick reassignment
    // looks like from DrainButton's perspective — the array identity
    // under `workers` now has different rows at different indices.
    live.splice(0, live.length, ...(ws as WorkerInfo[]));
  }
  export function getWorkers(): WorkerInfo[] {
    return live;
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
  // Expose the proxy to module scope so tests can mutate it mid-await.
  // DrainButton only mutates entries (workers[i].status = ...), never
  // reassigns `workers` wholesale, so holding the initial proxy is
  // correct here — the warning is a false positive for this pattern.
  // svelte-ignore state_referenced_locally
  live = workers;
</script>

{#each workers as w (w.workerId)}
  <span data-testid="status-{w.workerId}">{w.status}</span>
{/each}
<DrainButton {workerId} bind:workers />
