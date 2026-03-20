// Svelte 5 runes-in-module: the `.svelte.ts` extension opts this file into
// the rune compiler pass so `$state` works outside a component. The
// returned object exposes plain getters over the reactive backing —
// consumers just read `stream.lines` in their own $derived/$effect and
// the dependency is tracked automatically.
//
// connect-web's server-streaming client returns an async iterable; we
// drive it with a `for await` IIFE and push decoded lines into the
// reactive array. The AbortController gives callers a destroy() that
// cancels the underlying fetch — Envoy sees the client going away and
// closes the upstream h2 stream, which the scheduler's tonic handler
// observes as a dropped receiver.
import { admin } from '../api/admin';

// r[impl dash.stream.log-tail]
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
  destroy: () => void;
};

export function createLogStream(buildId: string, drvPath?: string): LogStream {
  const lines = $state<string[]>([]);
  let done = $state(false);
  let err = $state<Error | null>(null);
  let truncated = $state(false);
  const ctrl = new AbortController();

  (async () => {
    try {
      // sinceLine: 0n always fetches from the top. The field is uint64 on
      // the wire → bigint in the generated TS. A resume-from-offset would
      // look at `chunk.firstLineNumber + chunk.lines.length` but the
      // reconnect-on-transient-error story is a later plan; for now the
      // stream lives and dies with the component.
      const stream = admin.getBuildLogs(
        { buildId, derivationPath: drvPath ?? '', sinceLine: 0n },
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
        // Avoid spread: lines.push(...decoded) expands to one stack arg
        // per line — V8's ~65K arg limit means a 100K-line backfill
        // chunk throws RangeError. Loop-push is O(chunk) either way and
        // has no arg ceiling. Svelte's $state proxy tracks .push() per
        // call; the for-await yields between chunks so the per-line
        // pushes batch into one microtask and one re-render.
        const decoded = chunk.lines.map((b: Uint8Array) => decoder.decode(b));
        for (const line of decoded) lines.push(line);
        if (lines.length > MAX_LINES) {
          // splice(0, k) is a single memmove; cheaper than slice+reassign
          // and keeps the same proxied array object (no $state churn).
          lines.splice(0, DROP_LINES);
          truncated = true;
        }
        // The scheduler sets is_complete on the final chunk once the
        // build terminates (success or failure). We stop iterating
        // rather than waiting for the server to close the stream — the
        // two happen near-simultaneously but this lets the UI flip the
        // "done" indicator one chunk sooner.
        if (chunk.isComplete) {
          done = true;
          return;
        }
      }
      // Generator exhausted without isComplete — server closed the
      // stream early (shutdown, client abort observed). Treat as done
      // so the spinner doesn't lie.
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
    destroy: () => ctrl.abort(),
  };
}
