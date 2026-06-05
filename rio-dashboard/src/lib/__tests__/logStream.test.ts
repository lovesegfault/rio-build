// r[verify dash.stream.log-tail+4]
// Runes-in-module store driven by a mocked async-generator RPC. The
// `.svelte.ts` compile pass means `$state` inside createLogStream works
// under vitest too (the Svelte vite plugin handles the transform).
//
// Reading the returned getters bare — outside a component — works
// because we're not testing reactivity tracking, just the value
// progression as the generator drains. Each yield lands on the next
// microtask; the manually-driven iterator (merged_bug_254) adds a race
// and a tick per message, so each chunk costs a few more turns than the
// old for-await — flush() budgets generously.
//
// The follow/reconnect/gap battery lives in logStream.follow.test.ts
// (fake timers); this file pins the timer-free basics: accumulation,
// lossy decode, request shape, abort hygiene, completion semantics.
import { afterEach, describe, expect, it, vi } from 'vitest';

const { tailLog } = vi.hoisted(() => ({ tailLog: vi.fn() }));
vi.mock('../../api/logs', () => ({ logs: { tailLog } }));

import { createLogStream } from '../logStream.svelte';

// Small helper: flush N microtask turns so the for-await body can
// observe each yield. Two turns per chunk — one for the generator's
// resume, one for the loop body's state assignment to settle. Mirrors
// the GC test's drain loop; extracted here since every assertion needs
// it and the arithmetic is easy to botch inline (consol-mc185: the
// tick/Promise boilerplate is copy-pasted across five test files — a
// shared flushSvelte() helper is queued, this is the local first step).
async function flush(rounds = 2): Promise<void> {
  // Macrotask rounds: each setTimeout(0) hop lets the ENTIRE pending
  // microtask chain drain (the manually-driven iterator races a tick
  // per message, so its chains are deeper than the old for-await's —
  // counting microtasks made the budget depth-coupled).
  for (let i = 0; i < rounds; i++) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

function u8(...bytes: number[]): Uint8Array {
  return Uint8Array.from(bytes);
}

// Structural-shape fixture for TailLogChunk. The generated type is a
// branded Message<...> intersection; tests only hit the iteration path
// so a plain object matching the field layout is sufficient. Chunks
// thread firstLineNumber: the cursor dedups by line number, so a second
// chunk re-claiming line 0 is (correctly) skipped as resent.
function chunk(lines: Uint8Array[], isComplete = false, firstLineNumber = 0n) {
  return {
    execId: '',
    lines,
    firstLineNumber,
    isComplete,
  };
}

function texts(s: {
  rows: readonly { kind: string; text: string }[];
}): string[] {
  return s.rows.filter((r) => r.kind === 'line').map((r) => r.text);
}

describe('createLogStream', () => {
  afterEach(() => {
    tailLog.mockReset();
  });

  it('accumulates rows across chunks and flips done on isComplete', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk([u8(0x68, 0x65, 0x6c, 0x6c, 0x6f)]); // "hello"
      yield chunk(
        [u8(0x77, 0x6f, 0x72, 0x6c, 0x64), u8(0x21)], // "world", "!"
        true,
        1n,
      );
    });

    const s = createLogStream();
    expect(s.rows).toEqual([]);
    expect(s.done).toBe(false);

    await flush(3);

    expect(texts(s)).toEqual(['hello', 'world', '!']);
    expect(s.gapCount).toBe(0);
    expect(s.done).toBe(true);
    // Final log (isComplete chunk seen) → no incomplete banner.
    expect(s.incomplete).toBe(false);
    expect(s.err).toBeNull();

    // RPC was called with the expected request shape; sinceLine is the
    // bigint zero so we check the field directly rather than .toEqual
    // (bigint equality in nested objects has been flaky across vitest
    // minors — the point is "we sent 0n, not undefined").
    const [req, opts] = tailLog.mock.calls[0];
    // buildId is no longer part of the request — storage is keyed by
    // (drv_hash, exec_id); execId empty resolves to the latest execution.
    expect(req.execId).toBe('');
    expect(req.derivation).toBe('');
    expect(req.sinceLine).toBe(0n);
    // Live follow (merged_bug_306/bug_311): the stream re-opens at the
    // watermark on premature end instead of freezing on a one-shot
    // snapshot — the reconnect battery is logStream.follow.test.ts.
    expect(req.follow).toBe(true);
    expect(opts.signal).toBeInstanceOf(AbortSignal);
  });

  it('lossily decodes non-UTF-8 bytes to U+FFFD without throwing (R8)', async () => {
    // 0x48 0x69 = "Hi". 0xff 0xfe 0x21 = two invalid continuation-less
    // high bytes followed by "!". With {fatal:false} the decoder emits
    // U+FFFD per invalid sequence rather than throwing a TypeError.
    tailLog.mockImplementation(async function* () {
      yield chunk([u8(0x48, 0x69), u8(0xff, 0xfe, 0x21)], true);
    });

    const s = createLogStream();
    await flush(2);

    // No throw — err stays null, done flips.
    expect(s.err).toBeNull();
    expect(s.done).toBe(true);

    const lines = texts(s);
    expect(lines).toHaveLength(2);
    expect(lines[0]).toBe('Hi');
    // The invalid bytes became replacement characters. We assert on
    // inclusion rather than exact count — different JS engines emit
    // one U+FFFD per byte vs per maximal-subpart (both are
    // spec-conformant for WHATWG TextDecoder).
    expect(lines[1]).toContain('�');
    expect(lines[1].endsWith('!')).toBe(true);
  });

  it('keeps streaming when the generator exhausts on a running build', async () => {
    // Server closed the stream early (session rotation, store restart)
    // while the build still runs. Pre-fix this froze the tab as
    // done+incomplete; now the loop schedules a re-open at the
    // watermark and the spinner stays honest. (The reconnect itself is
    // driven under fake timers in logStream.follow.test.ts — here we
    // pin only "not done, not incomplete, no error".)
    tailLog.mockImplementation(async function* () {
      yield chunk([u8(0x7a)]);
      // No isComplete chunk, just end.
    });

    const s = createLogStream();
    await flush(3);

    expect(texts(s)).toEqual(['z']);
    expect(s.done).toBe(false);
    expect(s.incomplete).toBe(false);
    expect(s.err).toBeNull();
    s.destroy();
  });

  it('destroy() flips the AbortSignal and swallows the resulting error', async () => {
    let seenSignal: AbortSignal | undefined;
    tailLog.mockImplementation(async function* (
      _req: unknown,
      opts: { signal?: AbortSignal },
    ) {
      seenSignal = opts.signal;
      yield chunk([u8(0x78)]); // "x"
      // Park until destroy() aborts; then throw so the catch arm runs.
      // A bare `new Promise(() => {})` would never settle — abort()
      // flips the signal but doesn't wake a parked promise — and the
      // `s.err === null` assert below would pass vacuously (catch
      // never reached, err never assigned).
      await new Promise((_, rej) =>
        opts.signal?.addEventListener('abort', () =>
          rej(new DOMException('aborted', 'AbortError')),
        ),
      );
    });

    const s = createLogStream();
    await flush(2);
    expect(texts(s)).toEqual(['x']);
    expect(seenSignal?.aborted).toBe(false);

    s.destroy();
    await flush(1);

    expect(seenSignal?.aborted).toBe(true);
    // The catch arm checks ctrl.signal.aborted before assigning err, so
    // our own abort never shows up as a user-facing error. If this
    // assertion ever fails the viewer would flash "AbortError" every
    // time the drawer closed.
    expect(s.err).toBeNull();
    // done flips in the catch arm — structurally proves the abort
    // actually drove execution through the catch block (a parked-forever
    // mock would leave done false and err null for the wrong reason).
    expect(s.done).toBe(true);
  });

  it('passes drvPath through when provided', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk([], true);
    });

    createLogStream('/nix/store/xyz-foo.drv');
    await flush(2);

    const [req] = tailLog.mock.calls[0];
    expect(req.derivation).toBe('/nix/store/xyz-foo.drv');
    // No execId passed → empty string sent → server resolves the
    // latest execution for the drv. The dashboard's "approximate"
    // banner is the LogViewer's concern, not this store's.
    expect(req.execId).toBe('');
  });

  it('passes execId through when provided', async () => {
    tailLog.mockImplementation(async function* () {
      yield chunk([], true);
    });

    // execId is the per-build observation from GraphNode.exec_id —
    // pins the EXACT execution this build observed (not the latest
    // for the drv across all builds).
    createLogStream('/nix/store/xyz-foo.drv', '01976e8b-test-exec');
    await flush(2);

    const [req] = tailLog.mock.calls[0];
    expect(req.derivation).toBe('/nix/store/xyz-foo.drv');
    expect(req.execId).toBe('01976e8b-test-exec');
  });
});
