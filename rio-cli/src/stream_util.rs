//! Shared server-stream draining law for CLI commands (bug_141,
//! merged_bug_106: ONE chokepoint, composable policies).
//!
//! A server-streaming RPC that ends WITHOUT its terminal sentinel was
//! truncated — scheduler restart, store disconnect, LB idle reap. For
//! an audit command the distinction is the whole point: a truncated
//! `verify-chunks` scan that exits 0 looks exactly like a complete
//! scan that found nothing, and the operator acts on absence. For a
//! rendered stream (`logs`, `gc`) the truncation is surfaced on
//! stderr instead and the exit stays 0 — but the OTHER two halves of
//! the drain law still apply.
//!
//! The law has three independently composable parts
//! ([`DrainPolicy`]); opting out of one half cannot drop the others:
//!
//! 1. **Bounded silence** — every poll carries a per-message
//!    inactivity bound, converting the half-open-connection class
//!    (peer died without FIN/RST on the CLI's deliberately
//!    keepalive-free channel) from an unbounded hang into a prompt
//!    exit. Always on; no policy removes it.
//! 2. **Sentinel seal** — the terminal sentinel IS the end, by
//!    construction: the loop `break`s on it and never polls again, so
//!    a post-sentinel transport error (replica restart after sealing,
//!    RST before trailers) cannot fail a complete result (bug_163's
//!    post-sentinel poll is unrepresentable here). Always on.
//! 3. **Missing-sentinel posture** — what a clean end WITHOUT the
//!    sentinel means is the one genuinely per-command decision:
//!    [`MissingSentinel::Truncation`] (audit posture: `Err`, nonzero
//!    exit) or [`MissingSentinel::DiscloseExitZero`] (rendered-stream
//!    posture: `Ok(`[`DrainOutcome::EndedUnsealed`]`)`, and the
//!    `#[must_use]` outcome forces the caller to render its own
//!    disclosure).
//!
//! Every sentinel-bearing CLI server-stream routes through here —
//! `verify-chunks` (audit), `logs` (rendered, bug_163), `gc`
//! (rendered, merged_bug_106). The committed call-site census lives
//! in `docs/gen/sweeps/bughunt4-s6b.md`; zero raw `.message()` drain
//! loops remain outside this module.

use anyhow::anyhow;

/// Per-message inactivity bound for audit-posture drains
/// (merged_bug_085, merged_bug_023).
///
/// The CLI's store channel is `connect_channel` — eager,
/// throughput-tuned, and DELIBERATELY keepalive-free (the eager-connect
/// PING/PONG race documented at rio-proto/src/client/mod.rs). A peer
/// that dies without FIN/RST (SIGKILL, netsplit, OOM-kill, LB state
/// drop) therefore leaves a bare `next_message().await` pending until
/// the kernel's 2h TCP keepalive notices — the exact truncation class
/// the module doc covers, previously unbounded. The drain law owns the
/// bound itself: every consumer gets the hang-to-PARTIAL conversion.
///
/// The value IS the bilateral bound from `rio_common::liveness`
/// (merged_bug_023): `VerifyChunks` emits a progress frame at least
/// every `ADMIN_VERIFY_EMIT_EVERY` probes (producer-enforced emission
/// sub-batches), and the conformance test in `rio_common::liveness`
/// machine-binds the derived worst-case emission gap strictly inside
/// this bound --- the client bound cites a verified producer contract,
/// not an unverified estimate of "one batch fits".
pub(crate) const STREAM_INACTIVITY_TIMEOUT: std::time::Duration =
    rio_common::liveness::ADMIN_STREAM_INACTIVITY_TIMEOUT;

/// One message-at-a-time pull from a server stream. Blanket-implemented
/// for `tonic::Streaming`; test doubles implement it over a queue so
/// the drain law is unit-testable without a server.
pub(crate) trait MessageStream<T> {
    async fn next_message(&mut self) -> Result<Option<T>, tonic::Status>;
}

impl<T> MessageStream<T> for tonic::Streaming<T> {
    async fn next_message(&mut self) -> Result<Option<T>, tonic::Status> {
        self.message().await
    }
}

