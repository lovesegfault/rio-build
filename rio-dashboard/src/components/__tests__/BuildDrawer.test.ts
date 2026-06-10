// r[verify dash.log.attempt-scope]
// BuildDrawer mount-scope battery (bug_090, signed Q4): the Logs tab
// is the PER-ATTEMPT surface. The default render (no focused
// derivation) must construct NO log stream — the empty TailLog
// selector has no resolver (drv_log_hash('') matches no execution),
// so the pre-fix default mount was a guaranteed-NotFound dial loop.
// The focused leg (Graph node click → per-attempt stream) is the
// killer journey's log leg and must stay intact.
//
// Lanes: adminMock backs the drawer-lifetime graph poll; logsMock
// backs the real LogViewer→logStream chain (Builds.test.ts
// precedent) — the mount tests here deliberately keep logStream REAL
// so "no stream constructed" is witnessed at the wire mock, not at a
// mocked factory. Graph interactions ride the degraded-table branch
// (truncated views), the jsdom-safe path Graph.test.ts documents.
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  adminMock,
  flushSvelte,
  logsMock,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));
vi.mock('../../api/logs', () => ({ logs: logsMock }));

// Graph.svelte imports the layout worker; jsdom has no Worker. The
// degraded-table branch never posts to it — stub the constructor
// (Graph.test.ts precedent).
vi.mock('../../lib/graphLayout.worker?worker', () => ({
  default: class {
    postMessage() {}
    terminate() {}
    addEventListener() {}
    removeEventListener() {}
  },
}));

import type { BuildInfo } from '../../api/types';
import BuildDrawer from '../BuildDrawer.svelte';

const { getBuildGraph } = adminMock;

const DRV0 = `/nix/store/${'a'.repeat(32)}-pkg-0.drv`;

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

// Structurally-compatible BuildInfo (Builds.test.ts precedent): the
// drawer only renders these fields; `state` is the raw wire enum
// value (2 = ACTIVE, 4 = FAILED).
function mkBuild(over: Partial<Record<string, unknown>> = {}): BuildInfo {
  return {
    buildId: 'aaaa-bbbb-cccc-dddd',
    tenantId: 'tenant-1',
    priorityClass: 'normal',
    state: 2,
    totalDerivations: 1,
    completedDerivations: 0,
    cachedDerivations: 0,
    submittedAt: undefined,
    startedAt: undefined,
    finishedAt: undefined,
    errorSummary: '',
    ...over,
  } as unknown as BuildInfo;
}

describe('BuildDrawer log-stream scope', () => {
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  it('default_drawer_render_mounts_no_log_stream', async () => {
    // PROPOSITION: the doomed empty selector never reaches the wire
    // from the DEFAULT render — no stream object is constructed
    // while no derivation is focused, and the unfocused Logs tab
    // renders the unavailable-by-design affordance instead.
    getBuildGraph.mockResolvedValue(mkResp(['running'], true));

    render(BuildDrawer, { props: { build: mkBuild() } });
    await flushSvelte();
    await flushSvelte();

    expect(logsMock.tailLog).not.toHaveBeenCalled();
    const panel = screen.getByTestId('logs-unfocused');
    expect(panel.textContent).toContain('Logs are per-attempt');
    expect(panel.textContent).toContain('unavailable by design');
  });

  it('focused_node_still_mounts_the_per_attempt_stream', async () => {
    // PROPOSITION (green-preservation): the killer journey's log leg
    // is intact — a Graph node click focuses the derivation and the
    // Logs tab mounts the per-attempt stream with THAT selector.
    getBuildGraph.mockResolvedValue(mkResp(['poisoned'], true));

    render(BuildDrawer, { props: { build: mkBuild() } });
    await flushSvelte();

    // Graph tab → degraded table (truncated view) → row click.
    await fireEvent.click(screen.getByRole('tab', { name: 'Graph' }));
    await flushSvelte();
    await fireEvent.click(screen.getAllByTestId('graph-table-row')[0]);
    await flushSvelte();
    await flushSvelte();

    expect(screen.queryByTestId('logs-unfocused')).toBeNull();
    expect(logsMock.tailLog).toHaveBeenCalled();
    const req = logsMock.tailLog.mock.calls[0][0] as { derivation: string };
    expect(req.derivation).toBe(DRV0);
  });
});
