// Graph component LAYOUT semantics. The poll moved to
// lib/buildGraphPoll.svelte.ts (merged_bug_134) and carries its own
// battery in lib/__tests__/buildGraphPoll.test.ts — this file covers
// the half that stayed: degraded-table routing, the row-click
// callback, and the loading/error surfaces. We deliberately AVOID the
// SvelteFlow render path: every stub sets `truncated: true` so
// Graph.svelte routes to the degraded-table branch (plain DOM, no
// @xyflow/svelte mount). The xyflow canvas pulls in svelte's
// MediaQuery reactive store, which calls window.matchMedia —
// unimplemented under jsdom — and further wants ResizeObserver for
// fitView.
import { render } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  flushSvelte,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

// The `?worker` import resolves under vite's client types; the
// constructor is stubbed to a no-op so the second $effect's teardown
// (worker?.terminate()) doesn't blow up if it happens to run. Truncated
// stubs keep node-count under WORKER_THRESHOLD anyway, so
// layoutInWorker is never entered.
vi.mock('../../lib/graphLayout.worker?worker', () => ({
  default: class {
    postMessage() {}
    terminate() {}
    addEventListener() {}
    removeEventListener() {}
  },
}));

import type { BuildGraphPoll } from '../../lib/buildGraphPoll.svelte';
import Graph from '../Graph.svelte';

function mkNode(status: string, i: number) {
  return {
    drvPath: `/nix/store/${'a'.repeat(32)}-pkg-${i}.drv`,
    pname: `pkg-${i}`,
    system: 'x86_64-linux',
    status,
    assignedExecutorId: '',
    execId: '',
  };
}

// A poll-shaped stub: Graph only consumes the BuildGraphPoll
// interface, so a plain object is enough for render assertions (the
// props are read on mount; no cross-render reactivity is asserted
// here — that half lives in the store's own battery).
function stubPoll(
  statuses: string[],
  opts: { truncated?: boolean; loading?: boolean; error?: string | null } = {},
): BuildGraphPoll & { cleared: number } {
  const nodes = statuses.map((s, i) => mkNode(s, i));
  const stub = {
    nodes,
    edges: [],
    truncated: opts.truncated ?? true,
    totalNodes: nodes.length,
    loading: opts.loading ?? false,
    error: opts.error ?? null,
    // Graph renders only `error` (never-loaded surface); `degraded`
    // is BuildDrawer's staleness note (merged_bug_081) — inert here.
    degraded: null,
    allTerminal: false,
    cleared: 0,
    statusOf(drvPath: string | undefined) {
      return nodes.find((n) => n.drvPath === drvPath)?.status;
    },
    onCleared() {
      stub.cleared += 1;
    },
    destroy() {},
  };
  return stub;
}

describe('Graph layout half', () => {
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  it('routes a truncated view to the degraded table', async () => {
    const poll = stubPoll(['running', 'poisoned'], { truncated: true });
    const { getByTestId, getAllByTestId } = render(Graph, {
      props: { poll },
    });
    await flushSvelte();
    expect(getByTestId('graph-degraded')).toBeTruthy();
    // sortForTable puts poisoned first.
    const rows = getAllByTestId('graph-table-row');
    expect(rows.length).toBe(2);
    expect(rows[0].textContent).toContain('poisoned');
  });

  it('table row click reports drvPath + execId + status', async () => {
    const poll = stubPoll(['poisoned'], { truncated: true });
    const clicks: [string, string, string][] = [];
    const { getAllByTestId } = render(Graph, {
      props: {
        poll,
        ondrvclick: (drv: string, execId: string, status: string) =>
          clicks.push([drv, execId, status]),
      },
    });
    await flushSvelte();
    getAllByTestId('graph-table-row')[0].click();
    expect(clicks).toEqual([
      [`/nix/store/${'a'.repeat(32)}-pkg-0.drv`, '', 'poisoned'],
    ]);
  });

  it('renders the loading state until the store has data', async () => {
    const poll = stubPoll([], { loading: true, truncated: false });
    const { container } = render(Graph, { props: { poll } });
    await flushSvelte();
    expect(container.textContent).toContain('loading graph…');
  });

  it('renders the store error', async () => {
    const poll = stubPoll([], { error: 'boom', truncated: false });
    const { container } = render(Graph, { props: { poll } });
    await flushSvelte();
    expect(container.textContent).toContain('graph fetch failed: boom');
  });
});
