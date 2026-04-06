// r[verify dash.graph.degrade-threshold]
// Pure-function layout tests: dagre runs under jsdom (no DOM needed),
// and the degrade guard is a simple length check. WebWorker path is NOT
// exercised here — jsdom's Worker stub doesn't support module-type
// workers, and the worker body is the same runLayout() we test directly.
// @types/node is not a devDep — adding it would churn pnpm-lock.yaml →
// dashboard.nix hash bump for a test-only import. vitest's node env
// resolves node:fs/node:path at runtime; only svelte-check's tsc pass
// lacks the ambient types. ts-ignore (not ts-expect-error) because
// vite's transform DOES resolve these, so expect-error would fail as
// "unused directive" under vitest.
/* eslint-disable @typescript-eslint/ban-ts-comment */
// @ts-ignore — see block comment above
import { readFileSync } from 'node:fs';
// @ts-ignore — see block comment above
import { resolve } from 'node:path';
/* eslint-enable @typescript-eslint/ban-ts-comment */
import { describe, expect, it } from 'vitest';
import {
  DEGRADE_THRESHOLD,
  DERIVATION_STATUSES,
  SORT_RANK,
  TERMINAL,
  WORKER_THRESHOLD,
  hashPrefix,
  layoutGraph,
  runLayout,
  sortForTable,
  statusClass,
  type RawEdge,
  type RawNode,
} from '../graphLayout';

// Cross-language golden: the canonical {as_str, is_terminal} set
// emitted by rio-scheduler/src/state/derivation.rs. Same file the Rust
// nextest include_str!'s — both sides compare against ONE source of
// truth. readFileSync+JSON.parse (not `import ... from '*.json'`)
// because the golden lives outside rio-dashboard/ — tsconfig's
// `include` scope doesn't reach it, and vite's import-analysis would
// reject the cross-package path. fs resolves it regardless. Path is
// cwd-relative: vitest runs from the package root (rio-dashboard/)
// both locally (`pnpm run test`) and in the nix sandbox (dashboard.nix
// preBuild copies the golden to ../rio-test-support/golden/ so this
// same path works).
type GoldenRow = { status: string; terminal: boolean };
const goldenPath = resolve(
  '..',
  'rio-test-support',
  'golden',
  'derivation_statuses.json',
);
const goldenStatuses: GoldenRow[] = JSON.parse(
  readFileSync(goldenPath, 'utf8'),
);

function mkNode(over: Partial<RawNode> = {}): RawNode {
  return {
    drvPath: '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test.drv',
    pname: 'test',
    system: 'x86_64-linux',
    status: 'completed',
    assignedExecutorId: '',
    ...over,
  };
}

