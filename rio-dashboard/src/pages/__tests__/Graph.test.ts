// Graph page poll semantics: inflight re-entrancy gate + terminal poll
// stop. We deliberately AVOID the SvelteFlow render path: every mock
// response sets `truncated: true` so Graph.svelte routes to the
// degraded-table branch (plain DOM, no @xyflow/svelte mount). The
// xyflow canvas pulls in svelte's MediaQuery reactive store, which
// calls window.matchMedia — unimplemented under jsdom — and further
// wants ResizeObserver for fitView. None of that matters for the
// behaviours under test: the inflight gate, the allTerminal check, and
// the poll interval all run BEFORE the truncation branch in
// fetchAndLayout, so routing to the table doesn't change the
// call-count assertions.
import { render } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  adminMock,
  flushSvelte,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));

// The `?worker` import resolves under vite's client types; the
// constructor is stubbed to a no-op so the second $effect's teardown
// (worker?.terminate()) doesn't blow up if it happens to run. Truncated
// responses keep node-count under WORKER_THRESHOLD anyway, so
// layoutInWorker is never entered.
vi.mock('../../lib/graphLayout.worker?worker', () => ({
  default: class {
    postMessage() {}
    terminate() {}
    addEventListener() {}
    removeEventListener() {}
  },
}));

// Stub the sync layout path too: when truncated:false with few nodes,
// Graph.svelte calls layoutGraph() which runs dagre + hands nodes to
// SvelteFlow. SvelteFlow needs real DOM geometry jsdom can't provide.
// Returning degraded:true routes to the table branch, keeping the
// poll-loop semantics under test without the render-path complexity.
vi.mock('../../lib/graphLayout', async (orig) => {
  const actual = await orig<typeof import('../../lib/graphLayout')>();
  return {
    ...actual,
    layoutGraph: () => ({ degraded: true, reason: 'test-stub', nodes: [] }),
  };
});

import Graph from '../Graph.svelte';

const { getBuildGraph } = adminMock;

// truncated: true forces the degraded-table branch (no SvelteFlow, no
// dagre — fast and jsdom-safe). The inflight gate fires BEFORE the
// truncation check so that test is unaffected. The allTerminal check
// is GUARDED on !truncated (truncated = visible-terminal doesn't
// imply all-terminal) so the poll-stop test needs truncated:false.
function mkResp(statuses: string[], truncated = true) {
  return {
    nodes: statuses.map((status, i) => ({
      drvPath: `/nix/store/${'a'.repeat(32)}-pkg-${i}.drv`,
      pname: `pkg-${i}`,
      system: 'x86_64-linux',
      status,
      assignedWorkerId: '',
    })),
    edges: [],
    truncated,
    totalNodes: statuses.length,
  };
}

describe('Graph page poll loop', () => {
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  it('skips overlapping polls while a request is inflight', async () => {
    // Hold the RPC open under manual control. Two setInterval ticks fire
    // while the first promise is unresolved; the inflight gate must
    // short-circuit both — without the gate, call-count would climb to 3
    // (initial + 2 interval ticks) and the resulting concurrent promises
    // could resolve out-of-order (last-write-wins on `layout =`).
    let resolve!: (v: unknown) => void;
    getBuildGraph.mockImplementation(
      () => new Promise((r) => (resolve = r)),
    );

    render(Graph, { props: { buildId: 'b-1' } });
    await flushSvelte();
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    // Two 5s intervals elapse; both bounce off `if (inflight) return`.
    await vi.advanceTimersByTimeAsync(5000);
    await vi.advanceTimersByTimeAsync(5000);
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    // Release the first fetch — inflight clears, the NEXT interval tick
    // gets through. truncated:true routes to the degraded-table branch
    // (no SvelteFlow, no dagre) so this stays fast and jsdom-safe.
    resolve(mkResp(['running']));
    await flushSvelte();
    await vi.advanceTimersByTimeAsync(5000);
    expect(getBuildGraph).toHaveBeenCalledTimes(2);
  });

  it('stops polling once every node is terminal', async () => {
    // First response has a running node → poll continues. Second flips
    // to all-terminal → allTerminal $state fires, the effect re-runs
    // WITHOUT registering a new setInterval. Advancing further produces
    // no new calls. (The effect's unconditional fetchAndLayout-on-re-run
    // accounts for the third call — it's the poll-after-settle drain,
    // and the sig-match+patch path makes it cheap.)
    // truncated:false so the !r.truncated allTerminal guard fires.
    // 2 nodes < DEGRADE_THRESHOLD means this hits the SvelteFlow path;
    // the layout worker is stubbed via adminMock so this stays jsdom-safe.
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['running', 'completed'], false))
      .mockResolvedValue(mkResp(['completed', 'skipped'], false));

    render(Graph, { props: { buildId: 'b-2' } });
    await flushSvelte();
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    // One interval tick — response is all-terminal, allTerminal flips.
    await vi.advanceTimersByTimeAsync(5000);
    await flushSvelte();
    // Call 2 (interval) + call 3 (effect re-run's unconditional fetch).
    // The exact count is less interesting than the invariant below.
    const settled = getBuildGraph.mock.calls.length;
    expect(settled).toBeGreaterThanOrEqual(2);

    // Advance arbitrarily far — no interval, no further calls.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(getBuildGraph).toHaveBeenCalledTimes(settled);
  });

  it('does NOT stop polling on empty response', async () => {
    // every([]) → true is the vacuous-truth footgun. An empty nodes
    // array (build just submitted, scheduler hasn't walked the DAG yet)
    // must keep polling or the page freezes on "loading graph…" with
    // the drawer open.
    getBuildGraph.mockResolvedValue(mkResp([]));

    render(Graph, { props: { buildId: 'b-3' } });
    await flushSvelte();
    expect(getBuildGraph).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(5000);
    await flushSvelte();
    await vi.advanceTimersByTimeAsync(5000);
    await flushSvelte();
    // Still climbing — the length-0 guard kept the interval alive.
    expect(getBuildGraph.mock.calls.length).toBeGreaterThanOrEqual(3);
  });
});
