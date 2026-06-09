// THE SHARED CASE TABLE — these vectors duplicate
// rio-log-kernel/src/lib.rs `mod tests::visit_chunk_table` VERBATIM
// (same inputs, same expected variants/ranges); change them together.
// The Rust side additionally carries the kani contracts; this side pins
// the TypeScript mirror to the same decisions so a semantic drift fails
// one table or the other. The skip rows pin the two skip causes
// (zero-line chunk; chunk entirely below the watermark) leaving the
// watermark untouched; the gap rows pin the forward-jump split,
// including a cursor strictly inside the gap and the BIGINT edge.
import { describe, expect, it } from 'vitest';

import {
  ReopenPacer,
  REOPEN_BASE_MS,
  REOPEN_MAX_MS,
  tailNext,
  visitChunkKeyed,
  visitChunk,
  type ChunkVisit,
  type TailResolution,
  type TailStopCause,
} from '../lineCursor';

const I64_MAX = 9223372036854775807n; // i64::MAX, the manifest BIGINT bound

describe('visitChunk', () => {
  it('matches the rio-log-kernel visit_chunk_table vectors verbatim', () => {
    type Case = [[bigint, bigint, bigint], ChunkVisit];
    const cases: Case[] = [
      // Fresh cursor, chunk at it: contiguous serve.
      [
        [0n, 0n, 4n],
        { kind: 'serve', yieldFrom: 0n, yieldUntil: 4n, nextLine: 4n },
      ],
      // Fresh cursor, chunk strictly ahead: the jump is a gap.
      [
        [0n, 10n, 4n],
        {
          kind: 'gapThenServe',
          gapFrom: 0n,
          gapUntil: 10n,
          yieldFrom: 10n,
          yieldUntil: 14n,
          nextLine: 14n,
        },
      ],
      // Cursor inside the chunk: leading lines deduped, no gap.
      [
        [12n, 10n, 4n],
        { kind: 'serve', yieldFrom: 12n, yieldUntil: 14n, nextLine: 14n },
      ],
      // Cursor strictly inside a forward jump: gap is exactly
      // [cursor, first).
      [
        [47n, 50n, 3n],
        {
          kind: 'gapThenServe',
          gapFrom: 47n,
          gapUntil: 50n,
          yieldFrom: 50n,
          yieldUntil: 53n,
          nextLine: 53n,
        },
      ],
      // One-line jump: minimal non-empty gap.
      [
        [9n, 10n, 1n],
        {
          kind: 'gapThenServe',
          gapFrom: 9n,
          gapUntil: 10n,
          yieldFrom: 10n,
          yieldUntil: 11n,
          nextLine: 11n,
        },
      ],
      // Chunk entirely below the watermark: skipped, no advance.
      [[20n, 10n, 4n], { kind: 'skip', nextLine: 20n }],
      // Last line exactly at the watermark boundary: 10..14 with
      // cursor 14 is fully served already.
      [[14n, 10n, 4n], { kind: 'skip', nextLine: 14n }],
      // Zero-line chunk past the cursor must NOT advance the watermark
      // (the doc-comment swallow hazard) — and must NOT report a gap
      // either: it holds no lines to serve after one.
      [[3n, 50n, 0n], { kind: 'skip', nextLine: 3n }],
      // BIGINT-edge chunk reached by a gap: exact arithmetic at the
      // precondition boundary (first + n == i64::MAX is the largest
      // representable end here).
      [
        [0n, I64_MAX - 3n, 3n],
        {
          kind: 'gapThenServe',
          gapFrom: 0n,
          gapUntil: I64_MAX - 3n,
          yieldFrom: I64_MAX - 3n,
          yieldUntil: I64_MAX,
          nextLine: I64_MAX,
        },
      ],
      // BIGINT-edge contiguous serve.
      [
        [I64_MAX - 3n, I64_MAX - 3n, 3n],
        {
          kind: 'serve',
          yieldFrom: I64_MAX - 3n,
          yieldUntil: I64_MAX,
          nextLine: I64_MAX,
        },
      ],
    ];
    for (const [[cursor, first, n], expected] of cases) {
      expect(visitChunk(cursor, first, n), `visitChunk(${cursor}, ${first}, ${n})`).toEqual(
        expected,
      );
    }
  });
});

