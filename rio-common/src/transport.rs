//! Bounded, cancellable unary-RPC awaits — the transport discipline
//! every pull/report/claim loop builds on.
//!
//! The class this module closes by construction: a loop `await`s a
//! generated unary bare, so an accepted-never-answered request (a
//! black-holed peer, a half-open connection, a hung middlebox) pins
//! the task forever — SIGTERM is observed only between awaits, retry
//! budgets are enforced only on `Err` arms, and the process rides to
//! SIGKILL. [`bounded`] makes the three outcomes of any unary await
//! explicit and exhaustive: the answer arrived, shutdown won the race,
//! or the bound elapsed. Consumers match all three — the compiler
//! forces the SIGTERM and timeout arms to exist.
//!
//! [`AttemptBudget`] is the per-phase retry budget measured from phase
//! entry: `attempt_bound` clamps each attempt's bound to the remaining
//! budget so a sequence of timed-out attempts exhausts the budget
//! instead of resetting it (the "budget enforced only on Err" gap).
//!
//! [`GraceBudget`] partitions a pod's termination grace
//! ([`crate::limits::PULL_MODE_TERMINATION_GRACE_SECS`]) into an
//! abort-drain slice (waiting for a killed build/walk to surface its
//! completion) and a reserved report slice — reserving the report
//! window at the type level is what guarantees the final best-effort
//! report always has wire time before the kubelet's SIGKILL.
//!
//! Formal-coverage record (none-sensible for quint, relocated from the
//! retired retry invariant map): [`bounded`] is a local two-arm race
//! primitive whose model would restate tokio `select` semantics.
//! Correctness is carried type-level (`#[must_use]` [`BoundedOutcome`]),
//! by the `transport-unary-ban` policy check, and by the unit battery
//! (biased shutdown, timeout, budget arithmetic, [`GraceBudget`]
//! const-asserts).

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// One final best-effort RPC attempt after SIGTERM: bound per decision
/// P5, inside the AD5 grace. Shared by the builder report loop, the
/// store materialization report loop, and any future SIGTERM arm.
pub const SIGTERM_FINAL_ATTEMPT: Duration = Duration::from_secs(10);

/// The three outcomes of a bounded unary await. `#[must_use]` plus
/// exhaustive matching is the point: every consumer writes a `Shutdown`
/// arm and a `TimedOut` arm or does not compile.
#[must_use = "a bounded await's Shutdown/TimedOut outcomes carry control flow"]
#[derive(Debug)]
pub enum BoundedOutcome<T> {
    /// The peer answered (with the RPC's own success or error).
    Resolved(Result<T, tonic::Status>),
    /// The shutdown token fired while the RPC was in flight. The
    /// in-flight request is dropped (tonic cancels the HTTP/2 stream).
    Shutdown,
    /// The bound elapsed with no answer. The in-flight request is
    /// dropped; `after` is the bound that elapsed (for logging).
    TimedOut {
        /// The bound that elapsed.
        after: Duration,
    },
}

impl<T> BoundedOutcome<T> {
    /// True iff the await resolved with `Ok`.
    pub fn is_ok(&self) -> bool {
        matches!(self, BoundedOutcome::Resolved(Ok(_)))
    }
}

/// The three outcomes of a bounded streaming-RPC *open*. Same shape as
/// [`BoundedOutcome`], named separately because the abort signal is a
/// caller-chosen future (a drain edge, a shutdown token, a watch
/// receiver dying) rather than always a [`CancellationToken`], and
/// because the payload is the opened response stream, not a unary
/// answer.
#[must_use = "a bounded open's Aborted/TimedOut outcomes carry control flow"]
#[derive(Debug)]
pub enum OpenOutcome<T> {
    /// The open resolved (with the RPC's own success or error).
    Opened(Result<T, tonic::Status>),
    /// The abort future fired while the open was in flight. The
    /// in-flight request is dropped (tonic cancels the HTTP/2 stream).
    Aborted,
    /// The bound elapsed with no answer. The in-flight request is
    /// dropped; `after` is the bound that elapsed (for logging).
    TimedOut {
        /// The bound that elapsed.
        after: Duration,
    },
}

