//! End-to-end claims-floor fencing of a deposed actor's evidence
//! writes (`sched.evidence.durability`).
//!
//! The db-layer fence tests (db/tests/batch.rs, db/tests/derivations.rs)
//! pin each fenced statement in isolation; the test here pins the
//! actor-level composition: a real actor, deposed by a successor's
//! claim it never observes, drives a real evidence-writing command and
//! the whole write is a fenced no-op.

use super::*;

/// A deposed actor's evidence writes are fenced no-ops END-TO-END: a
/// successor claims a higher generation while the tenure-1 actor keeps
/// running (it never observes a lease transition — the deposed-believer
/// shape); the deposed actor's next evidence write — the admin
/// ClearPoison reset transaction here (ledger row + poison clear) —
/// must be refused by the claims-floor fence, leave PG exactly as the
/// successor's view (the poison survives), and increment
/// `rio_scheduler_evidence_write_fenced_total`.
///
/// What this test does NOT pin: the capture-point semantics. This
/// actor's lease never re-acquires, so a tenure-tracking field and a
/// fresh per-command atomic read are indistinguishable here. The
/// capture design is pinned structurally instead:
///
///  - the saturated-floor recovery test
///    (`saturated_floor_recovery_evidence_writes_land`) proves a new
///    leader's own recovery writes pass the fence (they would not,
///    under a per-command lease-atomic capture, in the saturated
///    regime);
///  - the field has exactly TWO write sites and zero per-command ones —
///    the structural grep
///    `grep -rEn "self\.serving_generation = |serving_generation: i64::try_from" rio-scheduler/src/actor/`
///    returns exactly DagActor::new's struct-literal init and
///    handle_leader_acquired's claim stamp.
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn deposed_actor_evidence_writes_are_fenced() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Tenure 1's own work lands normally: merge + poison a node (the
    // floor at this point is tenure 1's own assignment row).
    seed_poisoned(&handle, "fence-e2e").await?;
    let (status_before,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'fence-e2e'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status_before, "poisoned",
        "precondition: tenure 1's own poison evidence landed (it is at the floor)"
    );
    let fenced_before = handle.debug_counters().await?.evidence_writes_fenced;
    assert_eq!(
        fenced_before, 0,
        "precondition: nothing is fenced during normal single-leader operation \
         (a nonzero count here means the fence rejects a live leader's writes — \
         the permissiveness violation the saturated-regime tripwire also guards)"
    );

    // A successor claims generation 2: the durable floor moves above
    // this actor's tenure stamp of 1, BETWEEN two of this actor's
    // commands. The actor itself never observes any lease transition.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'successor')",
    )
    .execute(&db.pool)
    .await?;

    // The deposed believer's evidence write: admin ClearPoison drives
    // the poison-clear reset transaction. The fence must refuse it —
    // cleared=false is the admin contract for "nothing was cleared;
    // retry-safe".
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClearPoison {
            drv_hash: "fence-e2e".into(),
            reply: tx,
        })
        .await?;
    assert!(
        !rx.await?,
        "a deposed actor's ClearPoison must report cleared=false (the write was fenced)"
    );

    // PG kept the successor's view: the poison survives the stale clear.
    let (status, has_ts): (String, bool) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash = 'fence-e2e'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        status, "poisoned",
        "the deposed actor's fenced clear must not erase the poison evidence"
    );
    assert!(has_ts, "poisoned_at must survive the fenced clear");

    // The fence observable moved: the test-counter mirror of
    // rio_scheduler_evidence_write_fenced_total.
    let fenced_after = handle.debug_counters().await?.evidence_writes_fenced;
    assert!(
        fenced_after > fenced_before,
        "rio_scheduler_evidence_write_fenced_total must increment on a fenced write \
         (before={fenced_before}, after={fenced_after})"
    );

    // The deposed actor's in-memory state still has the node Poisoned:
    // handle_clear_poison's PG-first contract means a fenced (= failed)
    // clear leaves the in-memory state untouched too — consistent with
    // the PG view the successor owns.
    assert_eq!(
        handle
            .debug_query_derivation("fence-e2e")
            .await?
            .expect("the node must still be in the deposed actor's DAG")
            .status,
        DerivationStatus::Poisoned,
        "a fenced clear must not remove the node from the deposed actor's DAG"
    );
    Ok(())
}

