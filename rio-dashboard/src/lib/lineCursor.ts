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
