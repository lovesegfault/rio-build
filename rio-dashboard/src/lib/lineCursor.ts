// TypeScript mirror of rio-log-kernel's served-stream cursor: one visit
// step per chunk (`visit_chunk`) and the relay exit law (`tail_next`).
//
// THE CONTRACT: this file duplicates rio-log-kernel/src/lib.rs verbatim
// at the decision level — same case split, same range arithmetic, same
// exit law. The shared case table in
// `src/lib/__tests__/lineCursor.test.ts` duplicates the Rust
// `visit_chunk_table` vectors VERBATIM (rio-log-kernel/src/lib.rs
// `mod tests::visit_chunk_table`); change them together. The Rust side
// carries the kani proofs; this side carries the same vectors so a
// semantic drift fails one table or the other.
//
// bigint throughout: line numbers are uint64 on the wire (protobuf-es
// decodes them as bigint) and a number would silently lose precision
// past 2^53. bigint arithmetic is arbitrary-precision so the Rust
// side's saturation guards are unnecessary here — the BIGINT-edge
// vectors in the table pin exactness at i64::MAX.

/// One visit verdict. Discriminated on `kind`; the variants carry the
/// same ranges as the Rust enum:
///
/// - `skip` — the chunk contributes nothing (zero lines, or every line
///   below the watermark). The watermark MUST NOT advance: a zero-line
///   chunk starting past the cursor would otherwise swallow every line
///   between the cursor and its `first_line`.
/// - `serve` — contiguous contribution `[yieldFrom, yieldUntil)`; the
///   leading lines below the watermark (a resent prefix from a
///   reconnect) are deduplicated by starting at the watermark.
/// - `gapThenServe` — the chunk starts strictly past the watermark:
///   `[gapFrom, gapUntil) == [watermark, firstLine)` is missing. The
///   caller decides the disclosure (the dashboard renders a gap row).
export type ChunkVisit =
  | { kind: 'skip'; nextLine: bigint }
  | { kind: 'serve'; yieldFrom: bigint; yieldUntil: bigint; nextLine: bigint }
  | {
      kind: 'gapThenServe';
      gapFrom: bigint;
      gapUntil: bigint;
      yieldFrom: bigint;
      yieldUntil: bigint;
      nextLine: bigint;
    };

// r[impl store.log.tail-reconnect]
// (The resent-prefix MUST-skip lives here: a reconnect re-opens at
// `sinceLine = watermark`, and any overlap the server resends lands in
// the `serve` arm with `yieldFrom == watermark` — lines below it are
// never yielded twice.)
/// One step of the read path's overlap-dedup walk: which of a chunk's
/// `nLines` lines starting at `firstLine` lie at or above the watermark
/// `nextLine`, and where the watermark lands afterwards. Mirrors
/// `rio_log_kernel::visit_chunk` exactly.
export function visitChunk(
  nextLine: bigint,
  firstLine: bigint,
  nLines: bigint,
): ChunkVisit {
  const end = firstLine + nLines;
  // A degenerate zero-line chunk has nothing to contribute, and a chunk
  // whose last line is below the watermark is fully served already.
  // (`end - 1n` is guarded by the `nLines === 0n` arm, mirroring the
  // Rust saturating guard.)
  if (nLines === 0n || end - 1n < nextLine) {
    return { kind: 'skip', nextLine };
  }
  if (firstLine > nextLine) {
    // Forward jump: `[nextLine, firstLine)` is missing. Naming it is
    // the whole point — the caller decides the disclosure.
    return {
      kind: 'gapThenServe',
      gapFrom: nextLine,
      gapUntil: firstLine,
      yieldFrom: firstLine,
      yieldUntil: end,
      nextLine: end,
    };
  }
  return { kind: 'serve', yieldFrom: nextLine, yieldUntil: end, nextLine: end };
}