/// The fence never rejects a live single leader: drive the same
/// ClearPoison shape with NO successor claim and assert it applies and
/// the fenced counter stays at zero. This is the permissiveness
/// companion to the deposed-actor test above (the same property the
/// recovery batteries pin at scale — stop-and-report condition 2 if it
/// ever regresses).
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn live_leader_evidence_writes_are_never_fenced() -> TestResult {
    let (db, handle, _task) = setup().await;

    seed_poisoned(&handle, "fence-live").await?;

    // No successor: the floor is this tenure's own assignment row.
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClearPoison {
            drv_hash: "fence-live".into(),
            reply: tx,
        })
        .await?;
    assert!(
        rx.await?,
        "a live leader's ClearPoison must apply (cleared=true)"
    );

    let (status, has_ts): (String, bool) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash = 'fence-live'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        status, "created",
        "the live leader's clear must apply in PG"
    );
    assert!(
        !has_ts,
        "poisoned_at must be cleared by the live leader's clear"
    );

    assert_eq!(
        handle.debug_counters().await?.evidence_writes_fenced,
        0,
        "the fence must never reject a live single leader's writes"
    );
    Ok(())
}

// ── The confirm-exit fence, totalized over EVERY keyed licensing
//    answer (merged_bug_011) ──────────────────────────────────────────

use crate::actor::pull::{PullOutcome, PullRejection};

/// One keyed build-lane `PullAssignment` (the merged_bug_145 tag
/// convention: one production pod = one token = one tag).
async fn keyed_pull(
    handle: &ActorHandle,
    intent_id: &str,
    pod: &str,
) -> Result<PullOutcome, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: intent_id.into(),
            auth_intent: Some(intent_id.into()),
            kind: rio_evidence_kernel::pull::PullKind::Build,
            executor_instance: None,
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: Some(format!("tokhash-{pod}-{intent_id}")),
            reply,
        })
        .await
        .expect("actor alive")
}

// r[verify sched.executor.confirm-fence]
// r[verify sched.executor.pull-gone+1]
/// merged_bug_011 red 1: the LIVE-loop `Gone` (confirm_only=false) is
/// the builder's exit-0 license exactly like the confirm `Gone` — the
/// fence row must be durable BEFORE the answer. Pre-fix the write-ahead
/// fired only on `confirm_only`, so the live Gone licensed exit 0 with
/// no row and left the token unfenced.
///
/// World built through production constructors only (Q1): intent
/// merged via MergeDag, terminal state via CancelBuild, the answer via
/// the real pull path — no hand-seeded fence rows.
#[tokio::test]
async fn live_gone_writes_the_fence_before_answering() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build = Uuid::new_v4();
    let _ev =
        merge_single_node(&handle, build, "fence-live-gone", PriorityClass::Scheduled).await?;
    cancel_build(&handle, build).await?;

    // The pod's first (and only) live pull: the drv is terminal — Gone.
    let outcome = keyed_pull(&handle, "fence-live-gone", "pod-straggler").await;
    assert!(
        matches!(outcome, Ok(PullOutcome::Gone(_))),
        "terminal drv answers the live pull Gone, got {outcome:?}"
    );

    // The exit-0 license must be on disk before that answer.
    let fenced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM executor_confirm_fences WHERE executor_token_sha256 = $1",
    )
    .bind("tokhash-pod-straggler-fence-live-gone")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        fenced, 1,
        "a keyed live-loop Gone must write the confirm fence before answering \
         (pre-fix: only confirm_only Gone fenced; the live Gone licensed exit 0 row-free)"
    );
    Ok(())
}