/// Await one streaming-RPC open, racing it (biased, in order) against
/// an abort future and a deadline. The only sanctioned way to open a
/// generated streaming RPC from a daemon crate — see the
/// `streaming-open-ban` policy check, whose banned-method list is
/// derived from the proto descriptor set at check time, so a NEW
/// streaming rpc is born banned by protoc's own parse.
///
/// The abort future is caller-chosen: a `CancellationToken::cancelled`,
/// a drain watch edge, a peer-death signal. It must be cancel-safe
/// (it is dropped if the open resolves first).
// r[impl proto.client.streaming-open-bounded]
pub async fn bounded_open<T>(
    abort: impl Future<Output = ()>,
    bound: Duration,
    open: impl Future<Output = Result<T, tonic::Status>>,
) -> OpenOutcome<T> {
    tokio::select! {
        biased;
        () = abort => OpenOutcome::Aborted,
        resolved = tokio::time::timeout(bound, open) => match resolved {
            Ok(result) => OpenOutcome::Opened(result),
            Err(_elapsed) => OpenOutcome::TimedOut { after: bound },
        },
    }
}

/// A DEADLINE-TYPED open bound (bug_038, R34(iv)): the fixed
/// per-attempt bound CLAMPED to whatever remains of an enclosing
/// armed envelope deadline (a post-terminal grace window, a shutdown
/// budget). Awaits inside a grace envelope take THIS type, never a
/// bare `Duration` — constructing it forces the envelope consult
/// BEFORE the open arms, so the per-attempt bound structurally cannot
/// outlive the envelope it is nested inside. An unarmed envelope
/// (`None`) degenerates to the fixed bound; an already-expired one
/// clamps to zero (the open gets a single poll and times out — the
/// caller's exit law sees the expiry on the very next consult).
#[derive(Debug, Clone, Copy)]
pub struct OpenDeadline(Duration);

impl OpenDeadline {
    /// Clamp `per_attempt` to the time remaining until `envelope`
    /// (saturating at zero). `None` = no envelope armed.
    #[must_use]
    pub fn within(per_attempt: Duration, envelope: Option<tokio::time::Instant>) -> Self {
        Self(match envelope {
            Some(deadline) => {
                per_attempt.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
            }
            None => per_attempt,
        })
    }

    /// The effective bound this deadline admits.
    #[must_use]
    pub fn bound(self) -> Duration {
        self.0
    }
}

/// [`bounded_open`] with the bound supplied as a typed, envelope-
/// clamped [`OpenDeadline`] (bug_038): the sanctioned form for opens
/// that run INSIDE a grace envelope. `TimedOut::after` reports the
/// EFFECTIVE bound (the clamped value), so a log line distinguishes
/// "the per-attempt bound elapsed" from "the envelope had less
/// remaining than one attempt".
// r[impl proto.client.streaming-open-bounded]
pub async fn bounded_open_within<T>(
    abort: impl Future<Output = ()>,
    deadline: OpenDeadline,
    open: impl Future<Output = Result<T, tonic::Status>>,
) -> OpenOutcome<T> {
    bounded_open(abort, deadline.bound(), open).await
}

/// Await one unary RPC, racing it (biased, in order) against shutdown
/// and a deadline. The only sanctioned way to await a generated unary
/// in a retry loop — see the `transport-unary-ban` policy check.
pub async fn bounded<T>(
    shutdown: &CancellationToken,
    bound: Duration,
    rpc: impl Future<Output = Result<T, tonic::Status>>,
) -> BoundedOutcome<T> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => BoundedOutcome::Shutdown,
        resolved = tokio::time::timeout(bound, rpc) => match resolved {
            Ok(result) => BoundedOutcome::Resolved(result),
            Err(_elapsed) => BoundedOutcome::TimedOut { after: bound },
        },
    }
}

