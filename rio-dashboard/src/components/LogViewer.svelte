<script lang="ts">
  // Follow-tail scroller over the logStream store. The store drives the
  // data; this component only decides when to pin the scroll to the
  // bottom. If the user scrolls up (to re-read an earlier line) we stop
  // auto-following — jumping them back to the tail mid-read is hostile.
  // Scrolling back within a small threshold of the bottom re-engages
  // the follow.
  import { createLogStream, type LogStream } from '../lib/logStream.svelte';

  let {
    buildId,
    drvPath = undefined,
  }: { buildId: string; drvPath?: string } = $props();

  // One stream per mount. Unmount aborts the underlying fetch via the
  // $effect teardown below — BuildDrawer destroys this component when
  // the drawer closes or the tab switches, so there's no dangling
  // connection left streaming into the void.
  //
  // svelte-ignore: capturing the initial prop value is intentional.
  // BuildDrawer wraps us in `{#key build.buildId}` so a changed buildId
  // remounts the component (fresh stream) rather than expecting us to
  // reconnect mid-flight. Reacting to prop churn here would double up
  // with that key and leak the prior AbortController.
  // svelte-ignore state_referenced_locally
  const stream: LogStream = createLogStream(buildId, drvPath);
  $effect(() => () => stream.destroy());

  let container: HTMLElement | undefined = $state();
  let follow = $state(true);

  // Threshold in px — "near enough to the bottom" to count as tailing.
  // One line-height of slop avoids a fight with the browser's fractional
  // scroll rounding.
  const TAIL_SLOP = 16;

  function onScroll() {
    if (!container) return;
    const gap =
      container.scrollHeight - container.scrollTop - container.clientHeight;
    follow = gap <= TAIL_SLOP;
  }

  // Auto-scroll effect. Reading `stream.lines` establishes the reactive
  // dependency; the assignment inside the guard keeps us pinned to the
  // bottom whenever a new chunk lands and the user hasn't scrolled away.
  // jsdom's layout is all-zeros (scrollHeight === 0) so this is a no-op
  // under vitest — the follow-tail behavior is covered manually.
  $effect(() => {
    void stream.lines;
    if (follow && container) {
      container.scrollTop = container.scrollHeight;
    }
  });
</script>

<div
  class="log-viewer"
  bind:this={container}
  onscroll={onScroll}
  data-testid="log-viewer"
>
  {#if stream.err}
    <div role="alert" class="err">log stream failed: {stream.err.message}</div>
  {/if}
  {#each stream.lines as line, i (i)}
    <pre class="line">{line}</pre>
  {/each}
  {#if !stream.done}
    <div class="tail" data-testid="log-tail">
      <span class="spinner" aria-hidden="true"></span> streaming…
    </div>
  {:else if stream.lines.length === 0 && !stream.err}
    <div class="empty">no log output</div>
  {/if}
</div>

<style>
  .log-viewer {
    height: 24rem;
    overflow-y: auto;
    background: #0f172a;
    color: #e2e8f0;
    font-family:
      ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace;
    font-size: 0.8125rem;
    line-height: 1.25rem;
    border-radius: 4px;
  }
  .line {
    margin: 0;
    padding: 0 0.75rem;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .tail,
  .empty {
    padding: 0.5rem 0.75rem;
    color: #64748b;
    font-style: italic;
  }
  .err {
    padding: 0.5rem 0.75rem;
    background: #7f1d1d;
    color: #fecaca;
  }
  .spinner {
    display: inline-block;
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: #64748b;
    margin-right: 0.5rem;
    animation: pulse 1s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 0.3;
    }
    50% {
      opacity: 1;
    }
  }
</style>
