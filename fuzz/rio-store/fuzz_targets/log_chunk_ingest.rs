#![no_main]

//! Fuzz `LogService`'s ingest-accept gates and the read-path overlap
//! dedup end to end: arbitrary chunk sequences — line ranges, counts,
//! session identities, interleavings across two sessions and two
//! executions — flow through `IngestSession::accept` (monotone
//! numbering, i64/u64 overflow, the completeness ceiling, line
//! truncation) and back out through `read_chunk` + `LineCursor` (the
//! `TailLog` ordered-walk dedup). No PostgreSQL, no S3, no tokio
//! runtime.
//!
//! The async seams (`LogChunkStore::put`/`get` inside `read_chunk`) are
//! resolved against `MemoryLogChunkStore`, whose futures never suspend —
//! [`now_or_never`] polls them once and panics if one returns `Pending`.
//!
//! The decision kernels live in the dependency-free `rio-log-kernel`
//! crate (re-exported as `rio_store::logs::kernel`) and are fuzzed
//! DIRECTLY by the sibling targets `log_accept_kernel` (the accept
//! gate composed across a session lifetime) and `log_dedup_kernel`
//! (the chunk walk + completeness fold at arbitrary manifest length);
//! THIS harness remains the end-to-end integration oracle through the
//! real `IngestSession::accept`/`read_chunk`/`LineCursor` store paths.
//! Of the once-replayed PG-coupled contracts: the cutter replay is
//! gone — `SessionHarness::cut` calls
//! `rio_log_kernel::contiguous_prefix_len` for run discovery instead
//! of reimplementing the loop — while the `(first_line, session_id)`
//! sort below deliberately mirrors `read_manifest_range`'s `ORDER BY`,
//! an SQL contract a hermetic harness must replay by construction.
//! (The kernel targets share one corpus tree and lockfile with this
//! one rather than a separate fuzz workspace: any kernel edit rebuilds
//! this workspace's shared member drv anyway, so a third crate2nix
//! instantiation would buy no rebuild isolation.)
//!
//! Properties (`docs/spec/models/logService.qnt` names in parens):
//! - **No panic, no arithmetic overflow** anywhere in the accept or
//!   read path (overflow aborts under `cargo fuzz run`'s default
//!   debug-assertions; under the release CI build a wrap surfaces as a
//!   violation of one of the structural assertions below).
//! - **A rejected batch changes nothing**: the model's buffer grows
//!   only on accepts, and the session's real buffer must equal the
//!   model's at the end of the run.
//! - **The accept gates match the documented contract**
//!   (`completenessGate`, `store.log.ingest-bounds`): the predicted
//!   outcome (empty / overflow / non-monotone / past-final /
//!   accepted-with-truncation) equals the real one for every batch.
//! - **The deduplicated read never serves a line twice, and the served
//!   span never exceeds the accepted span** (`servedSpanExact`): the
//!   ordered walk over the chunked accepted lines — including
//!   overlapping chunks from two sessions — yields exactly the accepted
//!   line set at or above `since_line`, each line once, in increasing
//!   order, with bytes some session actually accepted for that line.
//!
//! # Input wire format
//!
//! `[since_sel, cap_sel]` then 5-byte ops `[tag, a, b, c, d]` until the
//! input runs out (a trailing partial op is ignored; at most
//! [`MAX_OPS`] ops are executed).
//!
//! - `since_line = since_sel * 3` (0..=765 — comparable to the dense
//!   line numbers so it can land before, inside, or past the log)
//! - `per_exec_byte_cap = 2048 + cap_sel * 256` (small enough that the
//!   stream-fatal byte-cap path is reachable with tiny lines)
//! - `tag & 0b11` selects the session (`exec` = bit 1, `sess` = bit 0);
//!   `tag >> 2` selects the op:
//!
//! | `tag >> 2` | op               | fields                                            |
//! |------------|------------------|---------------------------------------------------|
//! | 0..=31     | append, dense    | `first = u16(a,b) % 600`, `n = c % 17`, `len = d % 9` |
//! | 32..=43    | append, raw      | `first = u16(a,b)`, `n`/`len` as above            |
//! | 44..=49    | append, i64 edge | `first = i64::MAX - b + (a & 31)`                 |
//! | 50..=53    | append, u64 edge | `first = u64::MAX - b`                            |
//! | 54..=57    | seal             | `final_line_count = u16(a,b) % 700`               |
//! | 58..=60    | cut              | drain the session's pending lines into chunks     |
//! | 61..=63    | empty chunk      | inject a `line_count = 0` manifest row at `u16(a,b)` |

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use rio_proto::types::BuildLogBatch;
use rio_store::logs::chunks::{
    LogChunkStore, MemoryLogChunkStore, PutOutcome, compress_lines, log_chunk_key,
};
use rio_store::logs::ingest::{AcceptOutcome, IngestConfig, IngestSession, MAX_LINE_LEN};
use rio_store::logs::tail::{ChunkRef, LineCursor, read_chunk};
use uuid::Uuid;

