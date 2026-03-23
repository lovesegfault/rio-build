<script lang="ts">
  // Follow-tail scroller over the logStream store. The store drives the
  // data; this component only decides when to pin the scroll to the
  // bottom. If the user scrolls up (to re-read an earlier line) we stop
  // auto-following — jumping them back to the tail mid-read is hostile.
  // Scrolling back within a small threshold of the bottom re-engages
  // the follow.
  //
  // Virtualization: rendering one <pre> per line means 10K+ DOM nodes
  // for a long build — every scroll recomputes layout for all of them,
  // and the follow-tail $effect's `scrollHeight` read forces layout on
  // every chunk arrival. With a fixed line-height the visible window is
  // trivial arithmetic from scrollTop, so we render only the slice in
  // view plus an overscan buffer, with fixed-height spacers filling the
  // off-screen ranges. The container's scrollHeight stays synthetic
  // (spacer-driven) but arithmetically identical to full render, so
  // follow-tail's scrollTop = scrollHeight still lands at the bottom.
  import { createLogStream, type LogStream } from '../lib/logStream.svelte';

  let {
    buildId,
    // TODO(P0280): drvPath live once node-click wires it. Currently
    // dead in practice — BuildDrawer.svelte passes only buildId.
    drvPath = undefined,
    // @internal test-only hook: jsdom layout is all-zeros so the
    // scroll-derived viewport range can't be exercised. A test stubs
    // this to assert slice bounds directly. Production never passes it.
    // Underscore prefix signals "don't use this" in autocomplete.
    _viewportOverride = undefined,
  }: {
    buildId: string;
    drvPath?: string;
    /** @internal test-only — jsdom layout is all-zeros; tests stub the viewport directly. */
    _viewportOverride?: { start: number; end: number };
  } = $props();

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
  let scrollTop = $state(0);
  let clientHeight = $state(0);

  // Threshold in px — "near enough to the bottom" to count as tailing.
  // One line-height of slop avoids a fight with the browser's fractional
  // scroll rounding.
  const TAIL_SLOP = 16;

  // Virtualization constants. lineH matches the `.log-viewer`
  // line-height in CSS; the .line elements are margin:0 so each row is
  // exactly one line-height tall. OVERSCAN pads the visible window so
  // fast scrolling doesn't flash blank regions before the next render.
  // Under jsdom both clientHeight and scrollTop stay 0, so startIdx is
  // always 0 and endIdx = OVERSCAN — the jsdom render-count test
  // asserts exactly that many nodes.
  //
  // lineH is measured from CSS line-height, not hardcoded. The
  // .log-viewer has line-height: 1.25rem — at 16px root that's 20px,
  // but browser zoom or user font settings change the root. Spacers
  // are in px; CSS is in rem — divergence means wrong viewport math.
  //
  // Measurement happens once on mount via the container ref. jsdom
  // returns "" for getComputedStyle().lineHeight → parseFloat yields
  // NaN → fall back to 20 (keeps the render-count tests deterministic).
  let lineH = $state(20);
  const OVERSCAN = 10;

  $effect(() => {
    if (container) {
      const parsed = parseFloat(getComputedStyle(container).lineHeight);
      if (parsed > 0) lineH = parsed;
    }
  });

  function onScroll() {
    if (!container) return;
    scrollTop = container.scrollTop;
    clientHeight = container.clientHeight;
    const gap =
      container.scrollHeight - container.scrollTop - container.clientHeight;
    follow = gap <= TAIL_SLOP;
  }

  // Visible-slice bounds, reactive to both scroll position and line
  // count. Reading `stream.lines.length` tracks the runes-proxied array
  // — push() bumps .length and this $derived reruns. When
  // _viewportOverride is supplied (tests), use it verbatim and skip the
  // arithmetic. In production the override is undefined; the derived
  // falls through to scroll math. The Math.min on endIdx clamps to the
  // array length so a near-bottom scroll doesn't slice past the end.
  const viewport = $derived.by(() => {
    const n = stream.lines.length;
    if (_viewportOverride) {
      return {
        start: Math.max(0, Math.min(_viewportOverride.start, n)),
        end: Math.max(0, Math.min(_viewportOverride.end, n)),
      };
    }
    const start = Math.max(0, Math.floor(scrollTop / lineH) - OVERSCAN);
    const visible = Math.ceil(clientHeight / lineH) + 2 * OVERSCAN;
    const end = Math.min(n, start + visible);
    return { start, end };
  });

  // Auto-scroll effect. Reading `stream.lines.length` establishes the
  // reactive dependency; the assignment inside the guard keeps us pinned
  // to the bottom whenever a new chunk lands and the user hasn't
  // scrolled away. With virtualization the spacers keep scrollHeight
  // arithmetically equal to (lines.length × lineH) + any header/footer,
  // so the same scrollTop = scrollHeight write lands at the tail. jsdom
  // layout is all-zeros so this is a no-op under vitest — the
  // follow-tail behavior is covered manually.
  $effect(() => {
    void stream.lines.length;
    if (follow && container) {
      container.scrollTop = container.scrollHeight;
      // Sync the reactive mirror so the viewport $derived reruns. The
      // native scroll event fires asynchronously after scrollTop
      // assignment; without this the visible slice lags one tick
      // behind the tail.
      scrollTop = container.scrollTop;
      clientHeight = container.clientHeight;
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
  {#if stream.truncated}
    <div class="truncated" data-testid="log-truncated">
      — {stream.droppedLines.toLocaleString()} earlier lines truncated —
    </div>
  {/if}
  <div
    class="spacer"
    style:height="{viewport.start * lineH}px"
    aria-hidden="true"
  ></div>
  <!-- Keys are viewport-relative, NOT log-absolute — when the cap
       splices the head, keyed nodes get new content without re-mount.
       Invisible today (no animate:); don't add animate: without
       rekeying on a stable offset like stream.firstLineNumber. -->
  {#each stream.lines.slice(viewport.start, viewport.end) as line, i (viewport.start + i)}
    <pre class="line" title={line}>{line}</pre>
  {/each}
  <div
    class="spacer"
    style:height="{(stream.lines.length - viewport.end) * lineH}px"
    aria-hidden="true"
  ></div>
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
    /* Fixed height is load-bearing for virtualization: lineH in the
       script must match. pre-wrap would let long lines grow to multiple
       rows and desync the spacer math, so we clip with ellipsis instead.
       Losing wrap on 200-char lines is the trade for O(viewport) DOM
       under 100K-line builds. */
    margin: 0;
    padding: 0 0.75rem;
    height: 1.25rem;
    white-space: pre;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .spacer {
    flex-shrink: 0;
  }
  .truncated {
    padding: 0.5rem 0.75rem;
    color: #64748b;
    font-style: italic;
    text-align: center;
    border-bottom: 1px dashed #334155;
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
