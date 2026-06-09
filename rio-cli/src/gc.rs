//! `rio-cli gc` — trigger a store garbage-collection sweep.
//!
//! Calls `AdminService.TriggerGC` (server-streaming). Scheduler
//! proxies to the store after populating `extra_roots` from live
//! builds, so in-flight outputs aren't swept. Progress frames are
//! printed line-by-line so the operator sees activity during long
//! sweeps (mark phase on a large store can take minutes).

use crate::AdminClient;
use anyhow::anyhow;
use rio_proto::types::{GcProgress, GcRequest};

use crate::RPC_TIMEOUT;
use crate::stream_util::{self, DrainOutcome, DrainPolicy, MissingSentinel};

/// Per-message inactivity bound for the GC progress stream
/// (merged_bug_106). The sweep legitimately goes quiet for MINUTES
/// between frames — the mark phase on a large store is one long CTE
/// with no progress emission — so the generic
/// [`stream_util::STREAM_INACTIVITY_TIMEOUT`] (sized for
/// `VerifyChunks`' per-batch cadence) would kill healthy sweeps.
/// 15 min comfortably clears the mark phase while still converting a
/// half-open peer death (previously a ~2 h hang on the kernel's TCP
/// keepalive) into a prompt, disclosed exit.
const GC_STREAM_INACTIVITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// Report what would be collected without deleting anything.
    #[arg(long)]
    dry_run: bool,
    /// Override the per-tenant retention floor for this sweep.
    /// Paths younger than this are protected even if otherwise
    /// unreachable. Unset = use each tenant's configured retention.
    #[arg(long)]
    grace_hours: Option<u32>,
}

/// Run the `gc` subcommand.
///
/// `as_json` is NOT threaded here — gc progress is line-oriented
/// (operator wants to watch it scroll, not parse one big document
/// at the end). The global `--json` flag is ignored, same as `logs`.
pub(crate) async fn run(client: &mut AdminClient, a: Args) -> anyhow::Result<()> {
    // STREAMING — same open-timeout-only shape as `logs`. Per-message
    // silence is bounded by the drain law below (GC-sized: the mark
    // phase is minutes-quiet by design).
    let mut stream = rio_common::grpc::with_timeout(
        "TriggerGC",
        RPC_TIMEOUT,
        client.trigger_gc(GcRequest {
            dry_run: a.dry_run,
            grace_period_hours: a.grace_hours,
            ..Default::default()
        }),
    )
    .await?
    .into_inner();

    // Drain through the shared law (merged_bug_106): bounded silence
    // (15 min vs the pre-fix ~2 h half-open hang), sentinel seal (a
    // post-`is_complete` transport error — replica restart after the
    // final frame, RST before trailers — can no longer fail a
    // completed sweep), and the exit-0 disclosure posture for a
    // stream that dies WITHOUT the sentinel (the sweep may have
    // completed store-side after the progress relay died; TriggerGC
    // is idempotently re-runnable, so the exit status cannot honestly
    // assert either outcome — stderr names the ambiguity).
    let mut last: Option<GcProgress> = None;
    let outcome = stream_util::drain_with(
        &DrainPolicy {
            what: "TriggerGC",
            inactivity: GC_STREAM_INACTIVITY_TIMEOUT,
            missing_sentinel: MissingSentinel::DiscloseExitZero,
        },
        &mut stream,
        |p| {
            print_progress(&a, p);
            last = Some(p.clone());
            Ok(())
        },
        |p| p.is_complete,
    )
    .await?;
    match outcome {
        DrainOutcome::Sealed => {}
        DrainOutcome::EndedUnsealed => {
            eprintln!(
                "warning: GC stream closed without is_complete — \
                 scheduler or store disconnected mid-sweep"
            );
            return Ok(());
        }
    }

    // Exit-status posture for a FAILURE-BEARING sentinel (S6b
    // decision, coordinated with S4's typed phase-3 render): the
    // store stamps `is_complete=true` on EVERY terminal arm and
    // carries the phase-3 outcome in `current_path` — an exhaustive
    // server-side match guarantees a failure arm can never render the
    // "complete:" success string. When the store itself says the
    // chunk-collect cycle failed or was suspended, exit nonzero: an
    // operator script that ran a destructive maintenance command must
    // not read scroll-text to learn half of it failed. The
    // success-with-suffix variants ("; collect-state commit LOST",
    // "; durable observation WITHHELD") are degraded BOOKKEEPING on a
    // completed collection — disclosed in the printed line, exit 0.
    if let Some(p) = &last {
        let render = p.current_path.as_str();
        if render.starts_with("chunk collect SUSPENDED:")
            || render.starts_with("chunk collect FAILED:")
        {
            return Err(anyhow!(
                "TriggerGC: the sweep completed but the chunk-collect cycle did not: {render}"
            ));
        }
    }
    Ok(())
}

