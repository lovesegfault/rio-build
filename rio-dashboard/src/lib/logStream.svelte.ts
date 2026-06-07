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
import {
  ReopenPacer,
  assertNever,
  tailNext,
  visitChunkKeyed,
  type StreamPhase,
  type TailStopCause,
} from './lineCursor';

export type { BannerView, StreamPhase } from './lineCursor';
export { bannerFor } from './lineCursor';

// r[impl dash.stream.log-tail+4]
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

// Reconnect-loop budget: re-open pacing delegated to ReopenPacer
// (lineCursor.ts) — the ladder resets ONLY on productive chunk
// verdicts (serve/gapThenServe), never on bare receipt — plus an
// armed-once post-terminal grace window: once the derivation is
// terminal we keep draining for GRACE_MS and then stop, flagging the
// log incomplete if the store never stamped completion.
const GRACE_MS = 5_000;

// merged_bug_254: how often an OPEN, quiet stream re-checks
// terminality. The pre-fix `for await` parked the loop inside the
// iterator with no way to observe a terminal flip until the stream
// ended — a never-ending quiet stream on a terminal build kept the tab
// in "streaming" forever. The manually-driven iterator races each
// next() against this tick so the grace clock arms and fires
// mid-stream, finalizing through the SAME tail_next path every other
// exit uses.
const TERMINAL_TICK_MS = 1_000;

// One rendered row. MONOMORPHIC ON PURPOSE: every row carries all four
// fields (text empty for gaps, range zero for lines) so the rows array
// holds a single V8 hidden class — the virtualized viewer iterates this
// array on every scroll frame and a polymorphic shape would deopt the
// slice loop. Discriminate on `kind`.
export type LogRow = {
  kind: 'line' | 'gap' | 'execSwitch';
  /** The decoded line; `''` for gap rows; the NEW execution id for
   * execSwitch rows (merged_bug_002 — the numbering restarted). */
  text: string;
  /** First missing line of a gap row; `0n` for line/execSwitch rows. */
  from: bigint;
  /** One past the last missing line of a gap row; `0n` otherwise. */
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
  /** The store demanded credentials the KeylessOnly dashboard does not
   * hold (terminal — never retried). The viewer renders the
   * sign-in-required notice instead of the incomplete-log banner. */
  readonly authRequired: boolean;
  /** The terminal-cause surface the banner zone renders from. The
   * boolean flags above remain for tests/back-compat; the phase is the
   * law-bearing render input (bug_065). */
  readonly phase: StreamPhase;
  destroy: () => void;
};

function lineRow(text: string): LogRow {
  return { kind: 'line', text, from: 0n, until: 0n };
}

function gapRow(from: bigint, until: bigint): LogRow {
  return { kind: 'gap', text: '', from, until };
}