// r[verify sched.executor.confirm-fence]
/// merged_bug_011 red 2: the straggler chain. The token's live pull is
/// answered Gone (pod exits 0, Job goes Succeeded); a resubmit
/// re-readies the content-addressed drv (`intent_id == drv_hash`
/// outlives the pod, and so does the claims token); the SAME token's
/// straggler pull then admits `Ready ⇒ DeliverNew` — pre-fix the fence
/// read finds nothing and `mint_and_deliver` opens an attempt no sweep
/// can see (charged later as "unreported executor crash"); post-fix
/// the screen finds the live-Gone fence and answers Gone.
#[tokio::test]
async fn straggler_after_live_gone_is_screened() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build_a = Uuid::new_v4();
    let _ev = merge_single_node(
        &handle,
        build_a,
        "fence-straggler",
        PriorityClass::Scheduled,
    )
    .await?;
    cancel_build(&handle, build_a).await?;

    // The pod's live pull is answered Gone — its exit-0 license.
    let gone = keyed_pull(&handle, "fence-straggler", "pod-s").await;
    assert!(
        matches!(gone, Ok(PullOutcome::Gone(_))),
        "terminal drv answers Gone, got {gone:?}"
    );

    // A resubmit re-readies the same drv under a new build (the
    // resubmit-retry reset path).
    let build_b = Uuid::new_v4();
    let _ev = merge_single_node(
        &handle,
        build_b,
        "fence-straggler",
        PriorityClass::Scheduled,
    )
    .await?;

    // The straggler: the SAME token re-pulls (retry still in flight /
    // re-sent). It must be screened to Gone — its exit was declared.
    let straggler = keyed_pull(&handle, "fence-straggler", "pod-s").await;
    assert!(
        matches!(straggler, Ok(PullOutcome::Gone(_))),
        "the fenced token's straggler pull must be screened to Gone, not minted, \
         got {straggler:?}"
    );

    // And nothing was minted for it: zero open attempts.
    let assignments: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = 'fence-straggler'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        assignments, 0,
        "the screened straggler must not open an attempt (pre-fix: DeliverNew minted \
         against the re-readied drv — the invisible orphan)"
    );
    Ok(())
}

// r[verify sched.executor.confirm-fence]
/// bug_015 (SIGNED Q2) red: a DEPOSED replica cannot durably mint the
/// exit-0 license from a stale floor read. The caller-side guards
/// (`is_leader` + `max_known_generation`) are read-then-write — a
/// successor's claim landing between the handler's floor read and the
/// fence write is the exact TOCTOU `FencedTx` exists to close; the
/// injection hook (`bump_claims_floor_before_fence_write`, the
/// `fail_next_*` lane) stamps that claim deterministically inside the
/// window. Pre-fix, verbatim: the bare-pool INSERT landed and the
/// keyed confirm-only pull answered Gone — a deposed leader durably
/// licensed an exit-0 that then screened the CURRENT leader's
/// DeliverNew to Gone. Post-fix: the write transaction's OWN floor
/// check refuses (`ConfirmFenceWrite::Fenced`), nothing is written,
/// and the pull errs `StaleGeneration` (the same rejection the
/// kernel's floor check mints).
///
/// Witness strength: certifies "a below-floor replica cannot make the
/// license durable" — the capability claim itself, driven through the
/// production `pull_assignment` chain (not a db-layer shortcut).
#[tokio::test]
async fn deposed_replica_cannot_mint_the_exit0_license() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) =
        crate::actor::tests::helpers::setup_actor_configured(db.pool.clone(), None, |_, p| {
            p.bump_claims_floor_before_fence_write = true;
        });

    // A keyed confirm-only pull for an unknown intent answers Gone
    // (empty DAG answers Gone to builders) — a nothing-held licensing
    // answer, so the fence write-ahead runs. The hook stamps a
    // successor claim AFTER the handler's floor read, BEFORE the
    // write.
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "fence-toctou".into(),
            auth_intent: None,
            kind: rio_evidence_kernel::pull::PullKind::Build,
            executor_instance: None,
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: true,
            executor_token_sha256: Some("tokhash-deposed-pod".into()),
            reply,
        })
        .await?;
    assert!(
        matches!(outcome, Err(PullRejection::StaleGeneration)),
        "the deposed write must refuse with StaleGeneration (the durable \
         claims floor moved past this replica mid-handler); got {outcome:?}"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM executor_confirm_fences")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        rows, 0,
        "no fence row: the fenced transaction rolled back at the door — \
         the exit-0 license was never durably minted by the deposed replica"
    );
    Ok(())
}

// r[verify sched.executor.confirm-fence]
/// Green companion (happy-path regression for the fenced rewrite): at
/// the current generation the SAME production chain mints the witness
/// — the keyed confirm-only Gone is licensed and the fence row is
/// durable before the reply.
#[tokio::test]
async fn fenced_license_write_at_current_generation_mints_the_witness() -> TestResult {
    let (db, handle, _task) = setup().await;

    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "fence-current".into(),
            auth_intent: None,
            kind: rio_evidence_kernel::pull::PullKind::Build,
            executor_instance: None,
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: true,
            executor_token_sha256: Some("tokhash-current-pod".into()),
            reply,
        })
        .await?;
    assert!(
        matches!(outcome, Ok(crate::actor::pull::PullOutcome::Gone(_))),
        "the at-floor confirm-only pull licenses Gone; got {outcome:?}"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM executor_confirm_fences WHERE executor_token_sha256 = 'tokhash-current-pod'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        rows, 1,
        "the write-ahead fence row is durable before the reply"
    );
    Ok(())
}