/// Outcome of [`bounded_effectful`]: like [`BoundedOutcome`], but the
/// shutdown arm distinguishes whether the RPC future was ever polled.
///
/// Rust futures are lazy — an RPC future that has never been polled has
/// provably done nothing (tonic serializes and writes the request on
/// first poll). Once polled, the request MAY be on the wire and the
/// callee MAY have acted on it (e.g. the scheduler minted an
/// assignment), even though the caller abandoned the await. Callers
/// that must reconcile side effects on shutdown (merged_bug_270's
/// maybe-minted pull) branch on that distinction; callers indifferent
/// to it keep using [`bounded`].
#[must_use = "the shutdown arms carry the maybe-minted distinction"]
#[derive(Debug)]
pub enum EffectfulOutcome<T> {
    /// The RPC resolved (with the callee's answer or a transport error).
    Resolved(T),
    /// Shutdown fired before the future was first polled: the request
    /// never reached the wire; no side effect exists.
    ShutdownBeforeSend,
    /// Shutdown fired after at least one poll: the request may have
    /// been sent and acted on — the side effect is UNKNOWN.
    ShutdownAfterSend,
    /// The bound elapsed after at least one poll — sent, never
    /// answered. Side effect unknown (same epistemic state as
    /// `ShutdownAfterSend`; a separate variant so retry loops keep
    /// their pacing arm).
    TimedOut {
        /// The elapsed bound at expiry.
        after: Duration,
    },
}

/// [`bounded`] with a polled-once latch (merged_bug_270).
///
/// Identical race structure (biased shutdown, then a timeout-wrapped
/// await), but the shutdown arm reports whether the RPC future was
/// polled at least once before abandonment — the boundary between
/// "provably nothing happened" and "maybe the callee acted".
pub async fn bounded_effectful<T>(
    shutdown: &CancellationToken,
    bound: Duration,
    rpc: impl Future<Output = Result<T, tonic::Status>>,
) -> EffectfulOutcome<Result<T, tonic::Status>> {
    let mut polled = false;
    tokio::pin!(rpc);
    let tracked = std::future::poll_fn(|cx| {
        polled = true;
        rpc.as_mut().poll(cx)
    });
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            if polled {
                EffectfulOutcome::ShutdownAfterSend
            } else {
                EffectfulOutcome::ShutdownBeforeSend
            }
        }
        resolved = tokio::time::timeout(bound, tracked) => match resolved {
            Ok(result) => EffectfulOutcome::Resolved(result),
            Err(_elapsed) => EffectfulOutcome::TimedOut { after: bound },
        },
    }
}

/// A phase-scoped retry budget: started at phase entry, spent by both
/// answered failures AND unanswered (timed-out) attempts.
#[derive(Debug)]
pub struct AttemptBudget {
    started: tokio::time::Instant,
    total: Duration,
}

impl AttemptBudget {
    /// Start the budget clock now (tokio time, so paused-clock tests
    /// drive it deterministically).
    pub fn new(total: Duration) -> Self {
        Self {
            started: tokio::time::Instant::now(),
            total,
        }
    }

    /// Budget left, saturating at zero.
    pub fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.started.elapsed())
    }

    /// True once the budget is fully spent.
    pub fn expired(&self) -> bool {
        self.remaining() == Duration::ZERO
    }

    /// The bound for the next attempt: the per-attempt cap clamped to
    /// the remaining budget, floored at 1 ms so an almost-spent budget
    /// still makes a non-degenerate final attempt (and `expired()` is
    /// what actually terminates the loop).
    pub fn attempt_bound(&self, cap: Duration) -> Duration {
        self.remaining().min(cap).max(Duration::from_millis(1))
    }
}

/// The pod termination grace, partitioned: `abort_drain` is how long
/// the SIGTERM path may wait for an aborted build/walk to surface its
/// completion; `report` is the reserved tail for the final best-effort
/// report. `new` is const so the partition is checked at compile time
/// wherever the budget is a const.
#[derive(Debug, Clone, Copy)]
pub struct GraceBudget {
    total: Duration,
    report_reserve: Duration,
}