/// Why one connected `TailLog` stream stopped yielding messages, as the
/// dashboard's reconnect loop observed it. The dashboard has no
/// `gapObserved` cause (the gateway's reopen-once-at-gap discipline):
/// it talks to the store directly and renders the gap row inline
/// instead — `visitChunk`'s `gapThenServe` IS its disclosure.
export type TailStopCause = 'naturalEnd' | 'transportErr' | 'openFailed';

export type TailVerdict = 'reopen' | 'exit';

/// The relay exit law, mirrored from `rio_log_kernel::tail_next`:
/// **exit iff the grace budget is spent, or the stream ended naturally
/// with the derivation terminal and the served log complete.** Every
/// other shape re-opens. "Give up with grace unspent and the log
/// incomplete" is unrepresentable.
export function tailNext(
  cause: TailStopCause,
  terminal: boolean,
  graceExpired: boolean,
  servedComplete: boolean,
): TailVerdict {
  if (graceExpired) return 'exit';
  switch (cause) {
    case 'naturalEnd':
      return terminal && servedComplete ? 'exit' : 'reopen';
    case 'transportErr':
    case 'openFailed':
      return 'reopen';
    default:
      return assertNever(cause);
  }
}

/// Exhaustiveness backstop: a new `ChunkVisit`/`TailStopCause` variant
/// added on the Rust side must be mirrored here — the `never` arm makes
/// the TypeScript compiler the enforcement point (svelte-check runs
/// under `-Dwarnings` parity in the dashboard nix build).
export function assertNever(x: never): never {
  throw new Error(`lineCursor: unreachable variant ${JSON.stringify(x)}`);
}

/// Re-open pacing for the dashboard's follow loop (merged_bug_054).
///
/// The pre-fix loop reset its backoff on `receivedThisAttempt` — ANY
/// chunk receipt, keep-alives and fully-resent chunks included. A
/// store session with no live ingest ends immediately by contract
/// after serving one final (often zero-line) chunk, so every re-open
/// looked "productive" and the loop polled the store at a flat ~4 Hz
/// forever.
///
/// The pacer consumes the [`ChunkVisit`] VERDICT instead of a receipt
/// boolean — "reset on a non-productive receipt" is untypeable:
/// `noteVisit` matches the verdict exhaustively and only the two
/// arms that yield lines (`serve`, `gapThenServe`) reset the delay;
/// `skip` (keep-alive / fully-resent) escalates exactly like a failed
/// open. Ladder: 250 → 500 → 1000 → 2000 ms (cap).
///
/// The gateway relay deliberately keeps a FIXED 1 s reconnect backoff
/// instead of this ladder (log_tail.rs `RECONNECT_BACKOFF`): its
/// subscriptions are bounded per-derivation by the drain signal + the
/// post-terminal grace window, and its per-build fan-out makes a
/// predictable cadence worth more than a lower floor. The dashboard
/// tab has neither bound — a wedged store would otherwise hold every
/// open tab at 4 Hz indefinitely.
// r[impl dash.stream.reopen-pacing]
export const REOPEN_BASE_MS = 250;
export const REOPEN_MAX_MS = 2_000;

export class ReopenPacer {
  #delayMs: number = REOPEN_BASE_MS;

  /// A chunk verdict from the open stream. Productive verdicts reset
  /// the ladder; `skip` is NOT progress.
  noteVisit(visit: ChunkVisit): void {
    switch (visit.kind) {
      case 'serve':
      case 'gapThenServe':
        this.#delayMs = REOPEN_BASE_MS;
        break;
      case 'skip':
        break;
      default:
        assertNever(visit);
    }
  }

  /// The delay to sleep before the NEXT re-open, escalating the ladder
  /// for the one after it. Called once per stream end.
  nextDelayMs(): number {
    const d = this.#delayMs;
    this.#delayMs = Math.min(this.#delayMs * 2, REOPEN_MAX_MS);
    return d;
  }
}
