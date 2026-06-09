// The follow-mode battery (merged_bug_306 dashboard half + bug_311):
// the stream must request follow:true, render forward jumps as explicit
// gap rows, dedup resent prefixes through the shared cursor, reconnect
// at the watermark when a stream ends early, and exit only by the
// tail_next law (isComplete immediately; otherwise terminal + the
// quiet-time grace -- productive serves re-arm it, merged_bug_035). Uses fake timers — the reconnect loop sleeps through
// setTimeout and reads Date.now(), both faked.
import { Code, ConnectError } from '@connectrpc/connect';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { tailLog } = vi.hoisted(() => ({ tailLog: vi.fn() }));
vi.mock('../../api/logs', () => ({ logs: { tailLog } }));

import { createLogStream } from '../logStream.svelte';

function u8s(s: string): Uint8Array {
  return new TextEncoder().encode(s);
}

function chunk(
  lines: string[],
  opts: { isComplete?: boolean; firstLineNumber?: bigint } = {},
) {
  return {
    execId: '',
    lines: lines.map(u8s),
    firstLineNumber: opts.firstLineNumber ?? 0n,
    isComplete: opts.isComplete ?? false,
  };
}

async function flush(rounds = 12): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await Promise.resolve();
    await Promise.resolve();
  }
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  tailLog.mockReset();
});

