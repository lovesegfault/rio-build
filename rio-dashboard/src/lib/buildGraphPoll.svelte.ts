// ONE reactive node-status source per drawer (merged_bug_134).
//
// The GetBuildGraph poll used to live inside Graph.svelte — which
// unmounts whenever the drawer shows the Logs tab. BuildDrawer
// therefore fed the log stream's terminality oracle from a status
// snapshot CAPTURED AT CLICK TIME, frozen in both directions: a node
// clicked while running never read terminal after it completed
// mid-build (the stream re-dialed at the pacer cap with the tab stuck
// "streaming" for the rest of the build), and a node clicked while
// poisoned stayed "terminal" after ClearPoison (the stream exited
// incomplete instead of following the retry, against
// r[dash.stream.log-tail+6]).
//
// This store owns the poll for the drawer's whole lifetime, so the
// SAME live data feeds Graph's rendering AND the oracle — the
// frozen-snapshot configuration is unrepresentable: there is no second
// status source to capture from.
//
// r[impl dash.graph.auto-stop+1]
// The settle law lives here with the poll (merged_bug_043): once every
// node settles (and the view is complete — see the guards on the
// latch), the poll DOWNSHIFTS to the settled cadence
// (SETTLED_POLL_MS) instead of stopping — liveness for out-of-band
// clears (rio-cli, the admin RPC, another session) never depends on
// in-process notification; a settled drawer discovers them within the
// settled cadence and upshifts. `onCleared` still restores the live
// cadence immediately, which is exactly the contract
// ClearPoisonButton documents ("the next 5s poll picks up the new
// status"). Every completion-side state write is fenced by the
// dispatch generation (pollEpoch): a response dispatched before a
// clear/destroy is discarded wholesale by the single `settle`
// applier, so unfenced response-driven latching is unwritable.
import { admin } from '../api/admin';
import { TERMINAL } from './graphLayout';
import type { RawEdge, RawNode } from './layoutCore';
import { POLL_MS, startPoll } from './poll';

/** Settled-state poll cadence (merged_bug_043): a drawer whose build
 * fully settled keeps probing — slowly — so a ClearPoison from
 * rio-cli/admin RPC/another session is discovered within this bound
 * (12 → 2 RPC/min per settled drawer). */
export const SETTLED_POLL_MS = 30_000;

export type BuildGraphPoll = {
  /** Latest GetBuildGraph node set ($state.raw — wholesale replaced
   * per poll; identity-swap reactivity is all consumers need). */
  readonly nodes: readonly RawNode[];
  readonly edges: readonly RawEdge[];
  /** Server truncated the node set (first-N by insertion order). */
  readonly truncated: boolean;
  readonly totalNodes: number;
  /** True until the first response (success or failure) lands. */
  readonly loading: boolean;
  readonly error: string | null;
  /** Every node settled — the poll interval is stopped. */
  readonly allTerminal: boolean;
  /** The focused node's LIVE status — the terminality oracle input.
   * Reads the reactive node set at call time, so closures over this
   * see status transitions in both directions (completion AND
   * ClearPoison). */
  statusOf(drvPath: string | undefined): string | undefined;
  /** ClearPoison landed: un-latch allTerminal and restart the poll
   * (fires immediately, then every 5s until re-settled). */
  onCleared(): void;
  destroy(): void;
};

