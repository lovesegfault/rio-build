// Svelte 5 runes-in-module: the `.svelte.ts` extension opts this file into
// the rune compiler pass so `$state` works outside a component. The
// returned object exposes plain getters over the reactive backing —
// consumers just read `stream.rows` in their own $derived/$effect and
// the dependency is tracked automatically.
//
// connect-web's server-streaming client returns an async iterable; we
// drive it inside a reconnect loop (an IIFE) and push decoded rows into
// the reactive array. The AbortController gives callers a destroy()
// that cancels the underlying fetch — the proxy sees the client going
// away and closes the upstream h2 stream, which the store's tonic
// handler observes as a dropped receiver.
//
// The stream comes from rio-store's LogService (build logs live in the
// store as immutable chunks + a PG manifest), not the scheduler's
// AdminService — see api/logs.ts.
//
// FOLLOW MODE (merged_bug_306 / bug_311): the tail is opened with
// `follow: true` and lives until the store stamps `is_complete` or the
// tail_next law says exit — a RUNNING build's Logs tab streams lines as
// they are ingested instead of freezing on a one-shot snapshot. Every
// chunk steps the shared cursor (`lineCursor.visitChunk`, the
// rio-log-kernel mirror): resent prefixes after a reconnect are
// deduplicated, and a forward jump becomes an explicit `gap` row — a
// dropped span renders as a marked hole, never a seamless splice.
import { Code, ConnectError } from '@connectrpc/connect';

import { logs } from '../api/logs';
import { assertNever, tailNext, visitChunk, type TailStopCause } from './lineCursor';

// r[impl dash.stream.log-tail+3]
// r[impl dash.log.cap]
// r[impl dash.log.virtualize]
// (Virtualization itself lives in LogViewer.svelte — windowed slice over
// this store's `rows` with spacer divs. Tracey doesn't scan .svelte; this
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
// eventually OOMs the tab. At MAX_ROWS we drop the oldest DROP_ROWS
// rows and flip `truncated` so the viewer renders a banner. The cap
// counts ROWS (lines + gap markers — gap rows are vanishingly rare next
// to lines, so the budget arithmetic is unchanged from the line-only
// cap). 50K rows at ~100 bytes/line ≈ 5MB of strings — generous for a
// dashboard tab, small enough the GC keeps up.
const MAX_ROWS = 50_000;
const DROP_ROWS = 10_000;

// Reconnect-loop budget, mirroring the gateway relay's run_tail shape:
// exponential backoff between re-opens (reset after any productive
// stream), and an armed-once post-terminal grace window — once the
// derivation is terminal we keep draining for GRACE_MS and then stop,
// flagging the log incomplete if the store never stamped completion.
const BACKOFF_BASE_MS = 250;
const BACKOFF_MAX_MS = 2_000;
const GRACE_MS = 5_000;

// One rendered row. MONOMORPHIC ON PURPOSE: every row carries all four
// fields (text empty for gaps, range zero for lines) so the rows array
// holds a single V8 hidden class — the virtualized viewer iterates this
// array on every scroll frame and a polymorphic shape would deopt the
// slice loop. Discriminate on `kind`.
export type LogRow = {
  kind: 'line' | 'gap';
  /** The decoded line; `''` for gap rows. */
  text: string;
  /** First missing line of a gap row; `0n` for line rows. */
  from: bigint;
  /** One past the last missing line of a gap row; `0n` for line rows. */
  until: bigint;
};

export type LogStream = {
  readonly rows: readonly LogRow[];
  readonly done: boolean;
  readonly err: Error | null;
  readonly truncated: boolean;
  readonly droppedLines: number;
  readonly incomplete: boolean;
  /** Interior gaps observed (gap rows pushed). Drives the banner split:
   * interior gaps vs missing tail are different failure stories. */
  readonly gapCount: number;
  destroy: () => void;
};

function lineRow(text: string): LogRow {
  return { kind: 'line', text, from: 0n, until: 0n };
}

function gapRow(from: bigint, until: bigint): LogRow {
  return { kind: 'gap', text: '', from, until };
}

/** setTimeout that resolves early (and cleans up) on abort. */
function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const finish = () => {
      signal.removeEventListener('abort', finish);
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(finish, ms);
    signal.addEventListener('abort', finish, { once: true });
  });
}

