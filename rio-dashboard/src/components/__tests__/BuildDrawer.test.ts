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
// Spy (NOT replace) the log-stream factory: the mount tests above
// need the REAL chain (so "no stream constructed" is witnessed at
// the wire mock), while the oracle tests below capture the
// `isTerminal` closure BuildDrawer hands down — LogViewer forwards
// it verbatim into createLogStream, so the spy's third argument IS
// the drawer's oracle, captured at the real consumption seam.
vi.mock('../../lib/logStream.svelte', { spy: true });

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
import { createLogStream } from '../../lib/logStream.svelte';
import BuildDrawer, {
  mkDrawerSession,
  type DrawerSession,
} from '../BuildDrawer.svelte';

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

  // ---- the terminality oracle's legs (merged_bug_074) ----
  //
  // Every oracle input must derive from the drawer-lifetime graph
  // poll: the `build` prop is a click-time snapshot (Builds.svelte's
  // $effect tracks only statusFilter/pageIdx and `selected` is never
  // re-pointed when the list refreshes), so a prop-frozen leg poisons
  // the oracle in both directions.

  /** The drawer's oracle closure, captured at the real consumption
   * seam: LogViewer forwards the `isTerminal` prop verbatim into
   * createLogStream's options (third argument). */
  function capturedOracle(): () => boolean {
    const calls = vi.mocked(createLogStream).mock.calls;
    const opts = calls.at(-1)?.[2] as
      | { isTerminal?: () => boolean }
      | undefined;
    expect(opts?.isTerminal).toBeDefined();
    return opts!.isTerminal!;
  }

  /** Drive the drawer to a focused per-attempt mount via the
   * degraded-table click path (truncated views never latch, so the
   * later untruncated responses fully control the latch). */
  async function focusFirstNode(): Promise<void> {
    await fireEvent.click(screen.getByRole('tab', { name: 'Graph' }));
    await flushSvelte();
    await fireEvent.click(screen.getAllByTestId('graph-table-row')[0]);
    await flushSvelte();
  }

  it('oracle_follows_the_retry_after_out_of_band_clear_on_a_terminal_build', async () => {
    // PROPOSITION: the retry-follow flip the log-tail rule rides on —
    // on a TERMINAL-at-click build (state=4, the canonical poisoned
    // case), an out-of-band ClearPoison discovered by the settled
    // poll flips the oracle back to false so the stream follows the
    // retry instead of exiting. A frozen prop leg pins it true
    // forever (the resurrected pre-merged_bug_134 defect).
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['poisoned'], true))
      .mockResolvedValueOnce(mkResp(['poisoned']))
      .mockResolvedValue(mkResp(['queued']));

    render(BuildDrawer, { props: { build: mkBuild({ state: 4 }) } });
    await flushSvelte();
    await focusFirstNode();
    const oracle = capturedOracle();

    // The untruncated all-poisoned view settles the poll...
    await vi.advanceTimersByTimeAsync(5000);
    await flushSvelte();
    expect(oracle()).toBe(true);

    // ...and the settled-cadence probe discovers the out-of-band
    // clear: the shared row reads queued again. ZERO `build` prop
    // mutation happened — the flip must come from the poll.
    await vi.advanceTimersByTimeAsync(30_000);
    await flushSvelte();
    expect(oracle()).toBe(false);
  });

  it('oracle_reads_terminal_from_the_live_poll_not_the_prop', async () => {
    // PROPOSITION (structural pin for the post-090 partition): the
    // oracle's two legs are the focused node's live status and the
    // poll's allTerminal — it flips false -> true on an ACTIVE-at-
    // click build (state=2) purely from poll evidence, with ZERO
    // `build` prop mutation across the sequence. (Pre-fix this case
    // was green via the node leg — disclosed: the build-LEVEL leg's
    // false-frozen red was the whole-build forever-stream, whose
    // mount died with the bug_090 close last commit.)
    getBuildGraph
      .mockResolvedValueOnce(mkResp(['running'], true))
      .mockResolvedValue(mkResp(['completed']));

    render(BuildDrawer, { props: { build: mkBuild({ state: 2 }) } });
    await flushSvelte();
    await focusFirstNode();
    const oracle = capturedOracle();
    expect(oracle()).toBe(false);

    // The focused node completes and the (untruncated) view settles.
    await vi.advanceTimersByTimeAsync(5000);
    await flushSvelte();
    expect(oracle()).toBe(true);
  });

  // ---- the keyed session record (merged_bug_064) ----
  //
  // Per-build state has ONE owner whose lifecycle IS build.buildId: a
  // re-point replaces the whole record. The pre-fix shape recreated
  // only the poll while focusedDrv/focusedExecId survived, so the
  // {#key} remounted LogViewer with build B's id and build A's
  // NON-EMPTY exec-pinned selector -- past the api/logs.ts
  // empty-selector backstop (it refuses only the all-empty form).

  it('repoint_never_streams_the_previous_builds_selector', async () => {
    // PROPOSITION (the failure frame, end-to-end): build A's
    // exec-pinned selector never reaches the wire under build B's
    // header -- asserted at the wire mock's call record, not at
    // component internals.
    const execA = '01900000-0000-7000-8000-00000000aaa1';
    getBuildGraph.mockResolvedValue({
      nodes: [
        {
          drvPath: DRV0,
          pname: 'pkg-0',
          system: 'x86_64-linux',
          status: 'poisoned',
          assignedExecutorId: '',
          execId: execA,
        },
      ],
      edges: [],
      truncated: true,
      totalNodes: 1,
    });

    const { rerender } = render(BuildDrawer, {
      props: { build: mkBuild({ buildId: 'build-a' }) },
    });
    await flushSvelte();
    await focusFirstNode();
    await flushSvelte();

    // The legit per-attempt mount under A, with A's exec pin.
    expect(logsMock.tailLog).toHaveBeenCalledWith(
      expect.objectContaining({ derivation: DRV0, execId: execA }),
      expect.anything(),
    );
    logsMock.tailLog.mockClear();

    // Re-point the SAME drawer instance at build B (the deep-link
    // fallback race / keyboard-route shape -- no remount).
    await rerender({ build: mkBuild({ buildId: 'build-b' }) });
    await flushSvelte();
    await flushSvelte();

    expect(logsMock.tailLog).not.toHaveBeenCalledWith(
      expect.objectContaining({ derivation: DRV0, execId: execA }),
      expect.anything(),
    );
    // Post-fix shape: the Logs tab renders unfocused-by-design for B.
    expect(screen.queryByTestId('logs-unfocused')).not.toBeNull();
  });

  // r[verify dash.drawer.keyed-session]
  it('repoint_replaces_the_session_wholesale', async () => {
    // PROPOSITION (machine-censused membership): after a re-point,
    // EVERY session field reads its fresh-mint value -- the membership
    // is derived from Object.keys of a fresh constructor output (never
    // an author list), and the per-field observable map is total over
    // keyof DrawerSession (a new field fails compile until it maps).
    const execA = '01900000-0000-7000-8000-00000000aaa2';
    getBuildGraph.mockResolvedValue({
      nodes: [
        {
          drvPath: DRV0,
          pname: 'pkg-0',
          system: 'x86_64-linux',
          status: 'poisoned',
          assignedExecutorId: '',
          execId: execA,
        },
      ],
      edges: [],
      truncated: true,
      totalNodes: 1,
    });

    const { rerender } = render(BuildDrawer, {
      props: { build: mkBuild({ buildId: 'build-a' }) },
    });
    await flushSvelte();
    await focusFirstNode();
    await flushSvelte();
    logsMock.tailLog.mockClear();
    getBuildGraph.mockClear();

    await rerender({ build: mkBuild({ buildId: 'build-b' }) });
    await flushSvelte();
    await flushSvelte();

    // The fresh mint is the oracle for "reset": per session field,
    // one rendered observable certifies the field equals its
    // fresh-mint value (poll compared by buildId identity -- the
    // post-repoint dispatch carries B's id).
    const observableReset: Record<keyof DrawerSession, () => void> = {
      buildId: () => {
        // The key field: every keyed consumer re-minted under B — the
        // wire-visible one is the poll dispatch carrying B's id (the
        // constructor threads the key, so a stale key cannot dispatch).
        expect(getBuildGraph).toHaveBeenCalledWith(
          { buildId: 'build-b' },
          expect.anything(),
        );
      },
      poll: () => {
        expect(getBuildGraph).toHaveBeenCalledWith(
          { buildId: 'build-b' },
          expect.anything(),
        );
      },
      focusedDrv: () => {
        // fresh.focusedDrv === undefined renders the unfocused panel.
        expect(screen.queryByTestId('logs-unfocused')).not.toBeNull();
      },
      focusedExecId: () => {
        // fresh.focusedExecId === '' -- no stale exec pin reaches the
        // wire after the re-point.
        expect(logsMock.tailLog).not.toHaveBeenCalledWith(
          expect.objectContaining({ execId: execA }),
          expect.anything(),
        );
      },
    };
    const fresh = mkDrawerSession('census-probe');
    fresh.poll.destroy();
    for (const k of Object.keys(fresh)) {
      const probe = observableReset[k as keyof DrawerSession];
      expect(probe, `no reset observable mapped for session field ${k}`)
        .toBeDefined();
      probe();
    }
  });
});