describe('graphLayout', () => {
  it('thresholds are ordered: worker < degrade (spec says 500 < 2000)', () => {
    // Not just "both defined" — if someone swaps them the worker path
    // would never fire (degrade guard trips first). The spec text pins
    // both numbers.
    expect(WORKER_THRESHOLD).toBe(500);
    expect(DEGRADE_THRESHOLD).toBe(2000);
    expect(WORKER_THRESHOLD).toBeLessThan(DEGRADE_THRESHOLD);
  });

  it('lays out 3 nodes / 2 edges with non-zero, distinct positions', () => {
    // Chain: a → b → c (build order). dagre should rank them vertically
    // under rankdir:TB, so the y-coordinates monotone-increase.
    const gn: RawNode[] = [
      mkNode({ drvPath: 'a', pname: 'pkg-a', status: 'completed' }),
      mkNode({ drvPath: 'b', pname: 'pkg-b', status: 'running' }),
      mkNode({ drvPath: 'c', pname: 'pkg-c', status: 'queued' }),
    ];
    const ge: RawEdge[] = [
      { childDrvPath: 'a', parentDrvPath: 'b' },
      { childDrvPath: 'b', parentDrvPath: 'c' },
    ];

    const r = layoutGraph(gn, ge);
    expect(r.degraded).toBe(false);
    if (r.degraded) throw new Error('unreachable');

    expect(r.nodes).toHaveLength(3);
    expect(r.edges).toHaveLength(2);

    const by = new Map(r.nodes.map((n) => [n.id, n]));
    const a = by.get('a')!;
    const b = by.get('b')!;
    const c = by.get('c')!;

    // y monotone — a (source) at top, c (sink) at bottom. Dagre's exact
    // pixel values depend on internal spacing; we only care about the
    // ordering invariant.
    expect(a.position.y).toBeLessThan(b.position.y);
    expect(b.position.y).toBeLessThan(c.position.y);

    // All nodes got the custom type so xyflow routes them to DrvNode.
    for (const n of r.nodes) expect(n.type).toBe('drvNode');

    // Status class landed on the outer class string.
    expect(a.class).toBe('drv-green');
    expect(b.class).toBe('drv-yellow');
    expect(c.class).toBe('drv-gray');

    // Edges wire child→parent (build order = source→target in xyflow).
    expect(r.edges[0]).toMatchObject({ source: 'a', target: 'b' });
    expect(r.edges[1]).toMatchObject({ source: 'b', target: 'c' });
  });

  it(`degrades above ${DEGRADE_THRESHOLD} nodes`, () => {
    // DEGRADE_THRESHOLD + 1 empty nodes — we don't even try dagre.
    // Reason string mentions both counts so the user understands why.
    const gn: RawNode[] = Array.from({ length: DEGRADE_THRESHOLD + 1 }, (_, i) =>
      mkNode({ drvPath: `drv-${i}`, pname: `pkg-${i}` }),
    );
    const r = layoutGraph(gn, []);
    expect(r.degraded).toBe(true);
    if (!r.degraded) throw new Error('unreachable');
    expect(r.reason).toContain(`${DEGRADE_THRESHOLD + 1}`);
    expect(r.reason).toContain(`${DEGRADE_THRESHOLD}`);
    // Degraded result carries the nodes for the table.
    expect(r.nodes).toHaveLength(DEGRADE_THRESHOLD + 1);
  });

  it('does NOT degrade at exactly the threshold', () => {
    // Boundary: ≤2000 is interactive, >2000 is table. If this flips the
    // large-build UX changes silently.
    const gn: RawNode[] = Array.from({ length: DEGRADE_THRESHOLD }, (_, i) =>
      mkNode({ drvPath: `drv-${i}` }),
    );
    // Skip dagre (2000 nodes is slow under vitest) by calling with no
    // edges — dagre still assigns positions but the rank phase is O(n)
    // instead of O(n·e).
    const r = layoutGraph(gn, []);
    expect(r.degraded).toBe(false);
  });

  it('statusClass maps scheduler DerivationStatus strings', () => {
    // Mirror rio-scheduler/src/state/derivation.rs as_str() — if a new
    // status lands there without a mapping here, it defaults to gray
    // (safe but invisible). This test pins the full current set.
    const green = ['completed', 'skipped'];
    const yellow = ['running', 'assigned'];
    const red = ['failed', 'poisoned', 'dependency_failed'];
    const gray = ['created', 'queued', 'ready', 'cancelled'];

    for (const s of green) expect(statusClass(s)).toBe('green');
    for (const s of yellow) expect(statusClass(s)).toBe('yellow');
    for (const s of red) expect(statusClass(s)).toBe('red');
    for (const s of gray) expect(statusClass(s)).toBe('gray');

    // Unknown status → gray, never throw.
    expect(statusClass('new-state-from-future')).toBe('gray');
  });

  it('STATUS_CLASS is exhaustive over DERIVATION_STATUSES', () => {
    // Enumerate-and-assert-membership, same pattern as
    // rio-scheduler/tests/metrics_registered.rs. DERIVATION_STATUSES is
    // the authoritative string list (mirrors derivation.rs as_str()) —
    // GraphNode.status is a plain proto string, not an enum, so there's
    // no generated type to iterate. If a new variant lands in
    // derivation.rs without a DERIVATION_STATUSES entry this test can't
    // catch it (cross-language gap); but if DERIVATION_STATUSES grows
    // without a STATUS_CLASS arm, the gray-fallthrough would be silent
    // and THIS test flags it.
    //
    // "new-state-from-future" is the sentinel: if it ever stops
    // returning gray, STATUS_CLASS acquired a wildcard/default that
    // would mask the exhaustiveness check.
    const sentinel = statusClass('new-state-from-future');
    expect(sentinel).toBe('gray');
    for (const s of DERIVATION_STATUSES) {
      // A mapped status either has a non-gray class OR is one of the
      // four genuine grays. "Gray by omission" is the failure mode.
      const cls = statusClass(s);
      const deliberatelyGray = new Set(['created', 'queued', 'ready', 'cancelled']);
      if (cls === 'gray') {
        expect(deliberatelyGray.has(s), `${s} fell through to default gray — add to STATUS_CLASS`).toBe(true);
      }
    }
    // The four colour buckets from the prior test should together cover
    // the full DERIVATION_STATUSES list (set equality, order-agnostic).
    const covered = new Set([
      ...['completed', 'skipped'],
      ...['running', 'assigned'],
      ...['failed', 'poisoned', 'dependency_failed'],
      ...['created', 'queued', 'ready', 'cancelled'],
    ]);
    expect(covered).toEqual(new Set(DERIVATION_STATUSES));
  });

  it('TERMINAL mirrors scheduler is_terminal()', () => {
    // Cross-check against derivation.rs: is_terminal() matches
    // Completed|Poisoned|DependencyFailed|Cancelled|Skipped. NOT
    // Failed — failed is retriable (failed→ready until poison
    // threshold), so the poll must keep running until the scheduler
    // either succeeds the retry or escalates to poisoned.
    expect(TERMINAL).toEqual(
      new Set(['completed', 'skipped', 'poisoned', 'dependency_failed', 'cancelled']),
    );
    // Every TERMINAL member is a known status (no typos).
    for (const s of TERMINAL) {
      expect(DERIVATION_STATUSES).toContain(s);
    }
    // Inverse: running/assigned/failed are NOT terminal.
    for (const s of ['running', 'assigned', 'failed', 'queued', 'ready', 'created']) {
      expect(TERMINAL.has(s)).toBe(false);
    }
  });

  it('hashPrefix extracts the 8-char store-path hash', () => {
    expect(
      hashPrefix('/nix/store/0123456789abcdefghijklmnopqrstuv-hello.drv'),
    ).toBe('01234567');
    // Non-conforming input (test fixtures, mocks) → first-8 fallback.
    expect(hashPrefix('not-a-store-path')).toBe('not-a-st');
  });

  it('sortForTable floats failures to the top', () => {
    const nodes: RawNode[] = [
      mkNode({ drvPath: 'a', pname: 'zzz', status: 'completed' }),
      mkNode({ drvPath: 'b', pname: 'aaa', status: 'poisoned' }),
      mkNode({ drvPath: 'c', pname: 'mmm', status: 'running' }),
      mkNode({ drvPath: 'd', pname: 'bbb', status: 'failed' }),
      mkNode({ drvPath: 'e', pname: 'kkk', status: 'queued' }),
    ];
    const sorted = sortForTable(nodes).map((n) => n.drvPath);
    // failed + poisoned first (pname tie-break: aaa before bbb), then
    // running, then queued, then completed.
    expect(sorted).toEqual(['b', 'd', 'c', 'e', 'a']);
  });

  it('runLayout returns a position for every input node', () => {
    // Worker-path sanity: the map the worker posts back must cover
    // every drvPath or toXyflow() falls back to {0,0} and the graph
    // collapses into a single pile.
    const gn: RawNode[] = [
      mkNode({ drvPath: 'x' }),
      mkNode({ drvPath: 'y' }),
      mkNode({ drvPath: 'z' }),
    ];
    const pos = runLayout(gn, [{ childDrvPath: 'x', parentDrvPath: 'y' }]);
    expect(pos.size).toBe(3);
    for (const n of gn) {
      const p = pos.get(n.drvPath);
      expect(p).toBeDefined();
      expect(typeof p!.x).toBe('number');
      expect(typeof p!.y).toBe('number');
    }
  });
});

