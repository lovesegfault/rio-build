// r[verify dash.log.attempt-scope]
// The TailLog selector boundary (bug_090): the selector alphabet at
// the ONE seam every dashboard tail crosses is CLOSED — pinned
// (execId != '') | derivation (drvPath != ''). The empty form has no
// resolver anywhere in the store (drv_log_hash('') = '' matches no
// execution), so it is refused HERE, before the transport, with the
// store's own permanent-unservable type — logStream's exit law
// already classifies that metadata as terminal (no re-dial).
//
// Lane: transport-stub (transport.test.ts precedent) — the REAL logs
// module is driven against a stubbed transport, so "never touches
// the wire" is witnessed at the transport seam itself.
import { Code, ConnectError } from '@connectrpc/connect';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const transportStub = vi.hoisted(() => ({
  unary: vi.fn(),
  stream: vi.fn(),
}));
vi.mock('../transport', () => ({ transport: transportStub }));

import { logs } from '../logs';

type TailReq = {
  derivation: string;
  execId: string;
  sinceLine: bigint;
  follow: boolean;
};

function mkStreamResponse() {
  return {
    header: new Headers(),
    message: (async function* () {})(),
    trailer: new Headers(),
  };
}

async function drainInput(callIndex: number): Promise<TailReq[]> {
  // transport.stream's 5th argument is the input message iterable
  // (the promise-client wraps the single request in an async
  // iterable) — draining it shows exactly what reached the wire seam.
  const input = transportStub.stream.mock.calls[callIndex][4] as
    | AsyncIterable<TailReq>
    | undefined;
  const msgs: TailReq[] = [];
  if (input !== undefined) {
    for await (const m of input) msgs.push(m);
  }
  return msgs;
}

describe('logs.tailLog selector boundary', () => {
  beforeEach(() => {
    transportStub.stream.mockResolvedValue(mkStreamResponse());
  });
  afterEach(() => {
    transportStub.stream.mockReset();
    transportStub.unary.mockReset();
  });

  it('empty_selector_is_refused_before_the_transport', async () => {
    // PROPOSITION: the structurally-unservable selector (no
    // derivation, no pinned exec) NEVER reaches the transport, and
    // the refusal carries the store's permanent-unservable metadata
    // so the stream's existing exit law terminates without re-dial.
    const iterable = logs.tailLog({
      derivation: '',
      execId: '',
      sinceLine: 0n,
      follow: true,
    });
    expect(transportStub.stream).not.toHaveBeenCalled();

    const it = iterable[Symbol.asyncIterator]();
    let caught: unknown;
    try {
      await it.next();
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeDefined();
    const ce = ConnectError.from(caught);
    expect(ce.code).toBe(Code.FailedPrecondition);
    expect(ce.metadata.get('x-rio-log-unservable')).not.toBeNull();
    expect(transportStub.stream).not.toHaveBeenCalled();
  });

  it('pinned_selector_passes_through_untouched', async () => {
    // PROPOSITION: a pinned execution (execId != '') is a servable
    // selector — it reaches the transport verbatim.
    logs.tailLog({
      derivation: '',
      execId: 'exec-1',
      sinceLine: 0n,
      follow: true,
    });
    expect(transportStub.stream).toHaveBeenCalledTimes(1);
    const msgs = await drainInput(0);
    expect(msgs).toEqual([
      { derivation: '', execId: 'exec-1', sinceLine: 0n, follow: true },
    ]);
  });

  it('derivation_selector_passes_through_untouched', async () => {
    // PROPOSITION: a non-empty derivation is a servable selector —
    // it reaches the transport verbatim.
    const drv = `/nix/store/${'a'.repeat(32)}-pkg-0.drv`;
    logs.tailLog({
      derivation: drv,
      execId: '',
      sinceLine: 5n,
      follow: false,
    });
    expect(transportStub.stream).toHaveBeenCalledTimes(1);
    const msgs = await drainInput(0);
    expect(msgs).toEqual([
      { derivation: drv, execId: '', sinceLine: 5n, follow: false },
    ]);
  });
});