function execSwitchRow(newExecId: string): LogRow {
  return { kind: 'execSwitch', text: newExecId, from: 0n, until: 0n };
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
  let authRequired = $state(false);
  let phase = $state<StreamPhase>({ kind: 'streaming' });
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
    const pacer = new ReopenPacer();
    let lastErr: Error | null = null;
    let everReceived = false;

    // The execution whose numbering the cursor lives in. Empty until
    // the first exec-stamped chunk; a later chunk from a DIFFERENT
    // execution is a retry on another worker whose numbering restarted
    // at zero (merged_bug_002) — the keyed visit forces the switch arm
    // below instead of silently swallowing the new build's lines as
    // "duplicates".
    let lastExecId = '';

    // r[impl dash.stream.idle-timeout+3]
    // The reconnect loop has NO idle timer of its own: while a stream
    // is open we only await the next message — an hour of builder
    // silence holds the stream open (the infra chain is provisioned for
    // it; see the rule). The 1 s tick below observes TERMINALITY, not
    // idleness: it never closes a stream on silence alone, it lets the
    // armed-once grace clock run mid-stream (merged_bug_254).
    for (;;) {
      let cause: TailStopCause;
      let receivedThisAttempt = false;
      // Per-attempt controller (merged_bug_254): the mid-stream grace
      // cutoff cancels THIS attempt's fetch without touching the
      // master; destroy() aborts the master which chains here.
      const attempt = new AbortController();
      const chainAbort = () => attempt.abort();
      ctrl.signal.addEventListener('abort', chainAbort, { once: true });
      let graceCutoff = false;
      try {
        const stream = logs.tailLog(
          {
            derivation: drvPath ?? '',
            execId,
            sinceLine: cursor,
            follow: true,
          },
          { signal: attempt.signal },
        );
        // Manually-driven iterator: each pending next() is raced
        // against the terminal tick. The SAME next() promise is reused
        // across lost races (an async iterator must never have two
        // concurrent next() calls in flight).
        type TailChunk = {
          execId: string;
          lines: Uint8Array[];
          firstLineNumber: bigint;
          isComplete: boolean;
        };
        const it: AsyncIterator<TailChunk> = stream[Symbol.asyncIterator]();
        // The abort edge joins every race: a mock or proxy stream that
        // ignores its signal must not be able to hold the loop hostage
        // — destroy() and the grace cutoff resolve this promise even
        // when the transport never rejects.
        const abortEdge = new Promise<{ kind: 'abort' }>((resolve) => {
          attempt.signal.addEventListener('abort', () => resolve({ kind: 'abort' }), {
            once: true,
          });
        });
        // Wrapped ONCE and reused across lost races: an async iterator
        // must never have two concurrent next() calls in flight, and
        // re-wrapping per race would deepen the microtask chain.
        let pendingNext: Promise<{
          kind: 'msg';
          r: IteratorResult<TailChunk>;
        }> | null = null;
        stream_loop: for (;;) {
          const nextMsg = (pendingNext ??= it
            .next()
            .then((r) => ({ kind: 'msg' as const, r })));
          const tickCtrl = new AbortController();
          const winner = await Promise.race([
            nextMsg,
            abortEdge,
            sleep(TERMINAL_TICK_MS, tickCtrl.signal).then(() => ({
              kind: 'tick' as const,
            })),
          ]);
          tickCtrl.abort();
          if (winner.kind === 'abort') {
            // Master destroy or grace cutoff; the shared post-stream
            // path sorts out which.
            break;
          }
          if (winner.kind === 'tick') {
            // Observe terminality mid-stream: arm the grace clock the
            // first time the closure says terminal, and once the
            // armed deadline passes, cut THIS attempt — the exit
            // decision itself happens on the shared tail_next path
            // below, same as every other stream end.
            if (isTerminal() && graceDeadline === null) {
              graceDeadline = Date.now() + GRACE_MS;
            }
            if (graceDeadline !== null && Date.now() >= graceDeadline) {
              graceCutoff = true;
              attempt.abort();
              break stream_loop;
            }
            continue;
          }
          const { r } = winner;
          pendingNext = null;
          if (r.done) {
            break;
          }
          const chunk = r.value;
          receivedThisAttempt = true;
          everReceived = true;
          servedComplete = chunk.isComplete;
          // The execution axis FIRST (merged_bug_002): an empty
          // chunk-side id matches anything (pre-exec-stamping
          // servers), and the first stamped id adopts silently.
          let keysMatch =
            chunk.execId === '' || lastExecId === '' || chunk.execId === lastExecId;
          if (lastExecId === '' && chunk.execId !== '') {
            lastExecId = chunk.execId;
          }
          // The keyed visit may demand a re-visit of the SAME chunk
          // after the switch arm resets the floor.
          keyed_visit: for (;;) {
            const keyed = visitChunkKeyed(
              keysMatch,
              cursor,
              chunk.firstLineNumber,
              BigInt(chunk.lines.length),
            );
            if (keyed.kind === 'execSwitch') {
              // A retry on another worker: numbering restarted. An
              // explicit switch row — never a seamless splice or a
              // silent swallow — then the cursor lives in the new
              // execution's numbering.
              push(execSwitchRow(chunk.execId));
              cursor = 0n;
              lastExecId = chunk.execId;
              keysMatch = true;
              continue keyed_visit;
            }
            const visit = keyed.visit;
            // r[impl dash.stream.reopen-pacing]
            // The pacer sees the VERDICT, not the receipt: a keep-alive
            // or fully-resent chunk (skip) is not progress and must not
            // reset the re-open ladder (merged_bug_054's flat 4 Hz poll).
            pacer.noteVisit(visit);
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
            break;
          }
          applyCap();
          if (chunk.isComplete) {
            // The store stamps is_complete on the final chunk when the
            // execution is terminal AND the manifest contiguously covers
            // the log. Everything servable was served: exit immediately,
            // no grace needed.
            phase = { kind: 'complete' };
            done = true;
            return;
          }
        }
        if (ctrl.signal.aborted) {
          // destroy() while the transport stayed silent (signal-deaf
          // mocks, an already-buffered stream): same exit as the
          // rejected-transport path below.
          phase = servedComplete
            ? { kind: 'complete' }
            : { kind: 'incomplete', err: null };
          done = true;
          return;
        }
        // A grace cutoff that broke the loop without a transport throw
        // (signal-deaf mock) still decides on the shared path below
        // with graceExpired=true.
        cause = 'naturalEnd';
      } catch (e) {
        // Swallow AbortError: our own destroy() (master) or the
        // mid-stream grace cutoff (attempt) firing.
        if (ctrl.signal.aborted) {
          phase = servedComplete
            ? { kind: 'complete' }
            : { kind: 'incomplete', err: null };
          done = true;
          return;
        }
        if (graceCutoff) {
          // The grace clock cut a quiet open stream: decide on the
          // SAME path as a natural end — tail_next sees
          // graceExpired=true and exits, flagging incompleteness
          // exactly like every other exit (merged_bug_254).
          cause = 'naturalEnd';
        } else {
          lastErr = e instanceof Error ? e : new Error(String(e));
          const connectErr = ConnectError.from(e);
          const code = connectErr.code;
          if (code === Code.Unauthenticated || code === Code.PermissionDenied) {
            // The store demanded credentials. The dashboard is
            // registry-declared KeylessOnly (merged_bug_108, owner
            // decision Q1 2026-06-04) — terminal, never retried: an auth
            // deny does not heal by reconnecting, and the pre-fix
            // `openFailed` classification polled the store forever.
            cause = 'authRequired';
          } else if (connectErr.metadata.get('x-rio-log-unservable') !== null) {
            // merged_bug_164's reader half: the store typed this
            // failure as unservable-forever (an uncovered manifest
            // hole, a corrupt row). Every future open refuses
            // identically — the pre-fix loop re-dialed it once per
            // pacer delay until grace.
            cause = 'permanentErr';
          } else if (!everReceived && code === Code.NotFound) {
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
      } finally {
        ctrl.signal.removeEventListener('abort', chainAbort);
      }
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
        if (cause === 'authRequired') {
          // The terminal auth-required surface: the viewer renders the
          // sign-in notice, not the incomplete-log banner heuristics.
          authRequired = true;
          err = lastErr;
          phase = { kind: 'authRequired', err: lastErr };
        } else if (cause === 'permanentErr') {
          // A typed-permanent hole: the incomplete banner tells the
          // truth (some lines can never be served) and the error rides
          // along for the detail line.
          err = lastErr;
          phase = { kind: 'permanentHole', err: lastErr };
        } else if (!servedComplete && rows.length === 0 && lastErr !== null) {
          err = lastErr;
          phase = { kind: 'incomplete', err: lastErr };
        } else {
          phase = servedComplete
            ? { kind: 'complete' }
            : { kind: 'incomplete', err: null };
        }
        done = true;
        return;
      }
      // Re-open after the pacer's delay, capped at the remaining grace
      // so the last drain attempt lands before the deadline rather
      // than sleeping through it.
      let delay = pacer.nextDelayMs();
      if (graceDeadline !== null) {
        delay = Math.min(delay, Math.max(0, graceDeadline - Date.now()));
      }
      await sleep(delay, ctrl.signal);
      if (ctrl.signal.aborted) {
        phase = servedComplete
          ? { kind: 'complete' }
          : { kind: 'incomplete', err: null };
        done = true;
        return;
      }
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
    get authRequired() {
      return authRequired;
    },
    get phase() {
      return phase;
    },
    destroy: () => ctrl.abort(),
  };
}