// r[verify sched.state.transitions]
// Cross-language enforcement: rio-scheduler's DerivationStatus::as_str()
// set (and the is_terminal() predicate) emitted as a golden JSON that
// BOTH the Rust nextest (derivation.rs status_snapshot mod) and this
// vitest describe compare against. The P0400 regression (Skipped added
// to Rust, silently fell through to gray in STATUS_CLASS) is the
// motivating case — a 12th variant added to Rust without a STATUS_CLASS
// arm now breaks THIS test, not just looks wrong on screen.
describe('cross-language status enforcement', () => {
  it('golden is well-formed: 11 rows, all statuses distinct', () => {
    // Precondition for every test below. If the golden is malformed
    // (e.g., a merge conflict left a duplicate row), the downstream
    // assertions would either under- or over-count. This pins the
    // shape: exactly the 11 statuses, no dupes, terminal is a bool.
    expect(goldenStatuses).toHaveLength(11);
    const names = new Set(goldenStatuses.map((r) => r.status));
    expect(names.size).toBe(11);
    for (const r of goldenStatuses) {
      expect(typeof r.status).toBe('string');
      expect(typeof r.terminal).toBe('boolean');
    }
  });

  it('DERIVATION_STATUSES mirrors the golden (TS-side hand-list is current)', () => {
    // DERIVATION_STATUSES is the TS-side hand-maintained list that the
    // exhaustiveness test above iterates. If it drifts from the golden
    // (Rust-emitted), that test becomes a no-op for the missing status.
    // Set equality, order-agnostic.
    expect(new Set(DERIVATION_STATUSES)).toEqual(
      new Set(goldenStatuses.map((r) => r.status)),
    );
  });

  it('STATUS_CLASS covers every Rust DerivationStatus::as_str (no gray fall-through)', () => {
    // statusClass(s) returns 'gray' for unknown — a new Rust variant
    // without a STATUS_CLASS arm silently renders gray instead of
    // throwing. We can't distinguish "intentionally gray" from "fell
    // through to default", so we pin the INTENDED classification set
    // and assert every golden status is a member. A 12th variant lands
    // in the golden (via the Rust snapshot test forcing a golden
    // update) but NOT in this set → this test fails.
    const intended = new Set([
      'completed', 'skipped',                     // green
      'running', 'assigned',                      // yellow
      'failed', 'poisoned', 'dependency_failed',  // red
      'created', 'queued', 'ready', 'cancelled',  // gray (deliberate)
    ]);
    for (const { status } of goldenStatuses) {
      expect(
        intended.has(status),
        `Rust emitted "${status}" but STATUS_CLASS has no explicit arm ` +
          `— it will fall through to default gray. Add to ` +
          `rio-dashboard/src/lib/graphLayout.ts STATUS_CLASS and to ` +
          `the intended-set here.`,
      ).toBe(true);
    }
    // Inverse: intended-set shouldn't have members the golden doesn't
    // (would mean a removed Rust variant still listed here — stale).
    const goldenNames = new Set(goldenStatuses.map((r) => r.status));
    for (const s of intended) {
      expect(
        goldenNames.has(s),
        `"${s}" is in the intended-classification set but NOT in the ` +
          `golden — stale entry? If Rust removed this variant, drop it ` +
          `from STATUS_CLASS + this intended-set.`,
      ).toBe(true);
    }
  });

  it('SORT_RANK covers every golden status (no fall-through to rank-5)', () => {
    // sortForTable uses `SORT_RANK[s] ?? 5` — an unmapped status sorts
    // to the bottom (rank-5). There IS no intentional rank-5 in the
    // current design; every status has an explicit rank 0-4. A 12th
    // variant without a SORT_RANK entry silently sorts last. SORT_RANK
    // is exported specifically so this test can assert `s in SORT_RANK`
    // (membership, not "returns a number" — the ?? fallback ALWAYS
    // returns a number).
    for (const { status } of goldenStatuses) {
      expect(
        status in SORT_RANK,
        `Rust emitted "${status}" but SORT_RANK has no entry — it will ` +
          `fall through to rank-5 (bottom of the degraded table). Add ` +
          `to rio-dashboard/src/lib/graphLayout.ts SORT_RANK.`,
      ).toBe(true);
    }
    // Inverse: no stale SORT_RANK entries for removed variants.
    const goldenNames = new Set(goldenStatuses.map((r) => r.status));
    for (const s of Object.keys(SORT_RANK)) {
      expect(
        goldenNames.has(s),
        `SORT_RANK has an entry for "${s}" but the golden doesn't — ` +
          `stale entry from a removed Rust variant?`,
      ).toBe(true);
    }
  });

  it('TERMINAL matches Rust is_terminal() via golden', () => {
    // TERMINAL is a predicate mirror of DerivationStatus::is_terminal()
    // — NOT derivable from the status-string list alone. Graph.svelte
    // uses it to stop polling once every node is terminal. If Rust
    // reclassifies (e.g., Failed becomes terminal, or a new Quarantined
    // is terminal), the golden's `terminal: bool` field captures it and
    // this test flags the drift.
    for (const { status, terminal } of goldenStatuses) {
      expect(
        TERMINAL.has(status),
        `TERMINAL drift: Rust says "${status}".is_terminal()=${terminal}, ` +
          `TS TERMINAL.has("${status}")=${TERMINAL.has(status)}. ` +
          `Update rio-dashboard/src/lib/graphLayout.ts TERMINAL set.`,
      ).toBe(terminal);
    }
    // Inverse: TERMINAL shouldn't have members the golden doesn't list
    // (typo detection — already covered by the existing "Every TERMINAL
    // member is a known status" test above, but that compares against
    // DERIVATION_STATUSES which is also TS-side hand-maintained. This
    // compares against the Rust-emitted golden: strictly tighter).
    const goldenNames = new Set(goldenStatuses.map((r) => r.status));
    for (const s of TERMINAL) {
      expect(
        goldenNames.has(s),
        `TERMINAL contains "${s}" which Rust does not emit — typo or ` +
          `stale entry.`,
      ).toBe(true);
    }
  });
});