// Storage is keyed by `(drv_hash, exec_id)`. `drvPath` selects the
// derivation; `execId` selects the execution (the per-build observation
// from `GraphNode.exec_id` ← `build_derivations.exec_id`). Empty
// `execId` (the default — Cached / never-ran terminals / non-terminal
// have no per-build execution to observe) resolves server-side to the
// latest execution of the drv across all builds.
//
// `opts.isTerminal` is the live terminality closure (BuildDrawer passes
// one over the focused node's status + the build state). It feeds the
// tail_next exit law: a non-terminal stream re-opens forever (bounded
// only by unmount destroy()), a terminal one drains within the
// armed-once grace. Defaulting to `() => false` keeps callers without
// the closure streaming until the store's `is_complete` — which exits
// immediately regardless of terminality.
export function createLogStream(
  drvPath?: string,
  execId = '',
  opts?: { isTerminal?: () => boolean },
): LogStream {
  const rows = $state<LogRow[]>([]);
  let done = $state(false);
  let err = $state<Error | null>(null);
  let truncated = $state(false);
  let droppedLines = $state(0);
  let incomplete = $state(false);
  let gapCount = $state(0);
  const isTerminal = opts?.isTerminal ?? (() => false);
  const ctrl = new AbortController();

  function push(row: LogRow) {
    rows.push(row);
  }

  // Cap at MAX_ROWS, applied ONCE PER CHUNK (not per row — a giant
  // chunk would otherwise splice(0, 1) tens of thousands of times,
  // O(n²), and stabilize at MAX_ROWS instead of the hysteresis target).
  // DROP_ROWS gives hysteresis (don't splice every chunk once near the
  // cap). splice(0, k) is a single memmove and keeps the same proxied
  // array object (no $state churn).
  function applyCap() {
    if (rows.length > MAX_ROWS) {
      const excess = rows.length - (MAX_ROWS - DROP_ROWS);
      rows.splice(0, excess);
      truncated = true;
      droppedLines += excess;
    }
  }

  (async () => {
    // The served-stream cursor: the next line number we have not yet
    // rendered. Reconnects re-open at this watermark; the shared
    // visitChunk step dedups any resent overlap and names any jump.
    let cursor = 0n;
    // The store's own claim that everything durable was served, read
    // from EVERY message (empty finals included) — `tail_next`'s
    // served_complete input.
    let servedComplete = false;
    // Armed once, at the first post-terminal exit decision. `null`
    // until armed; epoch ms after.
    let graceDeadline: number | null = null;
    let backoff = BACKOFF_BASE_MS;
    let lastErr: Error | null = null;
    let everReceived = false;

    // r[impl dash.stream.idle-timeout+3]
    // The reconnect loop has NO idle timer of its own: while a stream
    // is open we only await the next message — an hour of builder
    // silence holds the stream open (the infra chain is provisioned for
    // it; see the rule). The loop acts solely on stream end/error.
    for (;;) {
      let cause: TailStopCause;
      let receivedThisAttempt = false;
      try {
        const stream = logs.tailLog(
          {
            derivation: drvPath ?? '',
            execId,
            sinceLine: cursor,
            follow: true,
          },
          { signal: ctrl.signal },
        );
        for await (const chunk of stream) {
          receivedThisAttempt = true;
          everReceived = true;
          servedComplete = chunk.isComplete;
          const visit = visitChunk(
            cursor,
            chunk.firstLineNumber,
            BigInt(chunk.lines.length),
          );
          switch (visit.kind) {
            case 'skip':
              // Zero-line keep-alive/final, or a fully-resent chunk:
              // nothing new, watermark untouched.
              break;
            case 'serve': {
              // Slice off the resent prefix (lines below the
              // watermark): yieldFrom is absolute, the buffer index is
              // chunk-relative. The offset fits in a number — chunk
              // sizes are bounded by the 16MiB codec ceiling.
              const offset = Number(visit.yieldFrom - chunk.firstLineNumber);
              for (let i = offset; i < chunk.lines.length; i++) {
                push(lineRow(decoder.decode(chunk.lines[i])));
              }
              cursor = visit.nextLine;
              break;
            }
            case 'gapThenServe': {
              // A span the store could not serve (dropped fan-out
              // batch, hand-deleted chunk, swept manifest rows): an
              // explicit gap row, then the chunk's lines. Never a
              // seamless splice.
              gapCount += 1;
              push(gapRow(visit.gapFrom, visit.gapUntil));
              for (let i = 0; i < chunk.lines.length; i++) {
                push(lineRow(decoder.decode(chunk.lines[i])));
              }
              cursor = visit.nextLine;
              break;
            }
            default:
              assertNever(visit);
          }
          applyCap();
          if (chunk.isComplete) {
            // The store stamps is_complete on the final chunk when the
            // execution is terminal AND the manifest contiguously covers
            // the log. Everything servable was served: exit immediately,
            // no grace needed.
            done = true;
            return;
          }
        }
        cause = 'naturalEnd';
      } catch (e) {
        // Swallow AbortError: that's our own destroy() firing.
        if (ctrl.signal.aborted) {
          done = true;
          return;
        }
        lastErr = e instanceof Error ? e : new Error(String(e));
        const code = ConnectError.from(e).code;
        if (!everReceived && code === Code.NotFound) {
          // The execution/manifest may simply not exist yet (the build
          // was just dispatched; the first chunk is still in flight to
          // the store). Mirrors the gateway relay's NotFound-retryable
          // open. Bounded by backoff + unmount destroy(), and by the
          // grace window once terminal.
          cause = 'openFailed';
        } else {
          cause = receivedThisAttempt ? 'transportErr' : 'openFailed';
        }
      }
      if (receivedThisAttempt) backoff = BACKOFF_BASE_MS;

      const terminal = isTerminal();
      if (terminal && graceDeadline === null) {
        graceDeadline = Date.now() + GRACE_MS;
      }
      const graceExpired = graceDeadline !== null && Date.now() >= graceDeadline;
      if (tailNext(cause, terminal, graceExpired, servedComplete) === 'exit') {
        // r[impl obs.log.incomplete-surfaced+2]
        // Exit with the store never having stamped completion: the
        // missing tail is usually the build error itself — flag it so
        // the viewer renders the banner instead of pretending the log
        // ended cleanly. A hard-down store with nothing rendered also
        // surfaces the last transport error.
        incomplete = !servedComplete;
        if (!servedComplete && rows.length === 0 && lastErr !== null) {
          err = lastErr;
        }
        done = true;
        return;
      }
      // Re-open after a backoff, capped at the remaining grace so the
      // last drain attempt lands before the deadline rather than
      // sleeping through it.
      let delay = backoff;
      if (graceDeadline !== null) {
        delay = Math.min(delay, Math.max(0, graceDeadline - Date.now()));
      }
      await sleep(delay, ctrl.signal);
      if (ctrl.signal.aborted) {
        done = true;
        return;
      }
      backoff = Math.min(backoff * 2, BACKOFF_MAX_MS);
    }
  })();

  return {
    get rows() {
      return rows;
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
    get gapCount() {
      return gapCount;
    },
    destroy: () => ctrl.abort(),
  };
}