/// What a clean end-of-stream WITHOUT the terminal sentinel means for
/// this consumer — the one per-command axis of the drain law.
pub(crate) enum MissingSentinel {
    /// Audit posture: the results are PARTIAL and the exit code must
    /// say so (`Err`).
    Truncation,
    /// Rendered-stream posture: the consumer surfaces the truncation
    /// on stderr itself and the exit stays 0. The drain returns
    /// [`DrainOutcome::EndedUnsealed`] — `#[must_use]`, so silently
    /// ignoring the unsealed end does not compile past review.
    DiscloseExitZero,
}

/// One consumer's composition of the drain law.
pub(crate) struct DrainPolicy {
    /// Human label for error messages (`"VerifyChunks"`, `"TailLog"`,
    /// `"TriggerGC"`).
    pub what: &'static str,
    /// Per-message inactivity bound (half-open-connection
    /// conversion). Every policy carries one; there is no way to
    /// construct a drain without it.
    pub inactivity: std::time::Duration,
    /// Clean-end-without-sentinel posture.
    pub missing_sentinel: MissingSentinel,
}

impl DrainPolicy {
    /// The audit composition (bug_141 shape): standard bound,
    /// missing sentinel is a truncation `Err`.
    pub(crate) const fn audit(what: &'static str) -> Self {
        Self {
            what,
            inactivity: STREAM_INACTIVITY_TIMEOUT,
            missing_sentinel: MissingSentinel::Truncation,
        }
    }
}

/// How a policy-compliant drain ended. `#[must_use]` is part of the
/// law: a `DiscloseExitZero` consumer cannot silently drop the
/// unsealed case — it owes the stderr disclosure.
#[must_use = "an EndedUnsealed drain owes the caller's stderr disclosure"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainOutcome {
    /// The terminal sentinel arrived and sealed the stream; nothing
    /// was polled after it.
    Sealed,
    /// The stream ended cleanly without the sentinel under
    /// [`MissingSentinel::DiscloseExitZero`].
    EndedUnsealed,
}

// r[impl cli.stream.drain-bound]
/// Drain `stream` under `policy`. `on_item` sees every message (print
/// progress, collect results); `is_done` recognizes the terminal
/// sentinel.
///
/// The two always-on terminality laws (merged_bug_085):
/// - The sentinel IS the end, by construction: the loop `break`s on it
///   and never polls again, so a post-sentinel transport error
///   (replica restart after sealing, RST before trailers) cannot fail
///   a complete result.
/// - Silence is bounded: each poll carries `policy.inactivity`,
///   converting the half-open connection class from an unbounded hang
///   into a prompt error.
///
/// Errors BEFORE the sentinel (transport status, inactivity timeout,
/// `on_item` failure) are `Err` under every policy — only the
/// clean-end-without-sentinel case is posture-dependent.
pub(crate) async fn drain_with<T, S: MessageStream<T>>(
    policy: &DrainPolicy,
    stream: &mut S,
    mut on_item: impl FnMut(&T) -> anyhow::Result<()>,
    is_done: impl Fn(&T) -> bool,
) -> anyhow::Result<DrainOutcome> {
    let what = policy.what;
    let inactivity = policy.inactivity;
    loop {
        let polled = tokio::time::timeout(inactivity, stream.next_message())
            .await
            .map_err(|_| {
                anyhow!(
                    "{what}: no message for {inactivity:?} — the \
                     connection is presumed half-open (peer died without \
                     FIN/RST) and the stream was truncated; the results above \
                     are PARTIAL"
                )
            })?;
        let Some(msg) =
            polled.map_err(|s| anyhow!("{what}: stream: {} ({:?})", s.message(), s.code()))?
        else {
            break;
        };
        on_item(&msg)?;
        if is_done(&msg) {
            // Terminal by construction: the server sealed the stream;
            // whatever the transport does after this is teardown
            // noise, not evidence. NO further poll happens (bug_163).
            return Ok(DrainOutcome::Sealed);
        }
    }
    match policy.missing_sentinel {
        MissingSentinel::Truncation => Err(anyhow!(
            "{what}: stream ended without the terminal sentinel — the scan was \
             truncated (store or scheduler disconnected mid-stream) and the \
             results above are PARTIAL"
        )),
        MissingSentinel::DiscloseExitZero => Ok(DrainOutcome::EndedUnsealed),
    }
}