/// Hard cap on decoded ops per iteration, bounding per-iteration work
/// regardless of how large libFuzzer grows the input.
const MAX_OPS: usize = 48;

/// The 32-char `drv_log_hash()` form used as the chunk-key prefix.
/// Both executions share it (two attempts at the same derivation — the
/// realistic overlap case); the key stays unique through the embedded
/// exec/session UUIDs.
const DRV_HASH: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm";

/// Drive a future that must complete without suspending. Every await in
/// the harness resolves against [`MemoryLogChunkStore`] (a mutex over a
/// `HashMap`), so a `Pending` here means the harness accidentally
/// pulled in real I/O — abort rather than spin.
fn now_or_never<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("in-memory log-chunk store futures must never suspend"),
    }
}

/// Deterministic line content: a session tag byte followed by the line
/// number's little-endian bytes, truncated/extended to `len`. Two
/// sessions' copies of the same line differ in byte 0 (when `len > 0`),
/// so the read-back check can tell which session's copy was served.
/// The generated bytes deliberately include `0x0A` (any line number
/// with a `\n` byte in its little-endian form): line content is
/// arbitrary worker-supplied bytes and the chunk codec must round-trip
/// the delimiter byte like any other — `seed-crash-embedded-newline`
/// pins the regression.
fn line_content(exec: usize, sess: usize, line_no: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| match j {
            0 => 0xA0 | ((exec as u8) << 1) | (sess as u8),
            _ => (line_no >> (((j - 1) % 8) * 8)) as u8,
        })
        .collect()
}

/// The model's prediction for one `accept` call, derived from the
/// documented contract (the gate order in `IngestSession::accept`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Predicted {
    /// `lines.is_empty()`: accepted before any gate runs, nothing
    /// buffered, the high-water mark untouched.
    AcceptEmpty,
    /// Buffered. `kept` is the post-ceiling line count; `end` is the
    /// new high-water mark (`min(first + n, ceiling)`).
    Accept {
        kept: usize,
        end: u64,
    },
    Overflow,
    NonMonotone,
    PastFinal,
}

/// One session's harness state: the real `IngestSession` plus the
/// model that predicts and mirrors it.
struct SessionHarness {
    exec: usize,
    sess: usize,
    session: IngestSession,
    /// The model's copy of the accepted buffer. The real buffer must
    /// equal this at the end of the run.
    expected: Vec<(u64, Vec<u8>)>,
    /// Index into `expected` of the first line not yet formed into a
    /// chunk by a `cut` op.
    chunked_to: usize,
    /// The model's high-water mark (one past the last accepted line).
    high_water: u64,
    /// The model's first-write-wins completeness ceiling.
    ceiling: Option<u64>,
    /// Next chunk seq for this session's keys.
    next_seq: u32,
}

