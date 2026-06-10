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
// incomplete instead of following the retry, against the log-tail
// rule's retry-follow clause — dash.stream.log-tail).
//
// This store owns the poll for the drawer's whole lifetime, so the
// SAME live data feeds Graph's rendering AND the oracle — the
// frozen-snapshot configuration is unrepresentable: there is no second
// status source to capture from.
//
// r[impl dash.graph.auto-stop+2]
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

/** Per-dispatch deadline envelope for the graph poll's unary
 * (merged_bug_076). The epoch-keyed re-entrancy gate serializes the
 * loop on the getBuildGraph await, so an unbounded dispatch would
 * import the transport's worst-case tail — nginx holds quiet upstream
 * reads for 3600s (dashboard-nginx.conf) — into the poll's liveness
 * and into every oracle reading it.
 *
 * Derivation: 15s = 3x the live cadence (the gate's intended
 * slow-fetch serialization tolerates up to two skipped ticks before
 * the dispatch is cancelled), and <= SETTLED_POLL_MS so a settled
 * probe structurally cannot overlap its own cadence. INDEPENDENT of
 * the settling cadence by construction: both the live and the settled
 * dispatch path read this one const — a settled drawer's probe gets
 * the same 15s, never a 30s-scaled envelope. Violable + testable: the
 * ordering law POLL_MS < GRAPH_FETCH_DEADLINE_MS <= SETTLED_POLL_MS is
 * asserted in the test battery. */
export const GRAPH_FETCH_DEADLINE_MS = 15_000;

/** The closed evidence alphabet a fenced fetch outcome classifies
 * into (merged_bug_081). Latch transitions consume EXACTLY this
 * alphabet through `nextTransition` — the un-latch edge structurally
 * cannot accept an input the latch edge excludes. */
export type GraphEvidence =
  | 'settled'
  | 'live'
  | 'empty'
  | 'partial-terminal'
  | 'failed';

/** Machine-derived census (R15): `satisfies` rejects non-members of
 * the alphabet; the identity-mapped pin below rejects omissions — a
 * variant added to `GraphEvidence` without a row fails to compile,
 * and a row's value must itself be a member of EVIDENCE_ALL. The
 * compiler, not the author, certifies the census; the product test
 * iterates these cells. */
export const EVIDENCE_ALL = [
  'settled',
  'live',
  'empty',
  'partial-terminal',
  'failed',
] as const satisfies readonly GraphEvidence[];

const EVIDENCE_PIN: Record<GraphEvidence, (typeof EVIDENCE_ALL)[number]> = {
  settled: 'settled',
  live: 'live',
  empty: 'empty',
  'partial-terminal': 'partial-terminal',
  failed: 'failed',
};
void EVIDENCE_PIN;

/** The structural slice of a fetch outcome the classifier reads
 * (structural, not the branded wire Message type, so production
 * responses and proto-shaped test fixtures both satisfy it). */
export type GraphFetchOutcome = {
  response?: {
    readonly nodes: readonly RawNode[];
    readonly truncated: boolean;
  };
  error?: unknown;
};

/** Classify a fenced fetch outcome into the closed evidence alphabet.
 * Exhaustive decision tree, no wildcard:
 * - `failed`: no response (transport error or deadline);
 * - `empty`: zero nodes — a build not yet populated, or an externally
 *   purged one (no in-tree path deletes builds rows for a merely
 *   terminal build; manual admin cleanup does);
 * - `live`: some visible node non-terminal;
 * - `partial-terminal`: truncated view, all VISIBLE nodes terminal —
 *   insertion-order truncation settles roots first, so visible
 *   terminal says nothing about the tail;
 * - `settled`: complete nonempty view, every node terminal. */
export function classifyResponse(outcome: GraphFetchOutcome): GraphEvidence {
  if (outcome.response === undefined) return 'failed';
  const r = outcome.response;
  if (r.nodes.length === 0) return 'empty';
  if (!r.nodes.every((n) => TERMINAL.has(n.status))) return 'live';
  return r.truncated ? 'partial-terminal' : 'settled';
}

/** Per-cell effects record (R14): every field mandatory, so a new
 * evidence variant cannot compile without taking a position on all
 * four effects. */
export type CellEffects = {
  latch: 'latch' | 'unlatch' | 'hold';
  cadence: 'live' | 'settled' | 'keep';
  data: 'apply' | 'retain';
  errorSurface: 'clear' | 'flag';
};