/// The audit-posture drain (bug_141 call sites). A stream that ends
/// without the sentinel returns `Err` — the caller's results are
/// PARTIAL and the process exit code must say so.
pub(crate) async fn drain_until_done<T, S: MessageStream<T>>(
    what: &'static str,
    stream: &mut S,
    on_item: impl FnMut(&T) -> anyhow::Result<()>,
    is_done: impl Fn(&T) -> bool,
) -> anyhow::Result<()> {
    match drain_with(&DrainPolicy::audit(what), stream, on_item, is_done).await? {
        DrainOutcome::Sealed => Ok(()),
        // Unreachable: Truncation converts the unsealed end to Err
        // above. The match is still total so a policy refactor cannot
        // silently change the audit posture.
        DrainOutcome::EndedUnsealed => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Scripted(VecDeque<Result<Option<u32>, tonic::Status>>);

    impl MessageStream<u32> for Scripted {
        async fn next_message(&mut self) -> Result<Option<u32>, tonic::Status> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
    }

    /// RED (bug_141): a stream that closes without the sentinel must
    /// be an Err — pre-fix, verify_chunks printed a warning and exited
    /// 0, so a truncated audit was indistinguishable from a clean one.
    #[tokio::test]
    async fn close_without_sentinel_is_err() {
        let mut s = Scripted(VecDeque::from([Ok(Some(1)), Ok(Some(2))]));
        let mut seen = Vec::new();
        let err = drain_until_done(
            "test",
            &mut s,
            |m| {
                seen.push(*m);
                Ok(())
            },
            |m| *m == 99,
        )
        .await
        .expect_err("close sans sentinel is a truncation");
        assert!(err.to_string().contains("PARTIAL"));
        assert_eq!(
            seen,
            vec![1, 2],
            "items before the close are still delivered"
        );
    }

    #[tokio::test]
    async fn sentinel_then_close_is_ok() {
        let mut s = Scripted(VecDeque::from([Ok(Some(1)), Ok(Some(99))]));
        drain_until_done("test", &mut s, |_| Ok(()), |m| *m == 99)
            .await
            .expect("sentinel seen");
    }

    #[tokio::test]
    async fn transport_error_is_err_with_context() {
        let mut s = Scripted(VecDeque::from([
            Ok(Some(1)),
            Err(tonic::Status::unavailable("nope")),
        ]));
        let err = drain_until_done("test", &mut s, |_| Ok(()), |m| *m == 99)
            .await
            .expect_err("status propagates");
        assert!(err.to_string().contains("test: stream"));
    }

    // r[verify cli.stream.drain-bound]
    /// RED (merged_bug_085 hole 2): the loop kept polling after the
    /// sentinel with `?` on every iteration, so a transport error
    /// between the final `done:true` frame and the clean end-of-stream
    /// (replica restart post-seal, RST before trailers) failed a
    /// COMPLETE audit as PARTIAL — and a recovery script keyed on the
    /// exit status re-ran an expensive full scan for nothing. The
    /// sentinel is terminal by construction: no further polls.
    #[tokio::test]
    async fn sentinel_then_transport_error_is_ok() {
        let mut s = Scripted(VecDeque::from([
            Ok(Some(1)),
            Ok(Some(99)),
            Err(tonic::Status::unavailable("replica restarted post-seal")),
        ]));
        drain_until_done("test", &mut s, |_| Ok(()), |m| *m == 99)
            .await
            .expect("the sentinel sealed the scan; post-sentinel noise is not truncation");
    }

    // r[verify cli.stream.drain-bound]
    /// merged_bug_085 hole 1: the half-open-connection class. A peer
    /// that dies without FIN/RST (SIGKILL, netsplit, OOM-kill) leaves
    /// `next_message()` pending forever on the CLI's keepalive-free
    /// channel — the exact truncation cause the module doc claims to
    /// cover, previously unbounded. The drain law now owns a
    /// per-message inactivity bound. (A pre-fix red is infeasible as a
    /// test — it IS the hang; the strawman red is recorded in the
    /// commit body instead.)
    struct HalfOpen;
    impl MessageStream<u32> for HalfOpen {
        async fn next_message(&mut self) -> Result<Option<u32>, tonic::Status> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_silence_is_bounded_truncation() {
        let mut s = HalfOpen;
        let err = drain_until_done("test", &mut s, |_: &u32| Ok(()), |_| false)
            .await
            .expect_err("unbounded silence is a truncation, not a wait");
        assert!(err.to_string().contains("PARTIAL"));
        assert!(err.to_string().contains("no message for"));
    }

    // ── The composable-policy axis (merged_bug_106 / bug_163) ──────

    // r[verify cli.stream.drain-bound]
    /// RED (bug_163 shape, at the law level): under the
    /// rendered-stream posture a post-sentinel transport error must
    /// not fail the drain — the sentinel sealed it. Pre-fix, logs.rs
    /// polled once more past `is_complete` and a post-seal RST exited
    /// nonzero with the complete log already on stdout.
    #[tokio::test]
    async fn disclose_posture_sentinel_then_transport_error_is_sealed() {
        let mut s = Scripted(VecDeque::from([
            Ok(Some(1)),
            Ok(Some(99)),
            Err(tonic::Status::unavailable("LB reset after the final chunk")),
        ]));
        let policy = DrainPolicy {
            what: "test",
            inactivity: STREAM_INACTIVITY_TIMEOUT,
            missing_sentinel: MissingSentinel::DiscloseExitZero,
        };
        let outcome = drain_with(&policy, &mut s, |_| Ok(()), |m| *m == 99)
            .await
            .expect("sealed before the error");
        assert_eq!(outcome, DrainOutcome::Sealed);
    }

    /// The rendered-stream posture converts a clean unsealed end into
    /// `Ok(EndedUnsealed)` — the caller renders the disclosure — but
    /// does NOT swallow pre-sentinel errors: only the
    /// clean-end-without-sentinel case is posture-dependent.
    #[tokio::test]
    async fn disclose_posture_clean_unsealed_end_is_ok_and_flagged() {
        let mut s = Scripted(VecDeque::from([Ok(Some(1))]));
        let policy = DrainPolicy {
            what: "test",
            inactivity: STREAM_INACTIVITY_TIMEOUT,
            missing_sentinel: MissingSentinel::DiscloseExitZero,
        };
        let outcome = drain_with(&policy, &mut s, |_| Ok(()), |m| *m == 99)
            .await
            .expect("clean unsealed end is not an error under this posture");
        assert_eq!(outcome, DrainOutcome::EndedUnsealed);

        let mut s = Scripted(VecDeque::from([
            Ok(Some(1)),
            Err(tonic::Status::unavailable("mid-stream death")),
        ]));
        drain_with(&policy, &mut s, |_| Ok(()), |m| *m == 99)
            .await
            .expect_err("a PRE-sentinel transport error is an error under every posture");
    }

    /// The bound composes with the disclosure posture: opting out of
    /// the truncation `Err` does not opt out of the half-open
    /// conversion (the policy struct has no way to express a
    /// bound-less drain).
    #[tokio::test(start_paused = true)]
    async fn disclose_posture_still_bounds_half_open_silence() {
        let mut s = HalfOpen;
        let policy = DrainPolicy {
            what: "test",
            inactivity: STREAM_INACTIVITY_TIMEOUT,
            missing_sentinel: MissingSentinel::DiscloseExitZero,
        };
        let err = drain_with(&policy, &mut s, |_: &u32| Ok(()), |_| false)
            .await
            .expect_err("the inactivity bound is not optional");
        assert!(err.to_string().contains("no message for"));
    }
}