impl SessionHarness {
    fn new(exec: usize, sess: usize, cap: u64) -> Self {
        // Deterministic, ordered UUIDs: the (first_line, session_id)
        // read-path sort tiebreak must be reproducible across runs.
        let exec_id = Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0001 + exec as u128);
        let session_id = Uuid::from_u128(
            0x2000_0000_0000_0000_0000_0000_0000_0011 + (exec as u128) * 16 + sess as u128,
        );
        Self {
            exec,
            sess,
            session: IngestSession::new(
                exec_id,
                session_id,
                DRV_HASH.to_string(),
                IngestConfig {
                    per_exec_byte_cap: cap,
                    // Small enough that the size trigger actually flips
                    // with tiny fuzz lines; the report is cross-checked
                    // against `cut_due()` but nothing hangs off it.
                    cut_threshold_bytes: 1024,
                    cut_interval: Duration::from_secs(3600),
                },
            ),
            expected: Vec::new(),
            chunked_to: 0,
            high_water: 0,
            ceiling: None,
            next_seq: 0,
        }
    }

    fn exec_id(&self) -> Uuid {
        self.session.exec_id
    }
    fn session_id(&self) -> Uuid {
        self.session.session_id
    }

    /// The contract `IngestSession::accept` documents, in gate order.
    fn predict(&self, first: u64, n: usize) -> Predicted {
        if n == 0 {
            return Predicted::AcceptEmpty;
        }
        let Some(end) = first.checked_add(n as u64) else {
            return Predicted::Overflow;
        };
        if end > i64::MAX as u64 {
            return Predicted::Overflow;
        }
        if first < self.high_water {
            return Predicted::NonMonotone;
        }
        let (kept, end) = match self.ceiling {
            Some(c) if first >= c => return Predicted::PastFinal,
            Some(c) if end > c => ((c - first) as usize, c),
            _ => (n, end),
        };
        Predicted::Accept { kept, end }
    }

    /// Drive one batch through the real session and the model, and
    /// check they agree.
    fn append(&mut self, first: u64, n: usize, len: usize) {
        let lines: Vec<Vec<u8>> = (0..n as u64)
            // wrapping_add: the overflow probes deliberately construct
            // line numbers past u64::MAX; the harness must not be the
            // thing that panics.
            .map(|i| line_content(self.exec, self.sess, first.wrapping_add(i), len))
            .collect();
        let predicted = self.predict(first, n);
        let outcome = self.session.accept(BuildLogBatch {
            derivation_path: String::new(),
            lines: lines.clone(),
            first_line_number: first,
            executor_id: String::new(),
        });
        match (predicted, &outcome) {
            (Predicted::AcceptEmpty, Ok(AcceptOutcome::Accepted { .. })) => {}
            (Predicted::Accept { kept, end }, Ok(AcceptOutcome::Accepted { .. })) => {
                self.expected
                    .extend((first..).zip(lines.into_iter().take(kept).map(|l| {
                        if l.len() > MAX_LINE_LEN {
                            l[..MAX_LINE_LEN].to_vec()
                        } else {
                            l
                        }
                    })));
                self.high_water = end;
            }
            // The per-execution byte cap is the one gate the model does
            // not re-derive (it depends on a private per-line overhead
            // constant). It is checked AFTER every per-batch gate and
            // only for non-empty batches, so an `Err` is only
            // consistent with a predicted non-empty accept; the batch
            // must leave no trace either way.
            (Predicted::Accept { .. }, Err(_)) => {}
            (Predicted::Overflow, Ok(AcceptOutcome::RejectedOverflow))
            | (Predicted::NonMonotone, Ok(AcceptOutcome::RejectedNonMonotone))
            | (Predicted::PastFinal, Ok(AcceptOutcome::RejectedPastFinal)) => {}
            (predicted, outcome) => panic!(
                "accept gate divergence: first={first} n={n} high_water={} ceiling={:?} \
                 predicted={predicted:?} got={outcome:?}",
                self.high_water, self.ceiling
            ),
        }
        // The size-trigger report must agree with the level-triggered
        // re-check the handler performs after each cut.
        if let Ok(AcceptOutcome::Accepted { cut_due }) = outcome {
            assert_eq!(
                cut_due,
                self.session.cut_due(),
                "accept's cut_due report disagrees with cut_due()"
            );
        }
    }

    /// Mirror `set_final_line_count`'s first-write-wins semantics.
    fn seal(&mut self, count: u64) {
        self.session.set_final_line_count(count);
        self.ceiling.get_or_insert(count);
        assert_eq!(
            self.session.final_line_count(),
            self.ceiling,
            "final_line_count is stamped once; later values must be ignored"
        );
    }

    /// Drain the not-yet-chunked accepted lines into immutable chunks,
    /// one per contiguous run (a chunk's manifest row describes a
    /// gap-free `[first_line, first_line + line_count)`, so the cutter
    /// never spans a forward gap). Run discovery is the real cutter's
    /// rule, called directly: `rio_log_kernel::contiguous_prefix_len`.
    fn cut(&mut self, store: &MemoryLogChunkStore, manifest: &mut Vec<(u64, Uuid, ChunkRef)>) {
        let pending = &self.expected[self.chunked_to..];
        let mut run_start = 0;
        while run_start < pending.len() {
            let run_end = run_start
                + rio_log_kernel::contiguous_prefix_len(
                    pending[run_start..].iter().map(|(n, _)| *n),
                );
            let run = &pending[run_start..run_end];
            let first_line = run[0].0;
            let lines: Vec<Vec<u8>> = run.iter().map(|(_, l)| l.clone()).collect();
            let blob = compress_lines(&lines).expect("in-memory zstd encode cannot fail");
            let key = log_chunk_key(DRV_HASH, &self.exec_id(), &self.session_id(), self.next_seq);
            self.next_seq += 1;
            assert_eq!(
                now_or_never(store.put(&key, blob)).expect("in-memory put cannot fail"),
                PutOutcome::Created,
                "the harness must never reuse a chunk key"
            );
            manifest.push((
                first_line,
                self.session_id(),
                ChunkRef {
                    exec_id: self.exec_id(),
                    s3_key: key,
                    first_line,
                    line_count: run.len() as u64,
                },
            ));
            run_start = run_end;
        }
        self.chunked_to = self.expected.len();
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let since_line = data[0] as u64 * 3;
    let cap = 2048 + data[1] as u64 * 256;

    let store = MemoryLogChunkStore::default();
    let mut sessions: Vec<SessionHarness> = (0..4)
        .map(|i| SessionHarness::new(i >> 1, i & 1, cap))
        .collect();
    // Per-execution manifest: (first_line, session_id, chunk).
    let mut manifests: Vec<Vec<(u64, Uuid, ChunkRef)>> = vec![Vec::new(), Vec::new()];

    for op in data[2..].chunks_exact(5).take(MAX_OPS) {
        let (tag, a, b, c, d) = (op[0], op[1], op[2], op[3], op[4]);
        let which = (tag & 0b11) as usize;
        let exec = which >> 1;
        let raw = u16::from_le_bytes([a, b]) as u64;
        let (n, len) = ((c % 17) as usize, (d % 9) as usize);
        match tag >> 2 {
            0..=31 => sessions[which].append(raw % 600, n, len),
            32..=43 => sessions[which].append(raw, n, len),
            // Straddle the BIGINT representability ceiling: end = first + n
            // lands on either side of i64::MAX.
            44..=49 => {
                sessions[which].append(i64::MAX as u64 - b as u64 + (a & 31) as u64, n, len);
            }
            // Straddle u64::MAX: first + n wraps for small b.
            50..=53 => sessions[which].append(u64::MAX - b as u64, n, len),
            54..=57 => sessions[which].seal(raw % 700),
            58..=60 => {
                let manifest = &mut manifests[exec];
                sessions[which].cut(&store, manifest);
            }
            // A degenerate zero-line manifest row (the cutter never
            // writes one, but the manifest is just a table): the read
            // path must skip it without fetching or contributing
            // anything.
            _ => {
                let key = format!("logs/{DRV_HASH}/empty/{exec}/{}", manifests[exec].len());
                assert_eq!(
                    now_or_never(store.put(&key, compress_lines(&[]).unwrap()))
                        .expect("in-memory put cannot fail"),
                    PutOutcome::Created
                );
                manifests[exec].push((
                    raw,
                    sessions[which].session_id(),
                    ChunkRef {
                        exec_id: sessions[which].exec_id(),
                        s3_key: key,
                        first_line: raw,
                        line_count: 0,
                    },
                ));
            }
        }
    }

    // Drain everything still buffered so every accepted line is
    // readable, then check each session's real buffer against the model
    // (the "a rejected batch changes nothing" half: the model only ever
    // grew on predicted-and-confirmed accepts).
    for s in &mut sessions {
        let manifest = &mut manifests[s.exec];
        s.cut(&store, manifest);
        assert_eq!(
            s.session.shared().lock().unwrap().snapshot(),
            s.expected,
            "exec {} session {}: the real buffer diverged from the accept contract",
            s.exec,
            s.sess
        );
    }

    // Read each execution's manifest back through the real dedup walk.
    for (exec, manifest) in manifests.iter_mut().enumerate() {
        // What every session of this execution accepted, line → the set
        // of byte contents (two overlapping sessions may hold different
        // bytes for the same line number; the read serves exactly one).
        let mut accepted: BTreeMap<u64, Vec<&[u8]>> = BTreeMap::new();
        for s in sessions.iter().filter(|s| s.exec == exec) {
            for (n, l) in &s.expected {
                accepted.entry(*n).or_default().push(l.as_slice());
            }
        }

        // The read path's ordering contract (store.log.session-keyed —
        // fuzz/ is excluded from tracey, so no marker here): ORDER BY
        // first_line, session_id. The walk + watermark below is only a
        // complete dedup under this order.
        manifest.sort_by_key(|x| (x.0, x.1));
        let mut cursor = LineCursor::new(since_line);
        let mut served: Vec<(u64, Vec<u8>)> = Vec::new();
        for (_, _, chunk) in manifest.iter() {
            served.extend(
                now_or_never(read_chunk(&store, chunk, &mut cursor))
                    .expect("reading a chunk the harness just stored cannot fail"),
            );
        }

        // Each line at most once, in increasing order.
        for w in served.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "exec {exec}: line {} served out of order or twice (after {})",
                w[1].0,
                w[0].0
            );
        }
        // Exactly the accepted lines at or above the cursor start: the
        // served span never exceeds the accepted span, and nothing
        // accepted in range is dropped.
        let expected_lines: Vec<u64> = accepted
            .keys()
            .copied()
            .filter(|n| *n >= since_line)
            .collect();
        assert_eq!(
            served.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            expected_lines,
            "exec {exec}: served line set != accepted line set ∩ [since_line, ∞)"
        );
        // Served bytes are some session's accepted bytes for that exact
        // line — never another line's, never another execution's.
        for (n, bytes) in &served {
            assert!(
                accepted[n].contains(&bytes.as_slice()),
                "exec {exec}: line {n} served with bytes no session accepted for it"
            );
        }
        // The resume watermark a follow-up read would use.
        assert_eq!(
            cursor.next_line(),
            served.last().map_or(since_line, |(n, _)| n + 1),
            "exec {exec}: cursor did not land one past the last served line"
        );
    }
});
