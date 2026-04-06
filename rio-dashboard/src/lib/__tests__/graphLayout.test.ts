// r[verify dash.graph.degrade-threshold]
// Pure-function layout tests: dagre runs under jsdom (no DOM needed),
// and the degrade guard is a simple length check. WebWorker path is NOT
// exercised here — jsdom's Worker stub doesn't support module-type
// workers, and the worker body is the same runLayout() we test directly.
import { describe, expect, it } from 'vitest';
import {
  DEGRADE_THRESHOLD,
  WORKER_THRESHOLD,
  hashPrefix,
  layoutGraph,
  runLayout,
  sortForTable,
  statusClass,
  type RawEdge,
  type RawNode,
} from '../graphLayout';

function mkNode(over: Partial<RawNode> = {}): RawNode {
  return {
    drvPath: '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test.drv',
    pname: 'test',
    system: 'x86_64-linux',
    status: 'completed',
    assignedWorkerId: '',
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