export function createBuildGraphPoll(buildId: string): BuildGraphPoll {
  let nodes = $state.raw<readonly RawNode[]>([]);
  let edges = $state.raw<readonly RawEdge[]>([]);
  let truncated = $state(false);
  let totalNodes = $state(0);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let allTerminal = $state(false);

  // Dispatch generation (merged_bug_043): captured when a fetch is
  // dispatched, compared by the single `settle` applier when its
  // response lands. `onCleared` and `destroy` bump it, so completions
  // whose server read predates the clear are discarded wholesale —
  // there is no unfenced write path for a response to re-latch
  // through.
  let pollEpoch = 0;
  // Re-entrancy gate, epoch-keyed: a CURRENT-epoch fetch in flight
  // serializes the pipeline (slow network, overlapping ticks), but a
  // STALE in-flight fetch must not swallow the restart's immediate
  // shot after a clear — its epoch differs, so a fresh dispatch
  // proceeds.
  let inflightEpoch: number | null = null;
  let destroyed = false;
  // Live until destroy(); never null while alive — settling swaps the
  // cadence instead of stopping (the downshift).
  let stop: (() => void) | null = null;

  /** Swap the poll cadence. `immediate` fires one shot now (the
   * onCleared contract); the settled downshift arms the interval
   * only — the response that settled IS the current state. */
  function startCadence(ms: number, immediate: boolean): void {
    stop?.();
    if (immediate) {
      stop = startPoll(fetchNow, ms);
    } else {
      // Same loop shape as startPoll minus the leading shot; the
      // document.hidden gate matches it (tab-switch quiesce).
      const id = setInterval(() => {
        if (document.hidden) return;
        void fetchNow();
      }, ms);
      stop = () => clearInterval(id);
    }
  }

  /** THE fenced applier (merged_bug_043): every completion-side state
   * write — the latch AND nodes/edges/truncated/totalNodes/error —
   * lands here, and only when the response's dispatch generation is
   * still current. Cadence shifts ride the latch EDGES, so a steady
   * settled state re-arms nothing. */
  function settle(
    epoch: number,
    outcome: {
      response?: Awaited<ReturnType<typeof admin.getBuildGraph>>;
      error?: unknown;
    },
  ): void {
    if (destroyed || epoch !== pollEpoch) return;
    if (outcome.response === undefined) {
      error = String(outcome.error);
      loading = false;
      return;
    }
    const r = outcome.response;
    error = null;
    // Terminal-settle check. `r.nodes.length > 0` guards the trivial
    // every([])→true — an empty response (build not yet populated)
    // must NOT settle. `!r.truncated` guards against settling on a
    // partial view: insertion-order truncation means roots settle
    // first while the tail may still run.
    const settled =
      !r.truncated &&
      r.nodes.length > 0 &&
      r.nodes.every((n: RawNode) => TERMINAL.has(n.status));
    if (settled && !allTerminal) {
      allTerminal = true;
      // Downshift, don't stop: out-of-band clears are discovered
      // within SETTLED_POLL_MS (no immediate shot — this response
      // IS the settled state).
      startCadence(SETTLED_POLL_MS, false);
    } else if (!settled && allTerminal) {
      // The settled probe found live work (an out-of-band clear):
      // un-latch and restore the live cadence.
      allTerminal = false;
      startCadence(POLL_MS, false);
    }
    nodes = r.nodes;
    edges = r.edges;
    truncated = r.truncated;
    totalNodes = r.totalNodes;
    loading = false;
  }

  async function fetchNow(): Promise<void> {
    if (destroyed) return;
    const epoch = pollEpoch;
    if (inflightEpoch === epoch) return;
    inflightEpoch = epoch;
    try {
      let r;
      try {
        r = await admin.getBuildGraph({ buildId });
      } catch (e) {
        settle(epoch, { error: e });
        return;
      }
      settle(epoch, { response: r });
    } finally {
      if (inflightEpoch === epoch) inflightEpoch = null;
    }
  }

  stop = startPoll(fetchNow);

  return {
    get nodes() {
      return nodes;
    },
    get edges() {
      return edges;
    },
    get truncated() {
      return truncated;
    },
    get totalNodes() {
      return totalNodes;
    },
    get loading() {
      return loading;
    },
    get error() {
      return error;
    },
    get allTerminal() {
      return allTerminal;
    },
    statusOf(drvPath: string | undefined): string | undefined {
      if (drvPath === undefined) return undefined;
      return nodes.find((n) => n.drvPath === drvPath)?.status;
    },
    onCleared() {
      if (destroyed) return;
      // The clear invalidates every in-flight dispatch: a response
      // whose server read predates it must not re-latch.
      pollEpoch += 1;
      allTerminal = false;
      // Restore the live cadence with an immediate shot — the
      // cleared node's queued status lands on the first poll
      // (the ClearPoisonButton contract).
      startCadence(POLL_MS, true);
    },
    destroy() {
      destroyed = true;
      pollEpoch += 1;
      stop?.();
      stop = null;
    },
  };
}