function assertNever(x: never): never {
  throw new Error(`unreachable evidence variant: ${String(x)}`);
}

/** THE latch transition law (merged_bug_081): one total function over
 * the `latched x evidence` product. fd135a0ab typed the latch arm's
 * guards but shipped the inverse edge bare (`!settled && allTerminal`)
 * — an empty response un-latched a settled drawer into absorbing 5s
 * polling (re-latching needs a NONEMPTY untruncated all-terminal
 * view, which a purged build never serves again) and wiped the
 * retained graph the oracle was reading; a truncated all-terminal
 * probe fell through the same bare edge. Deriving the edge-set from
 * the product makes a bare sibling edge unrepresentable: every cell
 * takes a position on all four effects.
 *
 * The cells, from the evidence law — before settle, absence is the
 * state; after settle, absence cannot retro-erase terminal evidence:
 * - un-latch ONLY on `(latched, live)`: the out-of-band-clear
 *   discovery, preserved.
 * - `(latched, empty)`: hold + retain + keep — the absorbing hole
 *   dies; retention keeps `statusOf` serving the oracle across
 *   purges.
 * - `(latched, partial-terminal | settled | failed)`: hold + keep
 *   (the truncated probe no longer un-latches either — the second
 *   input the bare edge wrongly accepted).
 * - `(unlatched, settled)`: latch + settled cadence.
 * - `(unlatched, empty | partial-terminal | live)`: apply on the live
 *   cadence — empty pre-population is the truthful loading view (the
 *   asymmetry against `(latched, empty)` IS the evidence law).
 *
 * RESPONSE evidence only: `onCleared`/`destroy` are command edges
 * (user action / teardown) that bypass classification by design. */