// r[verify sched.executor.confirm-fence]
/// W10-R (merged_bug_098) — proposition: NO verifying executor token
/// can outlive its fence row, asserted at the MAX-LIFETIME corner
/// (not the typical case). Pre-fix `CONFIRM_FENCE_GC_SECS` was a 24h
/// literal unrelated to the credential it screens: an ExecutorClaims
/// token lives up to `MAX_HMAC_LIFETIME_SECS` (the WO-S2-2 family
/// clamp — and before that clamp, deadline+eta+300 with NO hard
/// ceiling on the configured lead time), so a fence row aged past 24h
/// while its token still verifies left a window where the sweep
/// deletes the exit-0 license and the straggler's `DeliverNew`
/// fence-read finds nothing — `mint_and_deliver` opens the
/// sweep-invisible ghost attempt the fence exists to prevent.
///
/// Drive: the production chain (merge → cancel → keyed live pull =
/// Gone, fence written write-ahead) with the row's `confirmed_at`
/// aged 25h — inside (OLD 24h horizon, max token lifetime]; the age
/// parameterizes the SWEEP INPUT (a durable-row fact, not a paused
/// clock — the claim is the horizon CONSTANT's relation to the token
/// bound, and the compile-time tie in confirm_fences.rs carries the
/// law; this test carries the composition). Then the production
/// sweep at the production constant, a resubmit re-readying the drv,
/// and the SAME token's straggler pull.
///
/// Pre-fix red (verbatim in the owning commit): the sweep deletes the
/// aged row → the straggler pull is DELIVERED (the ghost mint) and an
/// attempt row exists. Post-fix: the horizon derives from the family
/// clamp (fence ≥ every verifying token's lifetime + slack), the row
/// survives, the straggler screens to Gone, zero attempts.
#[tokio::test]
async fn fence_horizon_outlives_every_verifying_token() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build_a = Uuid::new_v4();
    let _ev =
        merge_single_node(&handle, build_a, "fence-horizon", PriorityClass::Scheduled).await?;
    cancel_build(&handle, build_a).await?;

    // The pod's live pull: Gone — the fence row is the exit-0 license.
    let gone = keyed_pull(&handle, "fence-horizon", "pod-h").await;
    assert!(matches!(gone, Ok(PullOutcome::Gone(_))), "got {gone:?}");

    // Age the durable row to 25h — past the OLD 24h literal, well
    // inside the token's family-clamped lifetime (7d).
    let aged = sqlx::query(
        "UPDATE executor_confirm_fences \
         SET confirmed_at = now() - interval '25 hours' \
         WHERE executor_token_sha256 = $1",
    )
    .bind("tokhash-pod-h-fence-horizon")
    .execute(&db.pool)
    .await?
    .rows_affected();
    assert_eq!(aged, 1, "the live-Gone fence row exists to age");

    // The production sweep at the production constant (the same call
    // housekeeping makes).
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    sched_db
        .gc_confirm_fences(crate::db::confirm_fences::CONFIRM_FENCE_GC_SECS, 512)
        .await?;

    let survives: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM executor_confirm_fences WHERE executor_token_sha256 = $1",
    )
    .bind("tokhash-pod-h-fence-horizon")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        survives, 1,
        "left (pre-fix): the 24h-literal sweep deleted the fence row while its \
         token still verifies (lifetime up to the 7d family clamp) — the \
         exit-0 license is gone but the credential is not / right: the horizon \
         derives from the credential lifetime; the row outlives every \
         verifying token"
    );

    // The ghost-mint composition: resubmit re-readies the drv; the
    // SAME token's straggler pull must screen to Gone, never mint.
    let build_b = Uuid::new_v4();
    let _ev =
        merge_single_node(&handle, build_b, "fence-horizon", PriorityClass::Scheduled).await?;
    let straggler = keyed_pull(&handle, "fence-horizon", "pod-h").await;
    assert!(
        matches!(straggler, Ok(PullOutcome::Gone(_))),
        "left (pre-fix): DeliverNew's fence read found nothing and \
         mint_and_deliver DELIVERED — the sweep-invisible ghost attempt \
         (charged later as an unreported executor crash) / right: the \
         surviving fence screens the straggler to Gone; got {straggler:?}"
    );
    let assignments: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = 'fence-horizon'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(assignments, 0, "zero attempts for the fenced token's drv");
    Ok(())
}
