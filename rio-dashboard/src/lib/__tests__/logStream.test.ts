// r[verify dash.stream.log-tail]
// Runes-in-module store driven by a mocked async-generator RPC. The
// `.svelte.ts` compile pass means `$state` inside createLogStream works
// under vitest too (the Svelte vite plugin handles the transform).
//
// Reading the returned getters bare — outside a component — works
// because we're not testing reactivity tracking, just the value
// progression as the generator drains. Each yield lands on the next
// microtask, so `await Promise.resolve()` steps the for-await one chunk.
import { afterEach, describe, expect, it, vi } from 'vitest';

const { getBuildLogs } = vi.hoisted(() => ({ getBuildLogs: vi.fn() }));
vi.mock('../../api/admin', () => ({ admin: { getBuildLogs } }));

import { createLogStream } from '../logStream.svelte';

// Small helper: flush N microtask turns so the for-await body can
// observe each yield. Two turns per chunk — one for the generator's
// resume, one for the loop body's state assignment to settle. Mirrors
// the GC test's drain loop; extracted here since every assertion needs
// it and the arithmetic is easy to botch inline (consol-mc185: the
// tick/Promise boilerplate is copy-pasted across five test files — a
// shared flushSvelte() helper is queued, this is the local first step).
async function flush(rounds = 1): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await Promise.resolve();
    await Promise.resolve();
  }
}

function u8(...bytes: number[]): Uint8Array {
  return Uint8Array.from(bytes);
}

// Structural-shape fixture for BuildLogChunk. The generated type is a
// branded Message<...> intersection; tests only hit the iteration path
// so a plain object matching the field layout is sufficient.
function chunk(lines: Uint8Array[], isComplete = false) {
  return {
    derivationPath: '/nix/store/aaaa-test.drv',
    lines,
    firstLineNumber: 0n,
    isComplete,
  };
}

describe('createLogStream', () => {
  afterEach(() => {
    getBuildLogs.mockReset();
  });

  it('accumulates lines across chunks and flips done on isComplete', async () => {
    getBuildLogs.mockImplementation(async function* () {
      yield chunk([u8(0x68, 0x65, 0x6c, 0x6c, 0x6f)]); // "hello"
      yield chunk(
        [u8(0x77, 0x6f, 0x72, 0x6c, 0x64), u8(0x21)], // "world", "!"
        true,
      );
    });

    const s = createLogStream('build-accum');
    expect(s.lines).toEqual([]);
    expect(s.done).toBe(false);

    await flush(3);

    expect(s.lines).toEqual(['hello', 'world', '!']);
    expect(s.done).toBe(true);
    expect(s.err).toBeNull();

    // RPC was called with the expected request shape; sinceLine is the
    // bigint zero so we check the field directly rather than .toEqual
    // (bigint equality in nested objects has been flaky across vitest
    // minors — the point is "we sent 0n, not undefined").
    const [req, opts] = getBuildLogs.mock.calls[0];
    expect(req.buildId).toBe('build-accum');
    expect(req.derivationPath).toBe('');
    expect(req.sinceLine).toBe(0n);
    expect(opts.signal).toBeInstanceOf(AbortSignal);
  });

  it('lossily decodes non-UTF-8 bytes to U+FFFD without throwing (R8)', async () => {
    // 0x48 0x69 = "Hi". 0xff 0xfe 0x21 = two invalid continuation-less
    // high bytes followed by "!". With {fatal:false} the decoder emits
    // U+FFFD per invalid sequence rather than throwing a TypeError.
    getBuildLogs.mockImplementation(async function* () {
      yield chunk(
        [u8(0x48, 0x69), u8(0xff, 0xfe, 0x21)],
        true,
      );
    });

    const s = createLogStream('build-r8');
    await flush(2);

    // No throw — err stays null, done flips.
    expect(s.err).toBeNull();
    expect(s.done).toBe(true);

    expect(s.lines).toHaveLength(2);
    expect(s.lines[0]).toBe('Hi');
    // The invalid bytes became replacement characters. We assert on
    // inclusion rather than exact count — different JS engines emit
    // one U+FFFD per byte vs per maximal-subpart (both are
    // spec-conformant for WHATWG TextDecoder).
    expect(s.lines[1]).toContain('\ufffd');
    expect(s.lines[1].endsWith('!')).toBe(true);
  });

  it('surfaces transport errors when not self-aborted', async () => {
    getBuildLogs.mockImplementation(async function* () {
      yield chunk([u8(0x61)]);
      throw new Error('upstream reset');
    });

    const s = createLogStream('build-err');
    await flush(3);

    // First chunk landed before the throw — partial output is preserved.
    expect(s.lines).toEqual(['a']);
    expect(s.err?.message).toBe('upstream reset');
    // done flips so the viewer's spinner stops.
    expect(s.done).toBe(true);
  });

  it('destroy() flips the AbortSignal and swallows the resulting error', async () => {
    let seenSignal: AbortSignal | undefined;
    getBuildLogs.mockImplementation(async function* (
      _req: unknown,
      opts: { signal?: AbortSignal },
    ) {
      seenSignal = opts.signal;
      yield chunk([u8(0x78)]); // "x"
      // Park forever — only destroy() can unblock.
      await new Promise(() => {});
    });

    const s = createLogStream('build-abort');
    await flush(2);
    expect(s.lines).toEqual(['x']);
    expect(seenSignal?.aborted).toBe(false);

    s.destroy();
    await flush(1);

    expect(seenSignal?.aborted).toBe(true);
    // The catch arm checks ctrl.signal.aborted before assigning err, so
    // our own abort never shows up as a user-facing error. If this
    // assertion ever fails the viewer would flash "AbortError" every
    // time the drawer closed.
    expect(s.err).toBeNull();
  });

  it('marks done when the generator exhausts without isComplete', async () => {
    // Server closed the stream early (shutdown, abort observed). We
    // shouldn't leave the spinner spinning.
    getBuildLogs.mockImplementation(async function* () {
      yield chunk([u8(0x7a)]);
      // No isComplete chunk, just end.
    });

    const s = createLogStream('build-early');
    await flush(3);

    expect(s.lines).toEqual(['z']);
    expect(s.done).toBe(true);
    expect(s.err).toBeNull();
  });

  it('passes drvPath through when provided', async () => {
    getBuildLogs.mockImplementation(async function* () {
      yield chunk([], true);
    });

    createLogStream('b', '/nix/store/xyz-foo.drv');
    await flush(2);

    const [req] = getBuildLogs.mock.calls[0];
    expect(req.derivationPath).toBe('/nix/store/xyz-foo.drv');
  });
});
