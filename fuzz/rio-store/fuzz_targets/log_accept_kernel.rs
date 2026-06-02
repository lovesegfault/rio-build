#![no_main]

//! Fuzz the accept-gate kernel ([`rio_log_kernel::accept_verdict`])
//! COMPOSED across a session lifetime — the dimension the single-call
//! kani contract (`check_accept_verdict_contract`, full u64 domain, one
//! call) does not prove. A session model (`hw`, first-write-wins
//! `ceiling` — mirroring `set_final_line_count`) is folded through a
//! batch/seal op sequence; every batch is decided twice (determinism)
//! and checked against an independent transcription of
//! `docs/spec/models/logService.qnt::acceptVerdict` plus the overflow
//! arm, then the model advances only on `Accepted` (a rejected batch
//! changes nothing).
//!
//! Sibling targets: `log_dedup_kernel` (the read-path walk +
//! completeness fold at arbitrary N) and `log_chunk_ingest` (the
//! end-to-end store-path oracle through the real `IngestSession`).
//!
//! # Input wire format
//!
//! 5-byte ops `[tag, a, b, c, d]` until the input runs out (a trailing
//! partial op is ignored; at most [`MAX_OPS`] ops are executed) —
//! the e2e target's selector scheme, minus the session/cut plumbing:
//!
//! | `tag >> 2` | op               | fields                                  |
//! |------------|------------------|-----------------------------------------|
//! | 0..=31     | batch, dense     | `first = u16(a,b) % 600`, `count = c % 17` |
//! | 32..=43    | batch, raw       | `first = u16(a,b)`                      |
//! | 44..=49    | batch, i64 edge  | `first = i64::MAX - b + (a & 31)`       |
//! | 50..=53    | batch, u64 edge  | `first = u64::MAX - b`                  |
//! | 54..=63    | seal             | `ceiling = u16(a,b) % 700` (first wins) |

use libfuzzer_sys::fuzz_target;
use rio_log_kernel::{AcceptVerdict, accept_verdict};

/// Hard cap on decoded ops per iteration, bounding per-iteration work
/// regardless of how large libFuzzer grows the input.
const MAX_OPS: usize = 48;

/// Independent transcription of `logService.qnt::acceptVerdict` (gate
/// order: non-monotone, then ceiling) extended with the overflow arm
/// the model's bounded line domain cannot represent. Deliberately a
/// different code shape from the kernel (match-first on
/// representability, no early returns) so a shared mistake is
/// unlikely.
fn model_verdict(hw: u64, ceiling: Option<u64>, first: u64, count: u64) -> AcceptVerdict {
    match first.checked_add(count) {
        None => AcceptVerdict::RejectedOverflow,
        Some(end) if end > i64::MAX as u64 => AcceptVerdict::RejectedOverflow,
        Some(end) => {
            if first < hw {
                AcceptVerdict::RejectedNonMonotone
            } else {
                match ceiling {
                    Some(c) if first >= c => AcceptVerdict::RejectedPastFinal,
                    Some(c) => AcceptVerdict::Accepted { end: end.min(c) },
                    None => AcceptVerdict::Accepted { end },
                }
            }
        }
    }
}

/// One batch through the kernel and the model. Advances `hw` only on
/// accept; asserts the structural bounds with checked arithmetic
/// (debug-assertions are on under `cargo fuzz run`, so any overflow in
/// the harness itself also aborts).
fn step(hw: &mut u64, ceiling: Option<u64>, first: u64, count: u64) {
    let v1 = accept_verdict(*hw, ceiling, first, count);
    let v2 = accept_verdict(*hw, ceiling, first, count);
    assert_eq!(
        v1, v2,
        "determinism: two identical calls disagreed (hw={hw} ceiling={ceiling:?} first={first} count={count})"
    );
    let want = model_verdict(*hw, ceiling, first, count);
    assert_eq!(
        v1, want,
        "model divergence: hw={hw} ceiling={ceiling:?} first={first} count={count}"
    );
    match v1 {
        AcceptVerdict::Accepted { end } => {
            let batch_end = first
                .checked_add(count)
                .expect("an accepted batch's end must be representable");
            assert!(batch_end <= i64::MAX as u64, "accepted past BIGINT");
            assert!(
                first <= end && end <= batch_end,
                "accepted end {end} outside [{first}, {batch_end}]"
            );
            if let Some(c) = ceiling {
                assert!(end <= c, "accepted end {end} past the ceiling {c}");
            }
            // The high-water mark is monotone non-decreasing across the
            // whole run: end >= first >= hw (the non-monotone gate).
            assert!(end >= *hw, "hw regressed: {} -> {end}", *hw);
            *hw = end;
        }
        // A rejected batch changes nothing — the model state is only
        // written on the accept arm.
        AcceptVerdict::RejectedOverflow
        | AcceptVerdict::RejectedNonMonotone
        | AcceptVerdict::RejectedPastFinal => {}
    }
}

fuzz_target!(|data: &[u8]| {
    let mut hw: u64 = 0;
    let mut ceiling: Option<u64> = None;
    for op in data.chunks_exact(5).take(MAX_OPS) {
        let (tag, a, b, c) = (op[0], op[1], op[2], op[3]);
        let raw = u16::from_le_bytes([a, b]) as u64;
        let count = (c % 17) as u64;
        match tag >> 2 {
            // Dense: comparable line numbers so monotone/ceiling
            // interactions are common.
            0..=31 => step(&mut hw, ceiling, raw % 600, count),
            // Raw u16: forward gaps and far jumps.
            32..=43 => step(&mut hw, ceiling, raw, count),
            // Straddle the BIGINT representability ceiling: end lands
            // on either side of i64::MAX.
            44..=49 => step(
                &mut hw,
                ceiling,
                (i64::MAX as u64 - b as u64).wrapping_add((a & 31) as u64),
                count,
            ),
            // Straddle u64::MAX: first + count wraps for small b.
            50..=53 => step(&mut hw, ceiling, u64::MAX - b as u64, count),
            // Seal: first write wins, like set_final_line_count.
            _ => {
                ceiling.get_or_insert(raw % 700);
            }
        }
    }
});
