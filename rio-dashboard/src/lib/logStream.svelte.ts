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
  type CursorKey,
  type StreamPhase,
  type TailResolution,
  type TailStopCause,
} from './lineCursor';

export type { BannerView, StreamPhase } from './lineCursor';
export { bannerFor } from './lineCursor';

// r[impl dash.stream.log-tail+6]
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
// verdicts (serve/gapThenServe), never on bare receipt — plus a
// post-terminal grace window. The grace is a QUIET-TIME budget
// (merged_bug_035): it bounds how long a terminal stream may sit
// unproductive, not the total transfer — every productive serve
// re-arms it; keep-alives and resent chunks (the `skip` verdict)
// never extend it, so a flood of unproductive traffic still expires
// on schedule. Expiry flags the log incomplete if the store never
// stamped completion.
const GRACE_MS = 5_000;

// merged_bug_035: a re-open whose remaining grace cannot fund a real
// drain attempt is structurally futile — setTimeout never fires early,
// so a sleep capped AT the deadline wakes exactly at it and the next
// attempt is head-cut before a single message is read (one wasted
// TailLog RPC, then "incomplete"). The margin is one open round-trip
// plus first-chunk budget: with less than this remaining, finalize at
// the decision point instead of re-opening.
const DRAIN_MARGIN_MS = 1_000;

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
  /** Interior gap marker rows PRESENT — $derived from the rows array
   * (bug_169), so cap eviction cannot strand the count: the "holes are
   * marked inline" banner is true by construction (an evicted marker's
   * span lives inside the truncation banner's region instead). Drives
   * the banner split: interior gaps vs missing tail are different
   * failure stories. */
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
// quiet-time grace (productive serves re-arm it, merged_bug_035) — and
// a store-stamped completion on a NON-terminal
// derivation re-opens to follow the retry (merged_bug_063: is_complete
// is a per-EXECUTION predicate; the derivation may run again).
//
// Callers WITHOUT the closure have no terminality oracle: for them the
// store's own exec-level completion claim stands in for the law's
// terminal input (the best available knowledge — exit on is_complete,
// the documented legacy contract). With an oracle present the law is
// exact and the fallback never engages.
export function createLogStream(
  drvPath?: string,
  execId = '',
  opts?: { isTerminal?: () => boolean },
): LogStream {
  const rows = $state<LogRow[]>([]);
  // bug_169: DERIVED from the rows it describes — a counter maintained
  // beside the array desyncs the moment a third mutation site appears
  // (applyCap's splice did exactly that to the pushed-gaps counter).
  // O(rows) per recompute, lazily on read; the reduce touches only the
  // monomorphic `kind` field (~0.5ms at the 50K cap).
  const gapCount = $derived(
    rows.reduce((n, r) => (r.kind === 'gap' ? n + 1 : n), 0),
  );
  let done = $state(false);
  let err = $state<Error | null>(null);
  let truncated = $state(false);
  let droppedLines = $state(0);
  let incomplete = $state(false);
  let authRequired = $state(false);
  let phase = $state<StreamPhase>({ kind: 'streaming' });
  const hasOracle = opts?.isTerminal !== undefined;
  const isTerminal = opts?.isTerminal ?? (() => false);
  // The exit law's resolution-mode input (bug_348): a non-empty execId
  // PINS the stream to one execution — every re-open resends it, so a
  // retry is structurally unobservable and a stamped completion is
  // terminal by construction regardless of the derivation oracle.
  const mode: TailResolution = execId !== '' ? 'pinned' : 'latest';
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
      const spliced = rows.splice(0, excess);
      truncated = true;
      // bug_169: the truncation banner says "earlier LINES truncated"
      // — marker rows (gap/execSwitch) in the spliced prefix are not
      // lines and must not inflate it. gapCount needs no bookkeeping
      // here: it derives from the array this splice just mutated.
      for (const r of spliced) {
        if (r.kind === 'line') droppedLines += 1;
      }
    }
  }

  (async () => {
    // The served-stream cursor: the next line number we have not yet
    // rendered. Reconnects re-open at this watermark; the shared
    // visitChunk step dedups any resent overlap and names any jump.
    // THE RECONNECT CONTRACT (merged_bug_063): a nonzero watermark is
    // only ever sent for the execution it was minted in — an execution
    // switch resets it to 0 before any re-open, because the store
    // filters `since_line` inside the RESOLVED execution's numbering
    // and a stale watermark silently swallows the new execution's log
    // server-side.
    let cursor = 0n;
    // The store's own claim that everything durable was served —
    // `tail_next`'s served_complete input. PER-EXECUTION: adopted only
    // from chunks consumed in the cursor's own numbering, and RESET on
    // an execution switch (merged_bug_063: a stale claim minted against
    // the old numbering must never finish the new execution's tab).
    let servedComplete = false;
    // The law's terminal input. Without an oracle, the store's own
    // exec-level completion claim is the best available stand-in
    // (legacy contract: exit on is_complete); with one, the law is
    // exact and a completed-but-failed attempt follows the retry.
    const effTerminal = () => isTerminal() || (!hasOracle && servedComplete);
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
    // post-terminal grace clock run mid-stream (merged_bug_254).
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
      // merged_bug_063: an execution switch whose first sighted message
      // starts past line 0 means the new execution's head was filtered
      // SERVER-SIDE against the stale watermark — this attempt's view
      // is unrecoverable. The flag routes the deliberate cut through
      // the same naturalEnd path as the grace cutoff; the next open
      // goes out at the freshly reset sinceLine 0.
      let execSwitched = false;
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
        // An abort edge joins every race: a mock or proxy stream that
        // ignores its signal must not be able to hold the loop hostage
        // — destroy() and the grace cutoff resolve this promise even
        // when the transport never rejects.
        //
        // PER-ITERATION AND SELF-CLEANING (bug_277): the edge used to
        // be created once per attempt — a promise that never settles
        // on a healthy follow. Every race against it permanently
        // appended a PromiseReaction (one per chunk, one per 1 s
        // tick), retaining each iteration's result promise and closure
        // graph: a multi-hour follow leaked ~86k reactions/day from
        // the tick alone, bypassing the MAX_ROWS cap maintained for
        // exactly this concern. Each iteration now builds a fresh edge
        // and removes its listener once the race settles (mirroring
        // sleep()'s finish), so a lost edge is garbage, not a ledger.
        // (pendingNext is exempt by construction: the iterator
        // contract forces reuse across lost races, but it settles on
        // every message — at worst the keep-alive cadence — releasing
        // its reactions.)
        let abortListener: (() => void) | null = null;
        const abortEdgeFor = (): Promise<{ kind: 'abort' }> => {
          if (attempt.signal.aborted) {
            // A listener added to an already-aborted signal never
            // fires — resolve immediately instead.
            return Promise.resolve({ kind: 'abort' as const });
          }
          return new Promise((resolve) => {
            abortListener = () => resolve({ kind: 'abort' });
            attempt.signal.addEventListener('abort', abortListener, {
              once: true,
            });
          });
        };
        const dropAbortEdge = (): void => {
          if (abortListener !== null) {
            attempt.signal.removeEventListener('abort', abortListener);
            abortListener = null;
          }
        };
        // Wrapped ONCE and reused across lost races: an async iterator
        // must never have two concurrent next() calls in flight, and
        // re-wrapping per race would deepen the microtask chain.
        let pendingNext: Promise<{
          kind: 'msg';
          r: IteratorResult<TailChunk>;
        }> | null = null;
        stream_loop: for (;;) {
          // merged_bug_134: the oracle is LIVE in both directions —
          // ClearPoison flips a settled derivation back to
          // non-terminal. An armed deadline then belongs to a world
          // that no longer holds: un-arm it (a later terminal flip
          // re-arms a FRESH window). Without this, the stale clock
          // cuts the resumed follow at a grace minted for the
          // poisoned state. Self-clearing race edge; the m014 rider's
          // execSwitch re-derive is the same law at the other input.
          if (!effTerminal() && graceDeadline !== null) {
            graceDeadline = null;
          }
          // bug_145: arm AND enforce at the loop head, before racing.
          // The head runs after every message and every tick, so a
          // chatty stream arms the clock as promptly as a quiet one —
          // and an already-expired deadline cuts the attempt
          // deterministically, with no race for traffic to win.
          if (effTerminal() && graceDeadline === null) {
            graceDeadline = Date.now() + GRACE_MS;
          }
          if (graceDeadline !== null && Date.now() >= graceDeadline) {
            graceCutoff = true;
            attempt.abort();
            break stream_loop;
          }
          const nextMsg = (pendingNext ??= it
            .next()
            .then((r) => ({ kind: 'msg' as const, r })));
          const tickCtrl = new AbortController();
          // bug_145: once armed, the deadline joins the race as an
          // ABSOLUTE timer (the gateway's `sleep_until` shape,
          // log_tail.rs) — never a per-iteration relative tick that a
          // >1 msg/sec stream re-creates and starves forever. The 1 s
          // tick stays for the QUIET stream only: it wakes the loop so
          // the head observes a terminal flip with no traffic at all.
          const edges: Promise<
            | { kind: 'msg'; r: IteratorResult<TailChunk> }
            | { kind: 'abort' }
            | { kind: 'tick' }
            | { kind: 'grace' }
          >[] = [
            nextMsg,
            abortEdgeFor(),
            sleep(TERMINAL_TICK_MS, tickCtrl.signal).then(() => ({
              kind: 'tick' as const,
            })),
          ];
          if (graceDeadline !== null) {
            edges.push(
              sleep(
                Math.max(0, graceDeadline - Date.now()),
                tickCtrl.signal,
              ).then(() => ({ kind: 'grace' as const })),
            );
          }
          const winner = await Promise.race(edges);
          tickCtrl.abort();
          // bug_277: settle-time cleanup — the losing abort edge's
          // listener is removed so the edge promise (and every closure
          // it retains) is collectible. The tick/grace sleeps clean
          // themselves up via tickCtrl.abort() + sleep()'s finish.
          dropAbortEdge();
          if (winner.kind === 'abort') {
            // Master destroy or grace cutoff; the shared post-stream
            // path sorts out which.
            break;
          }
          if (winner.kind === 'grace') {
            // The absolute deadline edge fired mid-race: cut THIS
            // attempt — the exit decision itself happens on the shared
            // tail_next path below, same as every other stream end.
            graceCutoff = true;
            attempt.abort();
            break stream_loop;
          }
          if (winner.kind === 'tick') {
            // Nothing to do here: the tick only wakes the loop so the
            // head re-checks terminality on a quiet stream.
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
          // The execution axis FIRST (merged_bug_002), with the
          // cursor's PROVENANCE stated to the kernel mirror (bug_050):
          // an empty chunk-side id matches anything (pre-exec-stamping
          // servers); an unkeyed cursor (lastExecId === '') meeting a
          // stamped chunk is the adoption transition the KERNEL
          // decides — the old ad-hoc disjunct made the comparison
          // unconditionally true, so the first stamped chunk adopted
          // silently with no cursor/servedComplete reset (a retry's
          // server-filtered head spliced seamlessly; a stamped
          // restart below the cursor was swallowed as resent).
          // NOTE the completion claim is adopted AFTER the keyed visit
          // resolves (merged_bug_063): a final stamped by a DIFFERENT
          // execution carries a claim minted against the old numbering
          // — it must never finish this tab.
          let relation: CursorKey =
            chunk.execId === '' || chunk.execId === lastExecId
              ? 'matches'
              : lastExecId === ''
                ? 'unkeyed'
                : 'differs';
          // The keyed visit may demand a re-visit of the SAME chunk
          // after the switch arm resets the floor.
          keyed_visit: for (;;) {
            const keyed = visitChunkKeyed(
              relation,
              cursor,
              chunk.firstLineNumber,
              BigInt(chunk.lines.length),
            );
            if (keyed.kind === 'execSwitch') {
              // A retry on another worker: numbering restarted. An
              // explicit switch row — never a seamless splice or a
              // silent swallow — then the cursor lives in the new
              // execution's numbering, and the OLD execution's
              // completion claim is dead (merged_bug_063).
              push(execSwitchRow(chunk.execId));
              cursor = 0n;
              lastExecId = chunk.execId;
              servedComplete = false;
              // merged_bug_014's defensive rider (R10, load-bearing
              // since the terminality oracle went live in
              // merged_bug_134): an armed grace deadline belongs to
              // the OLD execution's world — re-derive it, or the new
              // execution's drain is cut at a deadline armed before
              // the switch existed. The loop head re-arms a FRESH
              // window from the live oracle.
              graceDeadline = null;
              if (chunk.firstLineNumber === 0n && chunk.lines.length > 0) {
                // The switching chunk IS the new execution's head:
                // nothing was filtered server-side. Recover in-stream
                // by re-visiting the same chunk against the fresh
                // floor (the cursor is keyed to the new execution
                // now — the relation is a plain match).
                relation = 'matches';
                continue keyed_visit;
              }
              // The switching message starts past zero (or is a
              // zero-line final): the span [0, firstLineNumber) was
              // filtered SERVER-SIDE against the stale watermark and
              // cannot arrive on this attempt. Cut it; the re-open
              // goes out at the reset watermark — a sinceLine is only
              // ever sent for the execution it was minted in.
              execSwitched = true;
              attempt.abort();
              break stream_loop;
            }
            const visit = keyed.visit;
            if (lastExecId === '' && chunk.execId !== '') {
              // The kernel approved this stamped chunk for the
              // unkeyed cursor (continuity proven, or nothing
              // served): adopt the key HERE, on the kernel's verdict
              // — never before it (bug_050).
              lastExecId = chunk.execId;
            }
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
                // seamless splice. (gapCount derives from the pushed
                // row — no counter to maintain, bug_169.)
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
            if (visit.kind !== 'skip' && graceDeadline !== null) {
              // merged_bug_035: the post-terminal grace bounds QUIET
              // time, not total transfer — every productive verdict
              // re-arms the window. `skip` (keep-alive / fully-resent)
              // never extends: unproductive floods expire on schedule.
              graceDeadline = Date.now() + GRACE_MS;
            }
            break;
          }
          // Adopt the completion claim only for a chunk consumed in
          // the cursor's own numbering (merged_bug_063) — the switch
          // arm above either re-floored before this point or cut the
          // attempt entirely.
          servedComplete = chunk.isComplete;
          applyCap();
          if (chunk.isComplete) {
            // The store stamps is_complete when the EXECUTION is
            // terminal and the manifest contiguously covers its log —
            // a per-execution predicate, not the derivation's end
            // (merged_bug_063). End the attempt and decide on the
            // shared tail_next path like every other stream end: with
            // the derivation terminal (or no oracle) this exits
            // complete exactly as before; with a live oracle saying
            // non-terminal it RE-OPENS and follows the retry, the
            // gateway relay's behavior.
            break stream_loop;
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
        if (graceCutoff || execSwitched) {
          // The grace clock cut a quiet open stream, or the switch arm
          // cut a server-filtered view (merged_bug_063): decide on the
          // SAME path as a natural end — the grace case exits via
          // graceExpired, the switch case re-opens at the reset
          // watermark (servedComplete was reset with the cursor).
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
      // r[impl obs.log.incomplete-surfaced+2]
      // The single exit epilogue: every law-decided exit (and the
      // structurally-futile-reopen finalize below) lands here. Exit
      // with the store never having stamped completion flags the
      // banner — the missing tail is usually the build error itself.
      // A hard-down store with nothing rendered also surfaces the
      // last transport error.
      const finishExit = (exitCause: TailStopCause): void => {
        incomplete = !servedComplete;
        if (exitCause === 'authRequired') {
          // The terminal auth-required surface: the viewer renders the
          // sign-in notice, not the incomplete-log banner heuristics.
          authRequired = true;
          err = lastErr;
          phase = { kind: 'authRequired', err: lastErr };
        } else if (exitCause === 'permanentErr') {
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
      };
      const terminal = effTerminal();
      if (!terminal && graceDeadline !== null) {
        // Same self-clearing edge as the loop head (merged_bug_134):
        // an attempt that ends after the oracle retreated must not
        // exit on a deadline minted for the old terminal world.
        graceDeadline = null;
      }
      if (terminal && graceDeadline === null) {
        graceDeadline = Date.now() + GRACE_MS;
      }
      const graceExpired = graceDeadline !== null && Date.now() >= graceDeadline;
      const decision = tailNext(cause, mode, terminal, graceExpired, servedComplete);
      if (decision.kind === 'exit') {
        finishExit(cause);
        return;
      }
      if (graceDeadline !== null && graceDeadline - Date.now() <= DRAIN_MARGIN_MS) {
        // merged_bug_035: the law said re-open, but the remaining
        // grace cannot fund a drain attempt — the sleep would wake at
        // the deadline and the next open would be head-cut before a
        // single message. Finalize at the decision point: same exit
        // the law would reach one wasted RPC later. (The claim is NOT
        // consumed — no retry is being followed.)
        finishExit(cause);
        return;
      }
      // The law's next-state: following past a stamped completion
      // consumed the claim (merged_bug_029) — the re-open must not
      // carry a dead execution's word into the retry's tab.
      servedComplete = decision.servedComplete;
      // Re-open after the pacer's delay, capped so the next drain
      // attempt lands a full margin BEFORE the deadline rather than
      // sleeping into it (merged_bug_035).
      let delay = pacer.nextDelayMs();
      if (graceDeadline !== null) {
        delay = Math.min(
          delay,
          Math.max(0, graceDeadline - Date.now() - DRAIN_MARGIN_MS),
        );
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