export function nextTransition(
  latched: boolean,
  ev: GraphEvidence,
): CellEffects {
  switch (ev) {
    case 'settled':
      return latched
        ? { latch: 'hold', cadence: 'keep', data: 'apply', errorSurface: 'clear' }
        : {
            latch: 'latch',
            cadence: 'settled',
            data: 'apply',
            errorSurface: 'clear',
          };
    case 'live':
      return latched
        ? {
            latch: 'unlatch',
            cadence: 'live',
            data: 'apply',
            errorSurface: 'clear',
          }
        : { latch: 'hold', cadence: 'keep', data: 'apply', errorSurface: 'clear' };
    case 'empty':
      return latched
        ? {
            latch: 'hold',
            cadence: 'keep',
            data: 'retain',
            errorSurface: 'clear',
          }
        : { latch: 'hold', cadence: 'keep', data: 'apply', errorSurface: 'clear' };
    case 'partial-terminal':
      return { latch: 'hold', cadence: 'keep', data: 'apply', errorSurface: 'clear' };
    case 'failed':
      return {
        latch: 'hold',
        cadence: 'keep',
        data: 'retain',
        errorSurface: 'flag',
      };
    default:
      return assertNever(ev);
  }
}

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
  /** Never-loaded failure surface: set only while NO graph data is
   * applied (`nodes` empty). Once data has applied, probe failures
   * surface as `degraded` instead — Graph's error-first banner arm
   * therefore truthfully means "nothing ever loaded" and can never
   * replace a rendered graph. */
  readonly error: string | null;
  /** Data-bearing failure (merged_bug_081): the latest probe failed
   * but a previously applied graph is retained. Rendered as a
   * non-replacing staleness note (BuildDrawer); cleared by any
   * non-failed evidence. */
  readonly degraded: string | null;
  /** Every node settled — the poll is DOWNSHIFTED to the settled
   * cadence (never stopped: out-of-band clears must be discovered,
   * see the latch law). */
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
  let degraded = $state<string | null>(null);
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
  // The CURRENT dispatch's abort controller, slot-level so destroy()
  // can cancel the in-flight request (merged_bug_076 — today's
  // teardown bumped the epoch but aborted nothing, leaking the hung
  // request into the proxy's 3600s read window). At most one
  // current-epoch dispatch exists (the gate above); a STALE in-flight
  // dispatch keeps its own controller and dies at its own deadline.
  let inflightCtl: AbortController | null = null;
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
   * write — the latch AND nodes/edges/truncated/totalNodes/error/
   * degraded — lands here, and only when the response's dispatch
   * generation is still current. It holds NO decision logic of its
   * own (merged_bug_081): the outcome is classified into the closed
   * evidence alphabet and the cell's four effects are executed —
   * cadence shifts ride the latch EDGES ('keep' cells re-arm
   * nothing), so a steady settled state never resets its interval. */
  function settle(
    epoch: number,
    outcome: {
      response?: Awaited<ReturnType<typeof admin.getBuildGraph>>;
      error?: unknown;
    },
  ): void {
    if (destroyed || epoch !== pollEpoch) return;
    const ev = classifyResponse(outcome);
    const cell = nextTransition(allTerminal, ev);
    if (cell.latch === 'latch') {
      // Downshift, don't stop: out-of-band clears are discovered
      // within SETTLED_POLL_MS (no immediate shot — this response
      // IS the settled state).
      allTerminal = true;
    } else if (cell.latch === 'unlatch') {
      // The settled probe found live work (an out-of-band clear):
      // un-latch and restore the live cadence.
      allTerminal = false;
    }
    if (cell.cadence === 'settled') {
      startCadence(SETTLED_POLL_MS, false);
    } else if (cell.cadence === 'live') {
      startCadence(POLL_MS, false);
    }
    if (cell.data === 'apply' && outcome.response !== undefined) {
      // The response guard is structurally redundant (only non-failed
      // cells apply, and non-failed evidence implies a response) but
      // keeps the narrowing visible to the compiler.
      const r = outcome.response;
      nodes = r.nodes;
      edges = r.edges;
      truncated = r.truncated;
      totalNodes = r.totalNodes;
    }
    if (cell.errorSurface === 'flag') {
      // `error` is reserved for "nothing loaded" (Graph's banner arm
      // replaces the canvas); data-bearing failures degrade without
      // replacing the retained graph.
      if (nodes.length === 0) {
        error = String(outcome.error);
      } else {
        degraded = String(outcome.error);
      }
    } else {
      error = null;
      degraded = null;
    }
    loading = false;
  }

  async function fetchNow(): Promise<void> {
    if (destroyed) return;
    const epoch = pollEpoch;
    if (inflightEpoch === epoch) return;
    inflightEpoch = epoch;
    // Per-dispatch deadline envelope (merged_bug_076): the owned timer
    // aborts the request at GRAPH_FETCH_DEADLINE_MS — the browser
    // fetch dies, defeating (not bypassing) the proxy's 3600s hold —
    // and the abort-edge race below releases the loop even when a
    // signal-deaf transport/mock never settles (logStream's
    // per-iteration abort edge is the in-repo precedent).
    const ctl = new AbortController();
    inflightCtl = ctl;
    const deadline = setTimeout(() => {
      ctl.abort(
        new Error(
          `getBuildGraph exceeded GRAPH_FETCH_DEADLINE_MS (${GRAPH_FETCH_DEADLINE_MS} ms)`,
        ),
      );
    }, GRAPH_FETCH_DEADLINE_MS);
    let onAbort: (() => void) | undefined;
    try {
      let r;
      try {
        const call = admin.getBuildGraph({ buildId }, { signal: ctl.signal });
        // Orphaned-rejection hygiene: when the abort edge wins the
        // race, the transport's own late rejection must not surface
        // as unhandled (transport.test.ts precedent).
        void call.catch(() => {});
        const abortEdge = new Promise<never>((_, reject) => {
          onAbort = () => reject(ctl.signal.reason);
          ctl.signal.addEventListener('abort', onAbort, { once: true });
        });
        r = await Promise.race([call, abortEdge]);
      } catch (e) {
        settle(epoch, { error: e });
        return;
      }
      settle(epoch, { response: r });
    } finally {
      clearTimeout(deadline);
      if (onAbort !== undefined) {
        ctl.signal.removeEventListener('abort', onAbort);
      }
      if (inflightCtl === ctl) inflightCtl = null;
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
    get degraded() {
      return degraded;
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
      // Abort the in-flight dispatch: a closed drawer must not keep a
      // hung request alive for the proxy's read window. The settle is
      // already fenced out (destroyed + epoch bump), so the abort's
      // rejection is swallowed by the dispatch's own catch.
      inflightCtl?.abort(new Error('poll destroyed'));
      inflightCtl = null;
      stop?.();
      stop = null;
    },
  };
}
