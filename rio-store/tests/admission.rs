//! `AdmissionGate` bounded-wait semantics.
//!
//! Pure tokio (no PG) so `tokio::time::pause()` works — the spike-0.1
//! note that "PG + paused-time don't compose" applies to the actor
//! integration test (rio-scheduler/dispatch.rs), not this unit. The
//! gate itself is just a `Semaphore` + `time::timeout`.

use std::time::Duration;

use rio_store::admission::{AdmissionError, AdmissionGate, SUBSTITUTE_ADMISSION_WAIT};
use tonic::Code;

/// At capacity, the (cap+1)th caller queues for the full
/// `SUBSTITUTE_ADMISSION_WAIT`, then returns `ResourceExhausted`
/// (transient — caller retries). Paused time: advancing past 25 s
/// resolves the timeout deterministically without a 25 s wall-clock
/// test.
// r[verify store.substitute.admission]
#[tokio::test(start_paused = true)]
async fn admission_returns_re_after_wait() {
    let gate = AdmissionGate::new(2);
    // Hold both permits via the raw semaphore (bypassing the
    // bounded wait — we're not testing acquire here, just the
    // contended-third-caller path).
    let _p1 = gate.semaphore().clone().acquire_owned().await.unwrap();
    let _p2 = gate.semaphore().clone().acquire_owned().await.unwrap();
    assert_eq!(gate.semaphore().available_permits(), 0);

    // Third caller: tokio's auto-advance (start_paused=true) jumps
    // straight to the 25 s timeout once the runtime is otherwise idle.
    let started = tokio::time::Instant::now();
    let err = gate.acquire_bounded().await.expect_err("must reject");
    assert!(
        matches!(err, AdmissionError::Saturated),
        "post-wait rejection must be Saturated; got {err:?}"
    );
    assert!(
        started.elapsed() >= SUBSTITUTE_ADMISSION_WAIT,
        "must queue for the full wait before rejecting; elapsed={:?}",
        started.elapsed()
    );
    // gRPC mapping: Saturated → ResourceExhausted, which classifies
    // as transient so the scheduler's detached-fetch retry absorbs it.
    let status = tonic::Status::from(err);
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert!(rio_common::grpc::is_transient(status.code()));
}

/// Under capacity for less than the wait window, callers QUEUE rather
/// than fail. Releasing the holder before the 25 s deadline lets the
/// waiter acquire — proving `acquire_bounded` is `sem.acquire` under
/// timeout, not `try_acquire` (the rejected-at-spike-0.1 design).
// r[verify store.substitute.admission]
#[tokio::test(start_paused = true)]
async fn admission_queues_under_wait() {
    let gate = AdmissionGate::new(1);
    let p1 = gate.semaphore().clone().acquire_owned().await.unwrap();

    // Second caller parks on the semaphore. Spawn so we can release
    // the holder concurrently. `start_paused` auto-advance is gated
    // on the runtime being idle, so it won't skip past the 5 s
    // releaser — the spawned `acquire_bounded` is a live task.
    let g = gate.clone();
    let waiter = tokio::spawn(async move { g.acquire_bounded().await });

    // Release after 5 s of virtual time — well under the 25 s wait.
    tokio::time::sleep(Duration::from_secs(5)).await;
    drop(p1);

    let permit = waiter
        .await
        .expect("join")
        .expect("waiter must acquire once holder releases (queued, not rejected)");
    // Permit accounting: waiter holds it, gate is at cap again.
    assert_eq!(gate.semaphore().available_permits(), 0);
    drop(permit);
    assert_eq!(gate.semaphore().available_permits(), 1);
}

/// Permit drops on ANY return path. `acquire_bounded` returns an
/// `OwnedSemaphorePermit` (RAII), so this is structural — but the
/// gate's correctness depends on it (an error inside
/// `try_substitute_on_miss` after acquire must not leak the slot).
// r[verify store.substitute.admission]
#[tokio::test]
async fn admission_permit_released_on_error() {
    let gate = AdmissionGate::new(1);

    // Model the moka init-future's shape: acquire → do work →
    // return. The work errors; the `_permit` binding lives to
    // end-of-scope regardless.
    async fn body(gate: &AdmissionGate) -> Result<(), &'static str> {
        let _permit = gate.acquire_bounded().await.map_err(|_| "unreachable")?;
        Err("upstream substitute fetch failed")
    }

    assert_eq!(gate.semaphore().available_permits(), 1);
    let _ = body(&gate).await.expect_err("body errors");
    assert_eq!(
        gate.semaphore().available_permits(),
        1,
        "permit must release on the error path (RAII drop), not leak"
    );
    // And on the Ok path (sanity).
    let p = gate.acquire_bounded().await.expect("acquire");
    assert_eq!(gate.semaphore().available_permits(), 0);
    drop(p);
    assert_eq!(gate.semaphore().available_permits(), 1);
}

/// `utilization()` = held/capacity, clamped, and clones share state.
// r[verify store.substitute.admission]
#[tokio::test]
async fn admission_utilization_tracks_held() {
    let gate = AdmissionGate::new(4);
    let observer = gate.clone();
    assert_eq!(gate.utilization(), 0.0);
    let _p1 = gate.semaphore().clone().acquire_owned().await.unwrap();
    assert!((observer.utilization() - 0.25).abs() < f32::EPSILON);
    let _p2 = gate.semaphore().clone().acquire_owned().await.unwrap();
    let _p3 = gate.semaphore().clone().acquire_owned().await.unwrap();
    assert!((observer.utilization() - 0.75).abs() < f32::EPSILON);
    // capacity=0 doesn't divide-by-zero (clamped).
    assert_eq!(AdmissionGate::new(0).utilization(), 0.0);
}

// r[verify store.substitute.admission]
#[test]
fn admission_wait_below_grpc_timeout() {
    // If SUBSTITUTE_ADMISSION_WAIT ≥ DEFAULT_GRPC_TIMEOUT, callers see
    // DeadlineExceeded (NOT in is_transient) instead of ResourceExhausted
    // and the retry contract breaks. Same precedent as
    // breaker.rs::open_duration_covers_merge_fmp_timeout.
    assert!(
        SUBSTITUTE_ADMISSION_WAIT < rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        "SUBSTITUTE_ADMISSION_WAIT ({SUBSTITUTE_ADMISSION_WAIT:?}) must stay below \
         DEFAULT_GRPC_TIMEOUT ({:?})",
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT
    );
}