describe('createLogStream follow mode', () => {
  // r[verify dash.stream.log-tail+6]
  /// merged_bug_254's recorded red: a NEVER-ENDING quiet stream on a
  /// terminal build. The pre-fix `for await` parked inside the
  /// iterator — `done` stayed false forever (the red observed exactly
  /// that after advancing the clock past every budget). The
  /// manually-driven iterator's 1 s tick arms the grace clock
  /// mid-stream and finalizes through the same tail_next path.
  it('never_ending_stream_finalizes_at_grace: terminal + quiet open stream exits', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0', 'l1']);
      // The stream stays open forever and ignores its signal.
      await new Promise(() => {});
    });
    const s = createLogStream('/nix/store/x.drv', '', { isTerminal: () => true });
    await vi.advanceTimersByTimeAsync(10);
    expect(s.rows.length).toBe(2);
    expect(s.done).toBe(false);
    // Tick 1: observes terminal, arms the 5 s grace. Then ride past it.
    await vi.advanceTimersByTimeAsync(1_000);
    await vi.advanceTimersByTimeAsync(6_000);
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(true);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// bug_145's absolute-timer discipline, RE-AIMED by merged_bug_035:
  /// the original test pinned "a chatty terminal stream exits at +5 s"
  /// — but the grace is now a QUIET-TIME budget, so productive serves
  /// legitimately re-arm it and a terminal build's long drain streams
  /// to the end (the skip_flood_still_expires test below carries the
  /// starvation-immunity duty bug_145 cared about: unproductive >1
  /// msg/sec traffic still cannot outrun the absolute deadline). This
  /// test pins the inverted polarity: continuous PRODUCTIVE serves on
  /// a terminal build keep the stream alive — the drain is never cut
  /// mid-transfer by a clock that pre-dates it.
  it('chatty_productive_terminal_stream_keeps_draining: serves re-arm the quiet-time budget', async () => {
    let line = 0;
    tailLog.mockImplementation(async function* () {
      for (;;) {
        yield chunk([`l${line}`], { firstLineNumber: BigInt(line) });
        line += 1;
        // 200 ms between chunks — five times faster than the 1 s tick.
        await new Promise((r) => setTimeout(r, 200));
      }
    });
    const s = createLogStream('/nix/store/x.drv', '', {
      isTerminal: () => true,
    });
    await vi.advanceTimersByTimeAsync(20_000);
    // Still draining at +20 s: every serve re-armed the window.
    expect(s.done).toBe(false);
    expect(s.rows.length).toBeGreaterThan(50);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_063's recorded red (scenario A): exec A serves lines,
  /// then the worker dies; the reconnect carries A's watermark, the
  /// store resolves latest=exec-b (sealed shorter) and filters every
  /// line server-side — the only message is a 0-line final stamped
  /// exec-b whose stale cursor arithmetic claims complete. Pre-fix the
  /// tab finished with B's entire log swallowed (done=true, no b rows,
  /// no re-open at 0). The switch now resets the served-complete claim
  /// and re-opens at sinceLine 0 in the new numbering.
  it('retry_log_recovered_after_stale_filtered_reconnect: switch on a filtered view re-opens at line 0', async () => {
    const requests: { sinceLine: bigint }[] = [];
    tailLog
      .mockImplementationOnce(async function* (req: { sinceLine: bigint }) {
        requests.push(req);
        yield { ...chunk(['a0', 'a1']), execId: 'exec-a' };
        throw new ConnectError('worker died', Code.Unavailable);
      })
      .mockImplementationOnce(async function* (req: { sinceLine: bigint }) {
        requests.push(req);
        // Latest resolved to exec-b; everything below the stale
        // watermark was filtered server-side: a 0-line final whose
        // first_line_number is the server's own (stale) cursor claim.
        yield {
          ...chunk([], { isComplete: true, firstLineNumber: 2n }),
          execId: 'exec-b',
        };
      })
      .mockImplementationOnce(async function* (req: { sinceLine: bigint }) {
        requests.push(req);
        yield { ...chunk(['b0', 'b1'], { isComplete: true }), execId: 'exec-b' };
      });
    const s = createLogStream('/nix/store/x.drv');
    await vi.advanceTimersByTimeAsync(30_000);
    expect(s.done).toBe(true);
    expect(
      s.rows.filter((r) => r.kind === 'line').map((r) => r.text),
    ).toEqual(['a0', 'a1', 'b0', 'b1']);
    expect(s.rows.some((r) => r.kind === 'execSwitch')).toBe(true);
    // The post-switch re-open went back to line 0 in the NEW numbering
    // — a sinceLine is only ever sent for the exec it was minted in.
    expect(requests.length).toBe(3);
    expect(requests[2].sinceLine).toBe(0n);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_063's law half: the in-loop isComplete exit skipped the
  /// terminal && servedComplete conjunct of the mirrored tail_next law —
  /// a complete-but-FAILED attempt on a non-terminal derivation must
  /// re-open and follow the retry (the gateway relay's behavior). The
  /// pre-fix arm finished the tab on the failed attempt's log.
  it('complete_final_on_nonterminal_drv_reopens: exec-level completion does not end a live follow', async () => {
    let opens = 0;
    tailLog.mockImplementation(async function* () {
      opens += 1;
      yield { ...chunk(['a0'], { isComplete: true }), execId: 'exec-a' };
    });
    const s = createLogStream('/nix/store/x.drv', '', {
      isTerminal: () => false,
    });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(s.done).toBe(false);
    expect(opens).toBeGreaterThan(1);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_002's recorded red: a retry on another worker restarts
  /// numbering at zero. Pre-fix the new execution's chunk was
  /// indistinguishable from a resent duplicate (`skip`) — the red
  /// observed rows ['a0','a1'] with the b-lines silently swallowed and
  /// no disclosure. The keyed visit forces the switch arm: an explicit
  /// execSwitch row, cursor reset, then the new execution's lines.
  it('exec_switch_disclosed_not_spliced: a new execution resets the cursor with a marked row', async () => {
    tailLog.mockImplementation(async function* () {
      yield { ...chunk(['a0', 'a1']), execId: 'exec-a' };
      yield { ...chunk(['b0', 'b1'], { isComplete: true }), execId: 'exec-b' };
    });
    const s = createLogStream('/nix/store/x.drv');
    await vi.advanceTimersByTimeAsync(10);
    expect(s.rows.map((r) => r.kind)).toEqual([
      'line',
      'line',
      'execSwitch',
      'line',
      'line',
    ]);
    expect(s.rows[2].text).toBe('exec-b');
    expect(s.rows.filter((r) => r.kind === 'line').map((r) => r.text)).toEqual([
      'a0',
      'a1',
      'b0',
      'b1',
    ]);
    expect(s.done).toBe(true);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_164's reader half (recorded red: pre-fix the
  /// `x-rio-log-unservable` refusal classified as transportErr and the
  /// loop re-dialed — tailLog call count climbed past 1). A
  /// typed-permanent refusal exits after ONE open: re-dialing cannot
  /// heal what the store typed as forever.
  it('permanent_unservable_exits_terminally: x-rio-log-unservable is never re-dialed', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0']);
      throw new ConnectError(
        'chunk is permanently unservable',
        Code.Internal,
        new Headers({ 'x-rio-log-unservable': 'short_object' }),
      );
    });
    const s = createLogStream('/nix/store/x.drv');
    await vi.advanceTimersByTimeAsync(10);
    // Ride well past several pacer delays: a re-dialing loop would
    // re-open within ~250-2000 ms.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(tailLog).toHaveBeenCalledTimes(1);
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(true);
    expect(s.err).not.toBeNull();
    s.destroy();
  });

  // r[verify dash.stream.idle-timeout+3]
  // (No idle timer of our own on an open stream: the loop only acts on
  // stream end/error — asserted here by the request shape and the
  // reconnect tests below, which drive the loop solely through
  // generator lifecycle events.)
  it('follow_true_requested: opens the tail with follow:true from line 0', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['a'], { isComplete: true });
    });
    const s = createLogStream('/nix/store/x.drv', 'exec-1');
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(1);
    const req = tailLog.mock.calls[0][0];
    expect(req.follow).toBe(true);
    expect(req.sinceLine).toBe(0n);
    expect(s.done).toBe(true);
    s.destroy();
  });

  it('gap_chunk_pushes_gap_row: a forward jump renders an explicit gap row then the lines', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0', 'l1']);
      // Lines 2..10 lost (fan-out drop / hand-deleted chunk): the next
      // chunk starts at 10.
      yield chunk(['l10', 'l11'], { firstLineNumber: 10n, isComplete: true });
    });
    const s = createLogStream();
    await flush();
    expect(s.rows.map((r) => r.kind)).toEqual([
      'line',
      'line',
      'gap',
      'line',
      'line',
    ]);
    const gap = s.rows[2];
    expect(gap.from).toBe(2n);
    expect(gap.until).toBe(10n);
    expect(s.gapCount).toBe(1);
    expect(s.rows[3].text).toBe('l10');
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(false);
    s.destroy();
  });

  // r[verify store.log.tail-reconnect]
  it('resent_prefix_skipped: lines below the watermark are never rendered twice', async () => {
    tailLog.mockImplementationOnce(async function* () {
      yield chunk(['l0', 'l1', 'l2']);
      // Stream dies (transport error mid-follow).
      throw new Error('h2 stream reset');
    });
    tailLog.mockImplementationOnce(async function* (req: { sinceLine: bigint }) {
      // The server resends an overlapping chunk starting below the
      // requested watermark (store.log.tail-reconnect allows overlap;
      // the client MUST dedup).
      void req;
      yield chunk(['l1', 'l2', 'l3'], { firstLineNumber: 1n, isComplete: true });
    });
    const s = createLogStream();
    await flush();
    // Cross the reconnect backoff.
    await vi.advanceTimersByTimeAsync(300);
    await flush();
    expect(s.rows.filter((r) => r.kind === 'line').map((r) => r.text)).toEqual([
      'l0',
      'l1',
      'l2',
      'l3',
    ]);
    expect(s.gapCount).toBe(0);
    expect(s.done).toBe(true);
    s.destroy();
  });

  it('reconnects_on_generator_end_with_sinceLine: a non-terminal natural end re-opens at the watermark', async () => {
    tailLog.mockImplementationOnce(async function* () {
      yield chunk(['l0', 'l1']);
      // Generator exhausts without isComplete: the serving session
      // closed while the build still runs.
    });
    tailLog.mockImplementationOnce(async function* () {
      yield chunk(['l2'], { firstLineNumber: 2n, isComplete: true });
    });
    const s = createLogStream();
    await flush();
    // Pre-fix the stream flipped done+incomplete here instead of
    // re-opening — the frozen Logs tab.
    expect(s.done).toBe(false);
    await vi.advanceTimersByTimeAsync(300);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(2);
    expect(tailLog.mock.calls[1][0].sinceLine).toBe(2n);
    expect(s.rows.map((r) => r.text)).toEqual(['l0', 'l1', 'l2']);
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(false);
    s.destroy();
  });

  it('terminal_and_incomplete_exits_after_grace: quiet grace bounds the stall, exit flags incomplete', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0']);
      // Natural end, never complete: the final lines never reached the
      // store.
    });
    const s = createLogStream('/nix/store/x.drv', 'exec-1', {
      isTerminal: () => true,
    });
    await flush();
    // Grace armed at the first post-terminal decision; the loop keeps
    // re-opening within grace…
    expect(s.done).toBe(false);
    // …and exits once the grace deadline passes.
    await vi.advanceTimersByTimeAsync(10_000);
    await flush();
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(true);
    expect(tailLog.mock.calls.length).toBeGreaterThan(1);
    s.destroy();
  });

  /// merged_bug_063 updated this contract: is_complete is a
  /// per-EXECUTION predicate, so a complete final ends the stream
  /// immediately only when the derivation-level conjunct holds — the
  /// oracle says terminal, or no oracle was supplied (the store's
  /// exec-level claim is then the best available stand-in, the legacy
  /// contract). The non-terminal-oracle case now RE-OPENS to follow
  /// the retry (`complete_final_on_nonterminal_drv_reopens` above).
  it('isComplete_exits_immediately: the final complete chunk ends a terminal stream with no further opens', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0', 'l1'], { isComplete: true });
    });
    const s = createLogStream(undefined, '', { isTerminal: () => true });
    await flush();
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(false);
    await vi.advanceTimersByTimeAsync(5_000);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(1);
    s.destroy();
  });

  it('isComplete_exits_immediately_without_oracle: the legacy no-closure contract is preserved', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0', 'l1'], { isComplete: true });
    });
    const s = createLogStream(undefined, '');
    await flush();
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(false);
    await vi.advanceTimersByTimeAsync(5_000);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(1);
    s.destroy();
  });

  it('notfound_before_first_chunk_is_retryable: the exec may not exist yet', async () => {
    const { ConnectError, Code } = await import('@connectrpc/connect');
    // The open itself fails: a yield before the throw would deliver a
    // chunk and defeat the pre-first-chunk premise this test pins.
    // eslint-disable-next-line require-yield
    tailLog.mockImplementationOnce(async function* () {
      throw new ConnectError('no manifest yet', Code.NotFound);
    });
    tailLog.mockImplementationOnce(async function* () {
      yield chunk(['l0'], { isComplete: true });
    });
    const s = createLogStream();
    await flush();
    expect(s.err).toBeNull();
    expect(s.done).toBe(false);
    await vi.advanceTimersByTimeAsync(300);
    await flush();
    expect(s.rows.map((r) => r.text)).toEqual(['l0']);
    expect(s.done).toBe(true);
    s.destroy();
  });

  // r[verify dash.stream.reopen-pacing]
  // THE FALSIFY TWIN for merged_bug_054: under keep-alive-only
  // sessions (a follow with no live ingest ends immediately after one
  // zero-line final — receipt without progress), the re-open ladder
  // MUST escalate 250 -> 500 -> 1000 -> 2000. Pre-fix, the loop reset
  // its backoff on receivedThisAttempt, so this exact scenario
  // re-opened at a flat 250 ms (~4 Hz) forever — this test recorded
  // that red (extra opens at the 500/1000 boundaries) before the
  // ReopenPacer rewire and stays as the regression pin.
  it('reopen_pacing_escalates: keep-alive receipts do not reset the ladder', async () => {
    tailLog.mockImplementation(async function* () {
      // One zero-line keep-alive/final chunk, then natural end: the
      // store's always-final arm for a session with no live ingest.
      yield chunk([], { firstLineNumber: 0n });
    });
    // Never terminal: no grace window, the loop re-opens indefinitely
    // (bounded here by destroy()).
    const s = createLogStream('/nix/store/x.drv', 'exec-1', {
      isTerminal: () => false,
    });
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(1);

    // 250 ms: second open.
    await vi.advanceTimersByTimeAsync(250);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(2);

    // +250 ms: the pre-fix loop opened AGAIN here (flat ladder). The
    // pacer is now at 500 ms, so NO new open yet.
    await vi.advanceTimersByTimeAsync(250);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(2);

    // +250 ms (=500 since open 2): third open.
    await vi.advanceTimersByTimeAsync(250);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(3);

    // 1000 ms rung: nothing at +999, the fourth open at +1000.
    await vi.advanceTimersByTimeAsync(999);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(3);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(4);

    // 2000 ms cap from here on.
    await vi.advanceTimersByTimeAsync(1999);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(4);
    await vi.advanceTimersByTimeAsync(1);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(5);
    await vi.advanceTimersByTimeAsync(2000);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(6);

    // A productive serve resets the ladder back to 250 ms.
    tailLog.mockImplementationOnce(async function* () {
      yield chunk(['l0']);
    });
    await vi.advanceTimersByTimeAsync(2000);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(7);
    await vi.advanceTimersByTimeAsync(250);
    await flush();
    expect(tailLog).toHaveBeenCalledTimes(8);

    s.destroy();
  });

});

