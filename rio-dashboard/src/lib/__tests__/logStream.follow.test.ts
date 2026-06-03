// The follow-mode battery (merged_bug_306 dashboard half + bug_311):
// the stream must request follow:true, render forward jumps as explicit
// gap rows, dedup resent prefixes through the shared cursor, reconnect
// at the watermark when a stream ends early, and exit only by the
// tail_next law (isComplete immediately; otherwise terminal + armed-once
// grace). Uses fake timers — the reconnect loop sleeps through
// setTimeout and reads Date.now(), both faked.
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

async function flush(rounds = 4): Promise<void> {
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

  it('terminal_and_incomplete_exits_after_grace: armed-once grace bounds the drain, exit flags incomplete', async () => {
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

  it('isComplete_exits_immediately: the final complete chunk ends the stream with no further opens', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk(['l0', 'l1'], { isComplete: true });
    });
    const s = createLogStream(undefined, '', { isTerminal: () => false });
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
});