/// Print one progress frame. The terminal frame's authoritative
/// phase-3 outcome string (`current_path`, exhaustively rendered
/// store-side) is printed as a detail line under the field summary —
/// it carries chunk-collect facts the numeric fields don't.
fn print_progress(a: &Args, p: &GcProgress) {
    if p.is_complete {
        println!(
            "GC {}: {} scanned, {} collected, {} bytes freed",
            if a.dry_run {
                "dry-run complete"
            } else {
                "complete"
            },
            p.paths_scanned,
            p.paths_collected,
            p.bytes_freed
        );
        if !p.current_path.is_empty() {
            println!("  {}", p.current_path);
        }
    } else {
        println!(
            "  scanned={} collected={} freed={}B current={}",
            p.paths_scanned, p.paths_collected, p.bytes_freed, p.current_path
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_util::MessageStream;
    use std::collections::VecDeque;

    struct Scripted(VecDeque<Result<Option<GcProgress>, tonic::Status>>);

    impl MessageStream<GcProgress> for Scripted {
        async fn next_message(&mut self) -> Result<Option<GcProgress>, tonic::Status> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
    }

    fn frame(complete: bool, render: &str) -> GcProgress {
        GcProgress {
            paths_scanned: 10,
            paths_collected: 3,
            bytes_freed: 1024,
            is_complete: complete,
            current_path: render.to_string(),
        }
    }

    /// Drive the same drain composition `run` uses (the policy and
    /// the sentinel predicate are the unit under test; the RPC
    /// plumbing above it is tonic boilerplate).
    async fn drain(
        script: Vec<Result<Option<GcProgress>, tonic::Status>>,
    ) -> anyhow::Result<(DrainOutcome, Option<GcProgress>)> {
        let mut s = Scripted(script.into());
        let mut last = None;
        let outcome = stream_util::drain_with(
            &DrainPolicy {
                what: "TriggerGC",
                inactivity: GC_STREAM_INACTIVITY_TIMEOUT,
                missing_sentinel: MissingSentinel::DiscloseExitZero,
            },
            &mut s,
            |p| {
                last = Some(p.clone());
                Ok(())
            },
            |p| p.is_complete,
        )
        .await?;
        Ok((outcome, last))
    }

    // r[verify cli.stream.drain-bound]
    /// RED (merged_bug_106, polarity 1): a post-sentinel transport
    /// error must not fail a completed sweep — pre-fix the drain loop
    /// `?`-propagated every poll, so a replica restarting after the
    /// final frame turned "GC complete" into a nonzero exit.
    #[tokio::test]
    async fn post_sentinel_error_is_sealed_ok() {
        let (outcome, last) = drain(vec![
            Ok(Some(frame(false, "/nix/store/abc"))),
            Ok(Some(frame(true, "complete: 3 paths deleted"))),
            Err(tonic::Status::unavailable("replica restarted post-seal")),
        ])
        .await
        .expect("the sentinel sealed the sweep");
        assert_eq!(outcome, DrainOutcome::Sealed);
        assert!(last.unwrap().is_complete);
    }

    // r[verify cli.stream.drain-bound]
    /// RED (merged_bug_106, polarity 2): half-open silence is bounded.
    /// Pre-fix, a peer that died without FIN/RST hung `rio-cli gc`
    /// until the kernel's ~2 h TCP keepalive — the operator's terminal
    /// just sat there. (The pre-fix red IS the hang; paused-clock
    /// test.)
    struct HalfOpen;
    impl MessageStream<GcProgress> for HalfOpen {
        async fn next_message(&mut self) -> Result<Option<GcProgress>, tonic::Status> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_silence_is_bounded() {
        let mut s = HalfOpen;
        let err = stream_util::drain_with(
            &DrainPolicy {
                what: "TriggerGC",
                inactivity: GC_STREAM_INACTIVITY_TIMEOUT,
                missing_sentinel: MissingSentinel::DiscloseExitZero,
            },
            &mut s,
            |_: &GcProgress| Ok(()),
            |p| p.is_complete,
        )
        .await
        .expect_err("a half-open peer is a bounded, disclosed exit — not a 2 h hang");
        assert!(err.to_string().contains("no message for"));
    }

    /// The failure-bearing sentinel posture (S6b decision): the
    /// store's own SUSPENDED/FAILED phase-3 renders exit nonzero;
    /// success and degraded-bookkeeping renders exit 0.
    #[test]
    fn failure_sentinel_posture() {
        let failing = [
            "chunk collect SUSPENDED: unparseable chunk_list aborted the cycle fail-closed",
            "chunk collect FAILED: db timeout; prior batches committed",
        ];
        let passing = [
            "complete: 3 paths deleted, 2 chunks, 2 S3 keys enqueued, 9 bytes freed, 0 resurrected",
            "complete: 3 paths deleted; collect-state commit LOST (stats real, cadence stamp not)",
            "dry run: would delete 3 paths",
            "already running (concurrent GC in progress)",
            "",
        ];
        for render in failing {
            assert!(
                render.starts_with("chunk collect SUSPENDED:")
                    || render.starts_with("chunk collect FAILED:"),
                "posture must catch: {render}"
            );
        }
        for render in passing {
            assert!(
                !(render.starts_with("chunk collect SUSPENDED:")
                    || render.starts_with("chunk collect FAILED:")),
                "posture must pass: {render}"
            );
        }
    }
}
