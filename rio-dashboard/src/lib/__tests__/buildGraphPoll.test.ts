// r[verify dash.graph.auto-stop+1]
// The drawer-lifetime poll store (merged_bug_134): one reactive
// node-status source feeding Graph's rendering AND the log stream's
// terminality oracle. The poll-loop semantics moved here from
// Graph.svelte (which unmounts on the Logs tab — exactly why the
// oracle had been frozen at click time); the original component-level
// battery (inflight gate, terminal settle, empty-response no-stop)
// migrates with the code, plus the laws the move exists for:
// statusOf is LIVE in both directions, onCleared restarts a settled
// poll, and (merged_bug_043) responses are epoch-fenced while settled
// state downshifts to the slow cadence instead of stopping.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  adminMock,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));

import { createBuildGraphPoll } from '../buildGraphPoll.svelte';

const { getBuildGraph } = adminMock;

function mkResp(statuses: string[], truncated = false) {
  return {
    nodes: statuses.map((status, i) => ({
      drvPath: `/nix/store/${'a'.repeat(32)}-pkg-${i}.drv`,
      pname: `pkg-${i}`,
      system: 'x86_64-linux',
      status,
      assignedExecutorId: '',
      execId: '',
    })),
    edges: [],
    truncated,
    totalNodes: statuses.length,
  };
}

async function flush(rounds = 4): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await Promise.resolve();
  }
}

