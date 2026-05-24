// Svelte 5 runes-in-module: the `.svelte.ts` extension opts this file into
// the rune compiler pass so `$state` works outside a component. The
// returned object exposes plain getters over the reactive backing —
// consumers just read `stream.lines` in their own $derived/$effect and
// the dependency is tracked automatically.
//
// connect-web's server-streaming client returns an async iterable; we
// drive it with a `for await` IIFE and push decoded lines into the
// reactive array. The AbortController gives callers a destroy() that
// cancels the underlying fetch — the proxy sees the client going away
// and closes the upstream h2 stream, which the store's tonic handler
// observes as a dropped receiver.
//
// The stream comes from rio-store's LogService (build logs live in the
// store as immutable chunks + a PG manifest), not the scheduler's
// AdminService — see api/logs.ts.
import { logs } from '../api/logs';

// r[impl dash.stream.log-tail+2]
// r[impl dash.log.cap]
// r[impl dash.log.virtualize]
// (Virtualization itself lives in LogViewer.svelte — windowed slice over
// this store's `lines` with spacer divs. Tracey doesn't scan .svelte; this
// is the scannable anchor at the data source the viewer renders.)
//
// R8: build output is raw bytes and can be non-UTF-8 (compiler locale
// garbage, a stray binary cat'd by a builder script). {fatal: false}
// makes decode() substitute U+FFFD rather than throwing — a single bad
// byte in a 10-minute log should never blank the whole viewer.
// The decoder is module-level-constructed once; stateless for our usage
// since each line is a full buffer (no streaming across call boundaries).
const decoder = new TextDecoder('utf-8', { fatal: false });

// Memory cap: unbounded growth means a stuck builder spinning output
// eventually OOMs the tab. At MAX_LINES we drop the oldest DROP_LINES
// and flip `truncated` so the viewer renders a banner. 50K lines at
// ~100 bytes/line ≈ 5MB of strings — generous for a dashboard tab,
// small enough the GC keeps up.
const MAX_LINES = 50_000;
const DROP_LINES = 10_000;

export type LogStream = {
  readonly lines: readonly string[];
  readonly done: boolean;
  readonly err: Error | null;
  readonly truncated: boolean;
  readonly droppedLines: number;
  readonly incomplete: boolean;
  destroy: () => void;
};

// Storage is keyed by `(drv_hash, exec_id)`. `drvPath` selects the
// derivation; `execId` selects the execution (the per-build observation
// from `GraphNode.exec_id` ← `build_derivations.exec_id`). Empty
// `execId` (the default — Cached / never-ran terminals / non-terminal
// have no per-build execution to observe) resolves server-side to the
// latest execution of the drv across all builds. `buildId` is no longer
// in the signature: the build view's `GetBuildGraph` already partitions
// the node set by build, and the log fetch is scoped to that.
export function createLogStream(drvPath?: string, execId = ''): LogStream {
  const lines = $state<string[]>([]);
  let done = $state(false);
  let err = $state<Error | null>(null);
  let truncated = $state(false);
  let droppedLines = $state(0);
  let incomplete = $state(false);
  const ctrl = new AbortController();

  (async () => {
    try {
      // sinceLine: 0n always fetches from the top. The field is uint64 on
      // the wire → bigint in the generated TS. A resume-from-offset would
      // look at `chunk.firstLineNumber + chunk.lines.length` but the
      // reconnect-on-transient-error story is not yet scoped (no plan
      // owns it — would ride along with a P0392-adjacent virtualization
      // follow-on if one is written). For now the stream lives and dies
      // with the component.
      //
      // follow: false — one-shot drain of the stored chunks (plus the
      // live in-memory buffer if the execution is still ingesting).
      // The TailLog reconnect contract (re-open at last_received+1 on
      // premature end) is what a future follow-mode would implement;
      // the one-shot read doesn't need it.
      const stream = logs.tailLog(
        { derivation: drvPath ?? '', execId, sinceLine: 0n, follow: false },
        { signal: ctrl.signal },
      );
      for await (const chunk of stream) {
        // Each chunk.lines entry is a Uint8Array (proto `bytes`). Map
        // through the lossy decoder. Svelte 5 runes proxy arrays deeply
        // so `.push()` triggers reactivity without a reassign. The prior
        // spread-reassign copied the entire existing array per chunk —
        // O(n) per update, O(n²) total for n lines — 50M ref-copies for
        // a 10K-line build emitted in 100-line chunks.
        //
        // Avoid spread-push: a single `.push(...chunk)` call expands to
        // one stack arg per line — V8's ~65K arg limit means a 100K-line
        // backfill chunk throws RangeError. Loop-push is O(chunk) either
        // way and has no arg ceiling. Svelte's $state proxy tracks
        // .push() per call; the for-await yields between chunks so the
        // per-line pushes batch into one microtask and one re-render.
        const decoded = chunk.lines.map((b: Uint8Array) => decoder.decode(b));
        for (const line of decoded) lines.push(line);
        // Cap at MAX_LINES. DROP_LINES gives hysteresis (don't splice
        // every chunk once near the cap). Single check post-push: if
        // over, trim to MAX_LINES - DROP_LINES. A giant chunk that
        // alone exceeds MAX_LINES gets its HEAD dropped — we keep the
        // TAIL (most recent). Prior `splice(0, DROP_LINES)` left a 70K
        // chunk at 60K — still over. splice(0, k) is a single memmove;
        // cheaper than slice+reassign and keeps the same proxied array
        // object (no $state churn).
        if (lines.length > MAX_LINES) {
          const excess = lines.length - (MAX_LINES - DROP_LINES);
          lines.splice(0, excess);
          truncated = true;
          droppedLines += excess;
        }
        // The store stamps is_complete on the final chunk when the
        // execution is terminal AND the chunk manifest contiguously
        // covers [0, final_line_count). We stop iterating rather than
        // waiting for the server to close the stream — the two happen
        // near-simultaneously but this lets the UI flip the "done"
        // indicator one chunk sooner.
        if (chunk.isComplete) {
          done = true;
          return;
        }
      }
      // Generator exhausted without isComplete — the build is still
      // running (the live ingest buffer was drained), the execution was
      // cancelled, the final lines never reached the store (the
      // builder's drain deadline expired during a store outage), or the
      // server shut down mid-stream. Treat as done so the spinner
      // doesn't lie, but flag the content as incomplete so LogViewer
      // can render a banner — the missing tail is usually the build
      // error itself. The error path below does NOT set this: the err
      // banner already signals abnormal termination.
      // r[impl obs.log.incomplete-surfaced]
      incomplete = true;
      done = true;
    } catch (e) {
      // Swallow AbortError: that's our own destroy() firing. Anything
      // else (transport failure, scheduler gone) surfaces as an error
      // the viewer can render inline.
      if (!ctrl.signal.aborted) {
        err = e instanceof Error ? e : new Error(String(e));
      }
      done = true;
    }
  })();

  return {
    get lines() {
      return lines;
    },
    get done() {
      return done;
    },
    get err() {
      return err;
    },
    get truncated() {
      return truncated;
    },
    get droppedLines() {
      return droppedLines;
    },
    get incomplete() {
      return incomplete;
    },
    destroy: () => ctrl.abort(),
  };
}