describe('tailNext', () => {
  it('matches the rio-log-kernel tail_next decision table (plus the dashboard mode axis)', () => {
    // Exit cells: every grace_expired row, plus exactly
    // (naturalEnd, !grace_expired, served_complete, terminal ∨ pinned).
    // The mode axis is dashboard-only (bug_348, same precedent as
    // authRequired): a PINNED stream's stamped completion is terminal
    // by construction — re-opening resends the pinned execId and can
    // never resolve a retry. The table is exhaustive over all five
    // inputs so the law stays total by inspection.
    const causes: TailStopCause[] = ['naturalEnd', 'transportErr', 'openFailed'];
    const modes: TailResolution[] = ['latest', 'pinned'];
    for (const cause of causes) {
      for (const mode of modes) {
        for (const terminal of [false, true]) {
          for (const graceExpired of [false, true]) {
            for (const servedComplete of [false, true]) {
              const got = tailNext(cause, mode, terminal, graceExpired, servedComplete);
              const want =
                graceExpired ||
                (cause === 'naturalEnd' &&
                  servedComplete &&
                  (terminal || mode === 'pinned'))
                  ? 'exit'
                  : 'reopen';
              expect(
                got,
                `tailNext(${cause}, ${mode}, ${terminal}, ${graceExpired}, ${servedComplete})`,
              ).toBe(want);
            }
          }
        }
      }
    }
  });

  // r[verify store.log.consumer-registry]
  it('authRequired exits every cell — auth deny does not heal by retry', () => {
    for (const mode of ['latest', 'pinned'] as const) {
      for (const terminal of [false, true]) {
        for (const graceExpired of [false, true]) {
          for (const servedComplete of [false, true]) {
            expect(
              tailNext('authRequired', mode, terminal, graceExpired, servedComplete),
              `tailNext(authRequired, ${mode}, ${terminal}, ${graceExpired}, ${servedComplete})`,
            ).toBe('exit');
          }
        }
      }
    }
  });

  // r[verify dash.stream.log-tail+6]
  it('permanentErr exits every cell — the store said never, retry cannot heal it', () => {
    for (const mode of ['latest', 'pinned'] as const) {
      for (const terminal of [false, true]) {
        for (const graceExpired of [false, true]) {
          for (const servedComplete of [false, true]) {
            expect(
              tailNext('permanentErr', mode, terminal, graceExpired, servedComplete),
              `tailNext(permanentErr, ${mode}, ${terminal}, ${graceExpired}, ${servedComplete})`,
            ).toBe('exit');
          }
        }
      }
    }
  });
});

describe('visitChunkKeyed', () => {
  // Mirrors rio-log-kernel::visit_chunk_keyed: the execution axis is
  // decided BEFORE the line axis, and a matched key delegates verbatim.
  it('keys mismatch => execSwitch, no line verdict', () => {
    expect(visitChunkKeyed(false, 5n, 0n, 3n)).toEqual({ kind: 'execSwitch' });
    // Even a chunk that would be a clean serve under the cursor.
    expect(visitChunkKeyed(false, 0n, 0n, 3n)).toEqual({ kind: 'execSwitch' });
  });

  it('keys match => exactly visitChunk, wrapped', () => {
    for (const [cursor, first, n] of [
      [0n, 0n, 3n],
      [5n, 0n, 3n],
      [2n, 10n, 4n],
      [0n, 0n, 0n],
    ] as const) {
      expect(visitChunkKeyed(true, cursor, first, n)).toEqual({
        kind: 'visit',
        visit: visitChunk(cursor, first, n),
      });
    }
  });
});

// r[verify dash.stream.reopen-pacing]
describe('ReopenPacer', () => {
  it('escalates 250 -> 500 -> 1000 -> 2000 and caps, absent productive visits', () => {
    const p = new ReopenPacer();
    expect(p.nextDelayMs()).toBe(REOPEN_BASE_MS);
    expect(p.nextDelayMs()).toBe(500);
    expect(p.nextDelayMs()).toBe(1000);
    expect(p.nextDelayMs()).toBe(REOPEN_MAX_MS);
    expect(p.nextDelayMs()).toBe(REOPEN_MAX_MS);
  });

  it('does NOT reset on skip — receipt is not progress (merged_bug_054)', () => {
    const p = new ReopenPacer();
    p.nextDelayMs(); // 250 consumed, ladder at 500
    p.noteVisit({ kind: 'skip', nextLine: 0n });
    expect(p.nextDelayMs()).toBe(500);
    p.noteVisit({ kind: 'skip', nextLine: 0n });
    expect(p.nextDelayMs()).toBe(1000);
  });

  it('resets only on serve / gapThenServe', () => {
    for (const visit of [
      { kind: 'serve', yieldFrom: 0n, yieldUntil: 1n, nextLine: 1n } as const,
      {
        kind: 'gapThenServe',
        gapFrom: 0n,
        gapUntil: 2n,
        yieldFrom: 2n,
        yieldUntil: 3n,
        nextLine: 3n,
      } as const,
    ]) {
      const p = new ReopenPacer();
      p.nextDelayMs();
      p.nextDelayMs(); // ladder at 1000
      p.noteVisit(visit);
      expect(p.nextDelayMs(), `reset on ${visit.kind}`).toBe(REOPEN_BASE_MS);
    }
  });
});