impl GraceBudget {
    /// Partition `total` with `report_reserve` held back for the final
    /// report.
    ///
    /// # Panics
    ///
    /// Panics (at compile time in const contexts) if the reserve does
    /// not leave a non-empty drain slice.
    pub const fn new(total: Duration, report_reserve: Duration) -> Self {
        assert!(
            report_reserve.as_millis() < total.as_millis(),
            "GraceBudget: the report reserve must leave a non-empty abort-drain slice"
        );
        Self {
            total,
            report_reserve,
        }
    }

    /// The abort-drain slice: total minus the report reserve.
    pub const fn abort_drain(&self) -> Duration {
        // const-safe subtraction: new() guarantees reserve < total.
        Duration::from_millis((self.total.as_millis() - self.report_reserve.as_millis()) as u64)
    }

    /// The reserved report slice.
    pub const fn report(&self) -> Duration {
        self.report_reserve
    }

    /// The whole grace.
    pub const fn total(&self) -> Duration {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    async fn answer_after(d: Duration) -> Result<u32, tonic::Status> {
        tokio::time::sleep(d).await;
        Ok(7)
    }

    /// The happy path: an answered RPC resolves with its own result.
    #[tokio::test(start_paused = true)]
    async fn bounded_resolves_an_answered_rpc() {
        let shutdown = token();
        let out = bounded(
            &shutdown,
            Duration::from_secs(30),
            answer_after(Duration::from_secs(1)),
        )
        .await;
        assert!(matches!(out, BoundedOutcome::Resolved(Ok(7))));
        assert!(out.is_ok());
    }

    /// A black-holed RPC (never answers) resolves `TimedOut` at the
    /// bound instead of pinning the caller.
    #[tokio::test(start_paused = true)]
    async fn bounded_times_out_a_black_hole() {
        let shutdown = token();
        let started = tokio::time::Instant::now();
        let out = bounded(&shutdown, Duration::from_secs(30), async {
            std::future::pending::<Result<(), tonic::Status>>().await
        })
        .await;
        assert!(
            matches!(out, BoundedOutcome::TimedOut { after } if after == Duration::from_secs(30))
        );
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    /// Shutdown wins the race against an in-flight RPC — biased, so a
    /// pre-cancelled token never even polls the RPC.
    #[tokio::test(start_paused = true)]
    async fn bounded_yields_to_shutdown_biased() {
        let shutdown = token();
        shutdown.cancel();
        let polled = std::sync::atomic::AtomicBool::new(false);
        let out = bounded(&shutdown, Duration::from_secs(30), async {
            polled.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<Result<(), tonic::Status>>().await
        })
        .await;
        assert!(matches!(out, BoundedOutcome::Shutdown));
        assert!(
            !polled.load(std::sync::atomic::Ordering::SeqCst),
            "biased select: a cancelled token short-circuits the RPC"
        );
    }

    /// Shutdown mid-flight interrupts the await.
    #[tokio::test(start_paused = true)]
    async fn bounded_shutdown_interrupts_in_flight() {
        let shutdown = token();
        let shutdown_for_task = shutdown.clone();
        let task = tokio::spawn(async move {
            bounded(&shutdown_for_task, Duration::from_secs(600), async {
                std::future::pending::<Result<(), tonic::Status>>().await
            })
            .await
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        shutdown.cancel();
        let out = task.await.expect("not panicked");
        assert!(matches!(out, BoundedOutcome::Shutdown));
    }

    /// Budget arithmetic: remaining decays, attempt_bound clamps to
    /// remaining, expired flips exactly at exhaustion, and the floor
    /// keeps the final attempt non-degenerate.
    #[tokio::test(start_paused = true)]
    async fn attempt_budget_arithmetic() {
        let budget = AttemptBudget::new(Duration::from_secs(100));
        assert!(!budget.expired());
        assert_eq!(
            budget.attempt_bound(Duration::from_secs(30)),
            Duration::from_secs(30)
        );

        tokio::time::advance(Duration::from_secs(80)).await;
        assert_eq!(budget.remaining(), Duration::from_secs(20));
        assert_eq!(
            budget.attempt_bound(Duration::from_secs(30)),
            Duration::from_secs(20),
            "the last attempt is clamped to the remaining budget"
        );

        tokio::time::advance(Duration::from_secs(25)).await;
        assert!(budget.expired());
        assert_eq!(
            budget.attempt_bound(Duration::from_secs(30)),
            Duration::from_millis(1),
            "the floor keeps a post-expiry bound non-degenerate (expired() terminates the loop)"
        );
    }

    /// The grace partition: const-constructible, drain + report == total.
    #[test]
    fn grace_budget_partitions() {
        const GRACE: GraceBudget =
            GraceBudget::new(Duration::from_secs(45), Duration::from_secs(15));
        assert_eq!(GRACE.abort_drain(), Duration::from_secs(30));
        assert_eq!(GRACE.report(), Duration::from_secs(15));
        assert_eq!(GRACE.total(), Duration::from_secs(45));
        // The report slice always covers the SIGTERM final attempt.
        assert!(GRACE.report() >= SIGTERM_FINAL_ATTEMPT);
    }

    /// An inverted partition (reserve >= total) is a compile-time error
    /// in const contexts and a panic at runtime.
    #[test]
    #[should_panic(expected = "non-empty abort-drain slice")]
    fn grace_budget_rejects_inverted_partition() {
        let _ = GraceBudget::new(Duration::from_secs(10), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_open_resolves_an_answered_open() {
        let out = bounded_open(std::future::pending(), Duration::from_secs(10), async {
            Ok::<_, tonic::Status>(7)
        })
        .await;
        assert!(matches!(out, OpenOutcome::Opened(Ok(7))));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_open_times_out_a_black_hole() {
        let out = bounded_open(
            std::future::pending(),
            Duration::from_secs(10),
            std::future::pending::<Result<(), tonic::Status>>(),
        )
        .await;
        assert!(matches!(
            out,
            OpenOutcome::TimedOut {
                after
            } if after == Duration::from_secs(10)
        ));
    }

    /// W13-AA2 + the clamp cells (bug_038): the deadline-typed open
    /// bound degenerates to the fixed per-attempt bound when no
    /// envelope is armed or the envelope is ample; it clamps to the
    /// remaining envelope when that is smaller; and it saturates at
    /// zero once the envelope expired — the open gets one poll and
    /// times out, so an expired grace can never fund a fresh attempt.
    /// Paused clock: pure timer arithmetic, no IO.
    #[tokio::test(start_paused = true)]
    async fn open_deadline_clamps_to_the_armed_envelope() {
        let per_attempt = Duration::from_secs(10);
        // No envelope: the fixed bound (fast opens unchanged).
        assert_eq!(OpenDeadline::within(per_attempt, None).bound(), per_attempt);
        // Ample envelope: still the fixed bound.
        let far = tokio::time::Instant::now() + Duration::from_secs(100);
        assert_eq!(
            OpenDeadline::within(per_attempt, Some(far)).bound(),
            per_attempt
        );
        // Tight envelope: clamped to the remainder — the production
        // shape (open 10 s vs 2 s grace) that pre-fix ran the full
        // fixed bound, 5x past exit-at-expiry.
        let tight = tokio::time::Instant::now() + Duration::from_secs(2);
        assert_eq!(
            OpenDeadline::within(per_attempt, Some(tight)).bound(),
            Duration::from_secs(2)
        );
        // Expired envelope: zero — and the bounded open with a zero
        // deadline resolves TimedOut without waiting.
        tokio::time::advance(Duration::from_secs(3)).await;
        let deadline = OpenDeadline::within(per_attempt, Some(tight));
        assert_eq!(deadline.bound(), Duration::ZERO);
        let out = bounded_open_within(
            std::future::pending(),
            deadline,
            std::future::pending::<Result<(), tonic::Status>>(),
        )
        .await;
        assert!(matches!(out, OpenOutcome::TimedOut { after } if after == Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_open_abort_is_biased_first() {
        // Both ready: the abort future wins by the biased ordering, so
        // an orphaned/drained caller never consumes a fresh stream.
        let out = bounded_open(std::future::ready(()), Duration::from_secs(10), async {
            Ok::<_, tonic::Status>(())
        })
        .await;
        assert!(matches!(out, OpenOutcome::Aborted));
    }
}
