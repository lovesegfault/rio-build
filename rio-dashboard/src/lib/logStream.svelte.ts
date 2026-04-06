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

export type LogStream = {
  readonly lines: readonly string[];
  readonly done: boolean;
  readonly err: Error | null;
  destroy: () => void;
};

export function createLogStream(buildId: string, drvPath?: string): LogStream {
  let lines = $state<string[]>([]);
  let done = $state(false);
  let err = $state<Error | null>(null);
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
        // through the lossy decoder. Spread-reassign so Svelte's $state
        // proxy sees the change — `lines.push(...)` would work too
        // (runes proxy arrays deeply) but a fresh array makes the
        // follow-tail $effect's dependency unambiguous.
        const decoded = chunk.lines.map((b: Uint8Array) => decoder.decode(b));
        lines = [...lines, ...decoded];
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
    destroy: () => ctrl.abort(),
  };
}