// r[verify store.log.consumer-registry]
// merged_bug_108: the dashboard is registry-declared KeylessOnly (owner
// decision Q1, 2026-06-04, extending bug_290: tenant JWT + ownership,
// no service bypass — and no dashboard credential funded this wave).
// When the store demands credentials (jwt-enabled deployment), the
// stream MUST end in the terminal `authRequired` state — no retry
// ladder: an auth failure does not heal by reconnecting, and the
// pre-fix loop classified it `openFailed` and polled the store forever.
//
// RED (pre-fix): `done` stayed false through the whole ladder (the
// loop re-opened on Unauthenticated exactly like a transient).
describe('authRequired terminal state', () => {
  it('unauthenticated ends the stream terminally, no retry', async () => {
    const { ConnectError, Code } = await import('@connectrpc/connect');
    // The deny happens at the open — a yield before the throw would
    // deliver a chunk and defeat the auth-denied premise.
    // eslint-disable-next-line require-yield
    tailLog.mockImplementation(async function* () {
      throw new ConnectError('tenant token required', Code.Unauthenticated);
    });
    const s = createLogStream('/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv');
    await flush(2);
    expect(s.done).toBe(true);
    expect(s.authRequired).toBe(true);
    expect(s.err).not.toBeNull();
    // No second open: the first verdict is terminal.
    expect(tailLog).toHaveBeenCalledTimes(1);
    s.destroy();
  });

  it('permission-denied is the same terminal cause', async () => {
    const { ConnectError, Code } = await import('@connectrpc/connect');
    // eslint-disable-next-line require-yield
    tailLog.mockImplementation(async function* () {
      throw new ConnectError('not yours', Code.PermissionDenied);
    });
    const s = createLogStream('/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv');
    await flush(2);
    expect(s.done).toBe(true);
    expect(s.authRequired).toBe(true);
    expect(tailLog).toHaveBeenCalledTimes(1);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// bug_348's recorded red: a PINNED stream (non-empty execId) can
  /// structurally never observe a retry — every re-open resends the
  /// pinned id — yet the exit law treated a store-stamped completion
  /// on a non-terminal oracle as "follow the retry". The pre-fix loop
  /// re-dialed skip→isComplete→reopen at the pacer cap forever
  /// (done=false, opens climbing), a streaming spinner over a provably
  /// complete log. The resolution mode is now a law input: pinned +
  /// servedComplete is terminal by construction.
  it('pinned_complete_exits_despite_nonterminal_oracle: a pinned stream cannot follow a retry', async () => {
    let opens = 0;
    tailLog.mockImplementation(async function* () {
      opens += 1;
      yield { ...chunk(['a0'], { isComplete: true }), execId: 'exec-a' };
    });
    const s = createLogStream('/nix/store/x.drv', 'exec-a', {
      isTerminal: () => false,
    });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(s.done).toBe(true);
    expect(opens).toBe(1);
    expect(s.incomplete).toBe(false);
    expect(s.phase).toEqual({ kind: 'complete' });
    s.destroy();
  });


  // r[verify dash.stream.log-tail+6]
  /// merged_bug_029's recorded red: servedComplete is per-execution,
  /// but its only reset was the execSwitch arm -- which requires a
  /// chunk stamped with the NEW execId to actually arrive. On the
  /// follow-the-retry reopen (exec A's isComplete + non-terminal
  /// oracle), A's completion claim survived; when exec B never
  /// delivered a chunk, the grace-expiry exit computed
  /// incomplete = !servedComplete = false and minted phase 'complete'
  /// from the DEAD execution's claim. The decision to follow the retry
  /// now structurally consumes the claim (tailNext returns the next
  /// servedComplete state).
  it('reopen_consumes_completion_claim: a dead exec claim cannot finish the retry tab', async () => {
    let term = false;
    tailLog
      .mockImplementationOnce(async function* () {
        yield { ...chunk(['a0'], { isComplete: true }), execId: 'exec-a' };
      })
      .mockImplementation(async function* () {
        // exec B: manifest lags / store outage -- no chunk, ever.
        await new Promise(() => {});
      });
    const s = createLogStream('/nix/store/x.drv', '', {
      isTerminal: () => term,
    });
    await vi.advanceTimersByTimeAsync(10);
    // The derivation settles terminal AFTER the reopen decision.
    term = true;
    await vi.advanceTimersByTimeAsync(10_000);
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(true);
    expect(s.phase).toEqual({ kind: 'incomplete', err: null });
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_014's defensive rider (R10, load-bearing once the
  /// terminality oracle went live in merged_bug_134): an armed grace
  /// deadline belongs to the OLD execution's world -- an execution
  /// switch must re-derive it, or the new execution's drain is cut at
  /// a deadline armed before the switch existed. Pre-rider red: b2
  /// (arriving 6s after open, 2s past the stale deadline) was cut.
  it('exec_switch_rearms_grace: the new execution gets a fresh grace window', async () => {
    tailLog.mockImplementation(async function* () {
      yield { ...chunk(['a0']), execId: 'exec-a' };
      await new Promise((r) => setTimeout(r, 4_000));
      // Retry on another worker; head at 0 -> in-stream recovery.
      yield { ...chunk(['b0', 'b1']), execId: 'exec-b' };
      await new Promise((r) => setTimeout(r, 2_000));
      yield { ...chunk(['b2'], { firstLineNumber: 2n }), execId: 'exec-b' };
      await new Promise(() => {});
    });
    const s = createLogStream('/nix/store/x.drv', '', {
      isTerminal: () => true,
    });
    await vi.advanceTimersByTimeAsync(20_000);
    expect(s.done).toBe(true);
    expect(
      s.rows.filter((r) => r.kind === 'line').map((r) => r.text),
    ).toEqual(['a0', 'b0', 'b1', 'b2']);
    s.destroy();
  });


  // r[verify dash.stream.log-tail+6]
  /// merged_bug_035's recorded red: for a build already terminal when
  /// the tab opens, the grace armed at the first loop head and never
  /// extended on productive serves -- the ENTIRE historical drain had
  /// to finish inside 5s or the head check cut it mid-transfer with a
  /// false "incomplete" banner, deterministically on every refresh.
  /// The grace is a QUIET-TIME budget: productive serves re-arm it.
  it('historical_drain_rearms_grace: a terminal drain longer than one window completes', async () => {
    tailLog.mockImplementation(async function* () {
      for (let i = 0; i < 7; i++) {
        yield chunk([`l${i}`], { firstLineNumber: BigInt(i) });
        await new Promise((r) => setTimeout(r, 1_500));
      }
      yield chunk([], { isComplete: true, firstLineNumber: 7n });
    });
    const s = createLogStream('/nix/store/x.drv', '', {
      isTerminal: () => true,
    });
    await vi.advanceTimersByTimeAsync(15_000);
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(false);
    expect(s.rows.filter((r) => r.kind === 'line')).toHaveLength(7);
    s.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_035's negative control (the starvation polarity that
  /// MUST keep expiring): a flood of unproductive chunks -- resends of
  /// already-served lines, the `skip` verdict -- never re-arms the
  /// window. Only serves extend; noise still expires on schedule.
  it('skip_flood_still_expires: unproductive traffic cannot extend the grace', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0']);
      for (;;) {
        // The same chunk, resent forever: every visit is `skip`.
        yield chunk(['l0']);
        await new Promise((r) => setTimeout(r, 200));
      }
    });
    const s = createLogStream('/nix/store/x.drv', '', {
      isTerminal: () => true,
    });
    await vi.advanceTimersByTimeAsync(20_000);
    expect(s.done).toBe(true);
    expect(s.incomplete).toBe(true);
    s.destroy();
  });

});
