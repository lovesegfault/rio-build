// Unit test for the standby-replica retry interceptor. The HTTPRoute
// backendRef is the ClusterIP rio-scheduler Service with both replicas
// Ready (tcpSocket probe), so ANY RPC — unary or server-stream — can
// land on the standby and receive Trailers-Only Unavailable. This test
// proves the interceptor retries that case for both kinds.
import { Code, ConnectError } from '@connectrpc/connect';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { retryUnavailableOnce } from '../transport';

// The Interceptor type is (next) => (req) => Promise<res>; we drive it
// with a hand-rolled `next` and a structurally-minimal `req`. The
// interceptor only inspects `req` insofar as it re-passes it to `next`,
// so the cast is safe.
type Next = Parameters<typeof retryUnavailableOnce>[0];

describe('retryUnavailableOnce', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  async function run(req: unknown, next: Next) {
    const p = retryUnavailableOnce(next)(req as Parameters<Next>[0]);
    // Attach a no-op catch so a rejection isn't flagged as unhandled
    // while we drain the 200ms backoff timer; the caller's
    // `await`/`expect().rejects` observes the original `p`.
    p.catch(() => {});
    await vi.runAllTimersAsync();
    return p;
  }

  it('retries server-stream Unavailable once', async () => {
    // Regression: a `req.stream` short-circuit used to re-throw
    // immediately on the false premise that streams "establish on the
    // leader". They don't — the opening POST is load-balanced. A
    // Trailers-Only Unavailable from the standby throws from next()
    // before any message yields, so the retry path is sound.
    const ok = { ok: true };
    const next = vi
      .fn()
      .mockRejectedValueOnce(new ConnectError('standby', Code.Unavailable))
      .mockResolvedValueOnce(ok);

    const res = await run({ stream: true }, next as unknown as Next);

    expect(next).toHaveBeenCalledTimes(2);
    expect(res).toBe(ok);
  });

  it('retries unary Unavailable once', async () => {
    const ok = { ok: true };
    const next = vi
      .fn()
      .mockRejectedValueOnce(new ConnectError('standby', Code.Unavailable))
      .mockResolvedValueOnce(ok);

    const res = await run({ stream: false }, next as unknown as Next);

    expect(next).toHaveBeenCalledTimes(2);
    expect(res).toBe(ok);
  });

  it('re-throws non-Unavailable without retry', async () => {
    const next = vi
      .fn()
      .mockRejectedValue(new ConnectError('nope', Code.PermissionDenied));

    await expect(
      run({ stream: false }, next as unknown as Next),
    ).rejects.toThrow('nope');
    expect(next).toHaveBeenCalledTimes(1);
  });

  it('retries at most once', async () => {
    // Two Unavailables in a row → second one surfaces. The "once" is
    // deliberate: with replicas=2 a single retry virtually always lands
    // on the leader; unbounded retry would mask a real outage.
    const next = vi
      .fn()
      .mockRejectedValue(new ConnectError('standby', Code.Unavailable));

    await expect(
      run({ stream: true }, next as unknown as Next),
    ).rejects.toThrow('standby');
    expect(next).toHaveBeenCalledTimes(2);
  });
});