describe('createBuildGraphPoll', () => {
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  it('skips overlapping polls while a request is inflight', async () => {
    let resolve!: (v: unknown) => void;
    getBuildGraph.mockImplementation(() => new Promise((r) => (resolve = r)));

    const p = createBuildGraphPoll('b-1');
    await flush();
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    // Two 5s intervals elapse; both bounce off the inflight gate.
    await vi.advanceTimersByTimeAsync(5000);
    await vi.advanceTimersByTimeAsync(5000);
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    // Release — the NEXT tick gets through.
    resolve(mkResp(['running']));
    await flush();
    await vi.advanceTimersByTimeAsync(5000);
    expect(getBuildGraph).toHaveBeenCalledTimes(2);
    p.destroy();
  });

  it('downshifts to the settled cadence once every node is terminal', async () => {
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['running', 'completed']))
      .mockResolvedValue(mkResp(['completed', 'skipped']));

    const p = createBuildGraphPoll('b-2');
    await flush();
    expect(getBuildGraph).toHaveBeenCalledTimes(1);
    expect(p.allTerminal).toBe(false);

    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(p.allTerminal).toBe(true);
    const settled = getBuildGraph.mock.calls.length;

    // The live 5s cadence is gone: nothing fires before the settled
    // interval elapses...
    await vi.advanceTimersByTimeAsync(25_000);
    await flush();
    expect(getBuildGraph).toHaveBeenCalledTimes(settled);
    // ...and the settled cadence probes (still terminal: stays
    // settled, 2 RPC/min instead of 12).
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(getBuildGraph).toHaveBeenCalledTimes(settled + 1);
    expect(p.allTerminal).toBe(true);
    await vi.advanceTimersByTimeAsync(30_000);
    await flush();
    expect(getBuildGraph).toHaveBeenCalledTimes(settled + 2);
    expect(p.allTerminal).toBe(true);
    p.destroy();
  });

  it('does NOT stop polling on empty response', async () => {
    // every([]) → true is the vacuous-truth footgun: an empty node set
    // (build just submitted) must keep polling.
    getBuildGraph.mockResolvedValue(mkResp([]));

    const p = createBuildGraphPoll('b-3');
    await flush();
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(getBuildGraph.mock.calls.length).toBeGreaterThanOrEqual(3);
    expect(p.allTerminal).toBe(false);
    p.destroy();
  });

  it('does NOT latch terminal on a truncated view', async () => {
    // Insertion-order truncation settles roots first — a fully
    // terminal PARTIAL view says nothing about the tail.
    getBuildGraph.mockResolvedValue(mkResp(['completed', 'skipped'], true));

    const p = createBuildGraphPoll('b-4');
    await flush();
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(p.allTerminal).toBe(false);
    expect(getBuildGraph.mock.calls.length).toBeGreaterThanOrEqual(2);
    p.destroy();
  });

  // r[verify dash.stream.log-tail+6]
  /// merged_bug_134's recorded red, oracle half: focusedStatus was
  /// captured once at click — the terminality oracle frozen in both
  /// directions. statusOf reads the live node set: a mid-build
  /// completion flips it terminal (the stream's grace can arm instead
  /// of the pacer-cap re-dial loop), and ClearPoison flips it back.
  it('statusOf_is_live_in_both_directions: the oracle follows the poll, not the click', async () => {
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['running']))
      .mockResolvedValueOnce(mkResp(['poisoned']))
      .mockResolvedValue(mkResp(['queued']));

    const p = createBuildGraphPoll('b-5');
    await flush();
    const drv = `/nix/store/${'a'.repeat(32)}-pkg-0.drv`;
    // The closure a drawer would build over statusOf — equivalent of
    // the log stream's isTerminal oracle input.
    const oracle = () => p.statusOf(drv);
    expect(oracle()).toBe('running');

    // Mid-build settle: poisoned (terminal) — the direction that used
    // to wedge the tab "streaming" forever.
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(oracle()).toBe('poisoned');

    // ClearPoison: back to non-terminal — the direction that used to
    // exit "incomplete" instead of following the retry. The poll
    // latched on the all-poisoned view, so the un-latch + restart is
    // what makes this observable at all.
    expect(p.allTerminal).toBe(true);
    p.onCleared();
    await flush();
    expect(oracle()).toBe('queued');
    p.destroy();
  });

  it('onCleared restarts a settled poll immediately and re-latches later', async () => {
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['poisoned']))
      .mockResolvedValueOnce(mkResp(['queued']))
      .mockResolvedValue(mkResp(['completed']));

    const p = createBuildGraphPoll('b-6');
    await flush();
    expect(p.allTerminal).toBe(true);
    const settled = getBuildGraph.mock.calls.length;
    // Settled: the live cadence is gone (the settled interval has
    // not elapsed yet).
    await vi.advanceTimersByTimeAsync(25_000);
    expect(getBuildGraph).toHaveBeenCalledTimes(settled);

    // ClearPoison → immediate fetch (queued), polling resumes at the
    // live cadence, then re-latches on the completed view.
    p.onCleared();
    await flush();
    expect(p.allTerminal).toBe(false);
    expect(p.statusOf(`/nix/store/${'a'.repeat(32)}-pkg-0.drv`)).toBe('queued');
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(p.allTerminal).toBe(true);
    const resettled = getBuildGraph.mock.calls.length;
    // Re-settled: quiet through the live cadence window again.
    await vi.advanceTimersByTimeAsync(25_000);
    expect(getBuildGraph).toHaveBeenCalledTimes(resettled);
    p.destroy();
  });

  it('stale_inflight_response_cannot_relatch_after_clear: a response whose server read predates the clear is discarded', async () => {
    // merged_bug_043 red #1: the latch fired purely on response
    // content with no generation fence, and onCleared never
    // invalidated the in-flight fetch — a GetBuildGraph response
    // whose server read predates the clear re-latched and stopped
    // the poll on the stale snapshot.
    let release!: (v: unknown) => void;
    getBuildGraph
      .mockImplementationOnce(() => new Promise((r) => (release = r)))
      .mockResolvedValue(mkResp(['queued']));

    const p = createBuildGraphPoll('b-8');
    await flush();
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    // ClearPoison lands while the fetch is in flight: the clear
    // invalidates everything dispatched before it.
    p.onCleared();
    await flush();

    // The held (pre-clear) response resolves all-terminal — the
    // server read it answers predates the clear.
    release(mkResp(['poisoned']));
    await flush();

    expect(p.allTerminal).toBe(false);
    // The poll is alive on the live cadence: the next tick fetches.
    const before = getBuildGraph.mock.calls.length;
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(getBuildGraph.mock.calls.length).toBeGreaterThan(before);
    p.destroy();
  });

  it('out_of_band_clear_is_discovered_without_oncleared: a CLI/admin clear un-settles within the settled cadence', async () => {
    // merged_bug_043 red #2: the only production onCleared wiring is
    // the ClearPoisonButton chain — a clear from rio-cli, the admin
    // RPC, or another session never un-latched, freezing the drawer
    // on the poisoned snapshot until remount. Settled state must
    // DOWNSHIFT (slow cadence), not stop: liveness for out-of-band
    // clears cannot depend on in-process notification.
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['poisoned']))
      .mockResolvedValue(mkResp(['running']));

    const p = createBuildGraphPoll('b-9');
    await flush();
    expect(p.allTerminal).toBe(true);
    const settled = getBuildGraph.mock.calls.length;

    // The out-of-band clear re-runs the node; NO onCleared fires in
    // this tab. The settled cadence discovers it.
    await vi.advanceTimersByTimeAsync(30_000);
    await flush();
    expect(getBuildGraph.mock.calls.length).toBeGreaterThan(settled);
    expect(p.allTerminal).toBe(false);

    // Discovery restores the live 5s cadence.
    const upshifted = getBuildGraph.mock.calls.length;
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(getBuildGraph.mock.calls.length).toBeGreaterThan(upshifted);
    p.destroy();
  });

  it('destroy stops the interval for good — onCleared after destroy is a no-op', async () => {
    getBuildGraph.mockResolvedValue(mkResp(['running']));
    const p = createBuildGraphPoll('b-7');
    await flush();
    const n = getBuildGraph.mock.calls.length;
    p.destroy();
    p.onCleared();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(getBuildGraph).toHaveBeenCalledTimes(n);
  });
});
