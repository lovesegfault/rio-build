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

/// Drain `stream`, requiring the server's terminal sentinel before the
/// end of the stream. `on_item` sees every message (print progress,
/// collect results); `is_done` recognizes the sentinel. A stream that
/// ends without it returns `Err` — the caller's results are PARTIAL
/// and the process exit code must say so.
pub(crate) async fn drain_until_done<T, S: MessageStream<T>>(
    what: &str,
    stream: &mut S,
    mut on_item: impl FnMut(&T) -> anyhow::Result<()>,
    is_done: impl Fn(&T) -> bool,
) -> anyhow::Result<()> {
    let mut done = false;
    while let Some(msg) = stream
        .next_message()
        .await
        .map_err(|s| anyhow!("{what}: stream: {} ({:?})", s.message(), s.code()))?
    {
        on_item(&msg)?;
        if is_done(&msg) {
            done = true;
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
}
