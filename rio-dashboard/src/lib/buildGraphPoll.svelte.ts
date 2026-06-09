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
// r[impl dash.graph.auto-stop]
// The auto-stop law moves here with the poll: once every node settles
// (and the view is complete — see the guards on the latch), the
// interval stops; `onCleared` un-latches and restarts it, which is
// exactly the contract ClearPoisonButton documents ("the next 5s poll
// picks up the new status") — pre-fix the latch made that contract
// unsatisfiable: the poll was permanently stopped on a settled build,
// so the cleared node rendered poisoned until remount.
import { admin } from '../api/admin';
import { TERMINAL } from './graphLayout';
import type { RawEdge, RawNode } from './layoutCore';
import { startPoll } from './poll';

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

  // Re-entrancy gate: the interval fires regardless of whether the
  // last fetch finished — a slow network means overlapping calls, and
  // concurrent responses can land out of order (last-write-wins on the
  // state). The gate serializes the pipeline (same discipline the
  // in-component poll had).
  let inflight = false;
  // Live until destroy(); the interval handle is null while stopped
  // (settled) and re-minted by onCleared.
  let destroyed = false;
  let stop: (() => void) | null = null;

  async function fetchNow(): Promise<void> {
    if (inflight || destroyed) return;
    inflight = true;
    try {
      let r;
      try {
        r = await admin.getBuildGraph({ buildId });
        error = null;
      } catch (e) {
        error = String(e);
        loading = false;
        return;
      }
      // Terminal-settle check. `r.nodes.length > 0` guards the trivial
      // every([])→true — an empty response (build not yet populated)
      // must NOT stop polling. `!r.truncated` guards against settling
      // on a partial view: insertion-order truncation means roots
      // settle first while the tail may still run.
      if (
        !r.truncated &&
        r.nodes.length > 0 &&
        r.nodes.every((n: RawNode) => TERMINAL.has(n.status))
      ) {
        allTerminal = true;
        // Stop the interval; the response that latched IS the final
        // state. onCleared un-latches and restarts.
        stop?.();
        stop = null;
      }
      nodes = r.nodes;
      edges = r.edges;
      truncated = r.truncated;
      totalNodes = r.totalNodes;
      loading = false;
    } finally {
      inflight = false;
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
      allTerminal = false;
      if (stop === null) {
        // startPoll fires immediately, then resumes the 5s cadence —
        // the cleared node's queued status lands on the first shot.
        stop = startPoll(fetchNow);
      }
    },
    destroy() {
      destroyed = true;
      stop?.();
      stop = null;
    },
  };
}
