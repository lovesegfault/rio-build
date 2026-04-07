// Table-driven unit tests for the shared BuildInfo formatters extracted
// from Builds.svelte / BuildDrawer.svelte / Executors.svelte. Pure functions,
// no DOM — these pin the 4× duplicated logic down in one place.
import { timestampFromMs } from '@bufbuild/protobuf/wkt';
import { describe, expect, it } from 'vitest';
import {
  fmtDuration,
  fmtTsAbs,
  fmtTsRel,
  progress,
  tsToMs,
} from '../buildInfo';
import type { BuildInfo } from '../../api/types';

// Fixture shim — BuildInfo is a branded `Message<...>` intersection that
// `create(BuildInfoSchema, ...)` would satisfy, but progress/fmtDuration
// only touch the scalar fields. Structural compat via Partial→cast keeps
// the test table terse.
const bi = (p: Partial<BuildInfo>) => p as BuildInfo;

// Reference "now" for deterministic rel/duration outputs. Chosen to be a
// round multiple of 1000ms so timestampFromMs round-trips cleanly
// (no nanos noise at second boundaries).
const NOW = 1_700_000_000_000;
const ts = (msBeforeNow: number) => timestampFromMs(NOW - msBeforeNow);

describe('progress', () => {
  it.each([
    // empty build — guard against NaN from 0/0
    [bi({ totalDerivations: 0, completedDerivations: 0, cachedDerivations: 0 }), 0],
    // cached counts toward done: 5+2 / 10 = 70%
    [bi({ totalDerivations: 10, completedDerivations: 5, cachedDerivations: 2 }), 70],
    // fully completed, no cache
    [bi({ totalDerivations: 3, completedDerivations: 3, cachedDerivations: 0 }), 100],
    // cached + completed can exceed total during the scheduler's
    // count-bump race window — Math.min(100, ...) clamps. Mutation-check:
    // s/min/max/ in progress() → this row fails with 167.
    [bi({ totalDerivations: 3, completedDerivations: 3, cachedDerivations: 2 }), 100],
    // rounding: 1/3 = 33.33… → 33
    [bi({ totalDerivations: 3, completedDerivations: 1, cachedDerivations: 0 }), 33],
  ])('progress(%j) → %d%%', (b, pct) => {
    expect(progress(b)).toBe(pct);
  });
});

describe('tsToMs', () => {
  it('undefined → undefined', () => {
    expect(tsToMs(undefined)).toBeUndefined();
  });

  it('round-trips with timestampFromMs', () => {
    expect(tsToMs(timestampFromMs(NOW))).toBe(NOW);
    // sub-second — bufbuild includes nanos, so this is preserved
    // (Builds.svelte's old hand-roll dropped nanos; standardizing on
    // bufbuild's timestampMs is the extraction's one runtime-visible
    // change, harmless at display granularity).
    expect(tsToMs(timestampFromMs(NOW + 567))).toBe(NOW + 567);
  });
});

describe('fmtTsAbs', () => {
  // %# (row index) not %j — Timestamp's `seconds` field is a BigInt and
  // vitest's it.each title-serializer chokes on it (`Do not know how to
  // serialize a BigInt`). The row index is enough to locate a failure.
  it.each([
    [undefined, '—'],
    [timestampFromMs(0), '1970-01-01T00:00:00.000Z'],
    [timestampFromMs(NOW), '2023-11-14T22:13:20.000Z'],
  ])('row %# → %s', (t, out) => {
    expect(fmtTsAbs(t)).toBe(out);
  });
});

describe('fmtTsRel', () => {
  it.each([
    ['undefined', undefined, '—'],
    ['0ms', ts(0), 'now'],
    ['500ms', ts(500), 'now'], // <1s → "now"
    ['5s', ts(5_000), '5s ago'],
    ['59s', ts(59_000), '59s ago'],
    ['90s', ts(90_000), '1m ago'],
    ['3599s', ts(3_599_000), '59m ago'],
    ['3700s', ts(3_700_000), '1h ago'],
    ['86399s', ts(86_399_000), '23h ago'],
    ['90000s', ts(90_000_000), '1d ago'],
  ])('%s → %s', (_label, t, out) => {
    expect(fmtTsRel(t, NOW)).toBe(out);
  });

  it('uses Date.now() default when now omitted', () => {
    // Can't pin Date.now here without fake timers — smoke-test the
    // shape only. undefined input short-circuits before the clock read.
    expect(fmtTsRel(undefined)).toBe('—');
  });
});

describe('fmtDuration', () => {
  it('— when startedAt absent', () => {
    expect(fmtDuration(bi({}), NOW)).toBe('—');
  });

  it.each([
    // in-flight: finishedAt undefined → end = NOW
    ['15s running', bi({ startedAt: ts(15_000) }), '15s'],
    ['2m30s running', bi({ startedAt: ts(150_000) }), '2m30s'],
    ['1h12m running', bi({ startedAt: ts(4_320_000) }), '1h12m'],
    // completed: finishedAt set → ignores NOW
    [
      '90s completed',
      bi({
        startedAt: timestampFromMs(NOW - 100_000),
        finishedAt: timestampFromMs(NOW - 10_000),
      }),
      '1m30s',
    ],
    [
      '2h completed',
      bi({
        startedAt: timestampFromMs(NOW - 7_800_000),
        finishedAt: timestampFromMs(NOW - 600_000),
      }),
      '2h0m',
    ],
  ])('%s → %s', (_label, b, out) => {
    expect(fmtDuration(b, NOW)).toBe(out);
  });
});
