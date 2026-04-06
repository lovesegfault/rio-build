// r[verify dash.stream.log-tail]
// Structural perf guard: the prior spread-reassign
// (`lines = [...lines, ...decoded]`) copied the entire existing array
// per chunk — O(n²) total for n lines. For 20K lines in 100-line chunks
// that's ~200 chunks × avg 10K copies ≈ 2M ref-copies on top of the
// generator-drive overhead; on CI machines that took ~1500ms.
//
// push() is O(chunk) per update — linear-ish total. We can't measure
// ref-copies directly but the wall-clock gap is ~30× wide, so a
// generous <200ms bound catches a regression back to spread-reassign
// reliably even across 3× CI-machine variance. This is a weak-bound
// structural test, not a microbenchmark — the point is to trip on O(n²)
// coming back, not to measure push() precisely.
//
// The same suite covers the 50K-line truncation cap introduced
// alongside the push() refactor.
import { afterEach, describe, expect, it, vi } from 'vitest';

const { getBuildLogs } = vi.hoisted(() => ({ getBuildLogs: vi.fn() }));
vi.mock('../../api/admin', () => ({ admin: { getBuildLogs } }));

import { createLogStream } from '../logStream.svelte';

// Flush helper mirrors logStream.test.ts: two microtask turns per
// for-await step — one for the generator's resume, one for the loop
// body's state assignment to settle. The perf test drains 200+ chunks
// so we loop many rounds; the flush cost itself is linear and folds
// into the baseline.
async function flush(rounds: number): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await Promise.resolve();
    await Promise.resolve();
  }
}

// Pre-encode once: the perf path we're testing is array accumulation,
// not TextDecoder. A single 4-byte line repeated means decode() is
// trivial and the elapsed time is dominated by the push() vs
// spread-reassign choice.
const enc = new TextEncoder();
const LINE = enc.encode('line');

function chunk(width: number, isComplete = false) {
  const lines: Uint8Array[] = [];
  for (let i = 0; i < width; i++) lines.push(LINE);
  return {
    derivationPath: '/nix/store/perf-test.drv',
    lines,
    firstLineNumber: 0n,
    isComplete,
  };
}

describe('createLogStream perf', () => {
  afterEach(() => {
    getBuildLogs.mockReset();
  });

  it('drains 20K lines in linear-ish time (not O(n²) spread-reassign)', async () => {
    // 200 chunks × 100 lines = 20_000 lines.
    const CHUNKS = 200;
    const WIDTH = 100;
    getBuildLogs.mockImplementation(async function* () {
      for (let i = 0; i < CHUNKS - 1; i++) yield chunk(WIDTH);
      yield chunk(WIDTH, true);
    });

    const start = performance.now();
    const s = createLogStream('build-perf');
    // CHUNKS+2 rounds: one per chunk resume, plus slack for the IIFE
    // entry and the final `done = true` assignment.
    await flush(CHUNKS + 2);
    const elapsed = performance.now() - start;

    expect(s.done).toBe(true);
    expect(s.lines.length).toBe(CHUNKS * WIDTH);
    // Generous upper bound — spread-reassign on the same input took
    // ~1500ms on CI; push() takes ~30-50ms. A 200ms ceiling tolerates
    // slow CI runners without letting the O(n²) regression through.
    expect(elapsed).toBeLessThan(200);
    expect(s.truncated).toBe(false);
  });

  it('truncates at 50K cap, dropping oldest 10K, setting truncated flag', async () => {
    // 550 chunks × 100 lines = 55_000 lines. Crosses the 50K cap once:
    // at chunk 501 (50_100 lines) we drop the oldest 10K, landing at
    // 40_100. Then 49 more chunks bring us to 45_000. We assert the
    // steady-state math, not the exact crossing moment — the important
    // invariants are (a) length stays bounded and (b) truncated flips.
    const CHUNKS = 550;
    const WIDTH = 100;
    getBuildLogs.mockImplementation(async function* () {
      for (let i = 0; i < CHUNKS - 1; i++) yield chunk(WIDTH);
      yield chunk(WIDTH, true);
    });

    const s = createLogStream('build-trunc');
    await flush(CHUNKS + 2);

    expect(s.done).toBe(true);
    expect(s.truncated).toBe(true);
    // After one drop: 50_100 - 10_000 = 40_100, then 49 × 100 = 45_000.
    // The exact value depends on where the cap fires; we assert the
    // bounded range instead of an exact count — the cap's job is to
    // keep memory under control, not to hit a precise number.
    expect(s.lines.length).toBeLessThanOrEqual(50_000);
    expect(s.lines.length).toBeGreaterThan(40_000);
  });

  it('stays under cap with a single oversized chunk', async () => {
    // Edge: one giant chunk that itself exceeds the cap. push(...60K)
    // → length 60K > 50K → splice(0, 10K) → 50K. Truncated flips.
    // A builder dumping a huge final burst (cat of a large file) hits
    // this path — the cap still holds.
    getBuildLogs.mockImplementation(async function* () {
      yield chunk(60_000, true);
    });

    const s = createLogStream('build-burst');
    await flush(3);

    expect(s.done).toBe(true);
    expect(s.truncated).toBe(true);
    expect(s.lines.length).toBe(50_000);
  });
});
