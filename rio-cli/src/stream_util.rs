//! Shared server-stream draining law for CLI commands (bug_141).
//!
//! A server-streaming RPC that ends WITHOUT its terminal sentinel was
//! truncated — scheduler restart, store disconnect, LB idle reap. For
//! an audit command the distinction is the whole point: a truncated
//! `verify-chunks` scan that exits 0 looks exactly like a complete
//! scan that found nothing, and the operator acts on absence. The
//! drain law makes "ended sans sentinel" an `Err` (nonzero exit) at
//! every consumer that opts in.
//!
//! Documented exceptions (deliberate exit-0 + stderr disclosure, NOT
//! converted):
//!
//! - `logs.rs` (TailLog): an incomplete LOG is a rendered state with
//!   user-facing semantics (`is_complete=false` = build still running
//!   / flush pending), not a command failure; the stderr completeness
//!   notes carry the disclosure and `rio-cli logs` stays pipeline-safe
//!   (exit 0 with whatever lines exist).
//! - `gc.rs` (TriggerGC): the sweep may have completed store-side
//!   after the progress relay died — the exit status cannot honestly
//!   assert success OR failure, the command is idempolently re-runnable,
//!   and the stderr warning names the ambiguity.

use anyhow::anyhow;

/// Per-message inactivity bound for the drain law (merged_bug_085).
///
/// The CLI's store channel is `connect_channel` — eager,
/// throughput-tuned, and DELIBERATELY keepalive-free (the eager-connect
/// PING/PONG race documented at rio-proto/src/client/mod.rs). A peer
/// that dies without FIN/RST (SIGKILL, netsplit, OOM-kill, LB state
/// drop) therefore leaves a bare `next_message().await` pending until
/// the kernel's 2h TCP keepalive notices — the exact truncation class
/// the module doc covers, previously unbounded. The drain law owns the
/// bound itself: every consumer that opts into sentinel semantics gets
/// the hang-to-PARTIAL conversion with it.
///
/// 120s == the CLI's RPC_TIMEOUT budget: `VerifyChunks` emits a
/// progress frame per batch, and one batch (a bounded PG scan + S3
/// HeadObject sweep) comfortably fits even against a degraded S3.
/// Interactive follow streams (`rio-cli logs`) are NOT drained through
/// this law — an hour-quiet build log is legitimate idle there
/// (dash.stream.idle-timeout), and logs.rs stays a documented
/// exception.
pub(crate) const STREAM_INACTIVITY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(120);

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

// r[impl cli.stream.drain-bound]
/// Drain `stream`, requiring the server's terminal sentinel before the
/// end of the stream. `on_item` sees every message (print progress,
/// collect results); `is_done` recognizes the sentinel. A stream that
/// ends without it returns `Err` — the caller's results are PARTIAL
/// and the process exit code must say so.
///
/// Two terminality laws (merged_bug_085):
/// - The sentinel IS the end, by construction: the loop `break`s on it
///   and never polls again, so a post-sentinel transport error
///   (replica restart after sealing, RST before trailers) cannot fail
///   a complete audit.
/// - Silence is bounded: each poll carries
///   [`STREAM_INACTIVITY_TIMEOUT`], converting the half-open
///   connection class from an unbounded hang into a nonzero PARTIAL
///   exit.
pub(crate) async fn drain_until_done<T, S: MessageStream<T>>(
    what: &str,
    stream: &mut S,
    mut on_item: impl FnMut(&T) -> anyhow::Result<()>,
    is_done: impl Fn(&T) -> bool,
) -> anyhow::Result<()> {
    let mut done = false;
    loop {
        let polled = tokio::time::timeout(STREAM_INACTIVITY_TIMEOUT, stream.next_message())
            .await
            .map_err(|_| {
                anyhow!(
                    "{what}: no message for {STREAM_INACTIVITY_TIMEOUT:?} — the \
                     connection is presumed half-open (peer died without \
                     FIN/RST) and the scan was truncated; the results above \
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
            done = true;
            // Terminal by construction: the server sealed the scan;
            // whatever the transport does after this is teardown
            // noise, not evidence.
            break;
        }
    }
    if done {
        Ok(())
    } else {
        Err(anyhow!(
            "{what}: stream ended without the terminal sentinel — the scan was \
             truncated (store or scheduler disconnected mid-stream) and the \
             results above are PARTIAL"
        ))
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
}
