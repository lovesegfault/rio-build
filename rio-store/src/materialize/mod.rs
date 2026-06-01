//! Store-side materialization executor (substitution-replacement
//! design §2.2/§5).
//!
//! Each store replica runs the pull-protocol client side of
//! materialization jobs: poll the scheduler's leader for claimable
//! jobs ([`client::poll_and_claim`]), claim one open attempt per job
//! through `PullAssignment` (kind=MATERIALIZATION + the per-replica
//! [`executor_instance`] identity), execute the in-process
//! reference-closure walk against this replica's own substitution
//! machinery, pin every ingested/verified path at ingest, and report
//! the outcome through `ReportOutcome` retried until acknowledged.
//!
//! **Phase A dormancy:** everything here is reachable ONLY when
//! `materialization.enabled = true` (default `false`). Flag-off,
//! `main.rs` never spawns the executor task set and the store is
//! byte-for-byte the as-built store — the dormancy proof is the
//! unchanged store battery plus the Wave-6 VM assertion.
//!
//! **Identity** (BC-1 + the Wave-3/4 security obligations):
//! - The *credential* is the kind-attested store-service token —
//!   `ServiceClaims { caller: "rio-store" }` signed with the service
//!   HMAC key (`service_hmac_key_path`), attached per-request by
//!   [`rio_auth::hmac::ServiceTokenInterceptor`]. Executor tokens are
//!   builder/fetcher pod-class credentials and never authorize
//!   materialization operations; the scheduler rejects them.
//! - The *replica identity* (`executor_instance`) is derived from this
//!   pod's own identity ([`executor_instance`]: the `HOSTNAME` pod
//!   name, a DNS-1123 label) and validated again scheduler-side. The
//!   full token-claim binding of the instance (the scheduler verifying
//!   rather than trusting it) is a recorded Phase B obligation — it
//!   requires a ServiceClaims field addition, which is a cross-cutting
//!   rio-auth wire change (`deny_unknown_fields` skew, the bug_011
//!   class).
//!
//! Spec: `store.materialize.executor`; design §2.2 (store as pull
//! client), §5 (pin-at-ingest).
// r[impl store.materialize.executor]

pub mod client;
pub mod executor;

use std::time::Duration;

use tracing::{info, warn};

/// How long a finished job's outcome report is retried before the
/// worker gives up (the builder pull client's `REPORT_RETRY_BUDGET`
/// value — same rationale: the establishment sweep is the
/// scheduler-side backstop for an open attempt whose report never
/// landed).
const REPORT_RETRY_BUDGET: Duration = Duration::from_secs(600);

/// ±20 % jitter applied to the poll interval so a fleet of store
/// replicas doesn't poll the leader in lockstep (the builder pull
/// client's pacing discipline).
const POLL_JITTER: rio_common::backoff::Jitter = rio_common::backoff::Jitter::Proportional(0.2);

/// Spawn the materialization-executor task set:
/// `cfg.executor_concurrency` claim loops, each running
/// poll → claim → execute → report against the scheduler's
/// ExecutorService until shutdown.
///
/// **The dormancy gate lives HERE and is unit-testable:** flag-off
/// (`cfg.enabled == false`, the Phase A deployed state) this function
/// spawns NOTHING and returns 0 — the store process is byte-for-byte
/// the as-built store. main.rs calls this unconditionally; the flag
/// check is not duplicated at the call site so there is exactly one
/// tested gate.
///
/// Returns the number of claim loops spawned (0 flag-off; also 0 when
/// the scheduler address is malformed — logged, never fatal: a broken
/// materialization executor must not take down the store data plane).
// r[impl store.materialize.executor]
pub fn spawn_materialization_executor(
    cfg: crate::config::MaterializationConfig,
    pool: sqlx::PgPool,
    substituter: std::sync::Arc<crate::substitute::Substituter>,
    service_signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    shutdown: rio_common::signal::Token,
) -> usize {
    if !cfg.enabled {
        return 0;
    }
    let instance = executor_instance();
    info!(
        instance = %instance,
        concurrency = cfg.executor_concurrency,
        scheduler_addr = %cfg.scheduler_addr,
        authenticated = service_signer.is_some(),
        "materialization executor enabled; spawning claim loops"
    );
    let mut spawned = 0;
    for worker in 0..cfg.executor_concurrency {
        let transport = match client::SchedulerTransport::connect_lazy(
            &cfg.scheduler_addr,
            service_signer.clone(),
        ) {
            Ok(t) => t,
            Err(e) => {
                // Malformed address: a config bug. Loud, never fatal —
                // the store's data plane keeps serving.
                warn!(
                    scheduler_addr = %cfg.scheduler_addr, error = %e,
                    "materialization executor channel creation failed; executor disabled"
                );
                return spawned;
            }
        };
        let ctx = executor::ExecutorContext {
            pool: pool.clone(),
            substituter: std::sync::Arc::clone(&substituter),
        };
        let cfg_for_worker = cfg.clone();
        let instance_for_worker = instance.clone();
        let shutdown_for_worker = shutdown.clone();
        rio_common::task::spawn_monitored("materialization-executor", async move {
            claim_loop(
                worker,
                cfg_for_worker,
                ctx,
                transport,
                instance_for_worker,
                shutdown_for_worker,
            )
            .await;
        });
        spawned += 1;
    }
    spawned
}

/// One worker's claim loop: poll → claim (one job at a time) →
/// execute → report, with jittered pacing; shutdown-aware.
async fn claim_loop(
    worker: usize,
    cfg: crate::config::MaterializationConfig,
    ctx: executor::ExecutorContext,
    mut transport: client::SchedulerTransport,
    instance: String,
    shutdown: rio_common::signal::Token,
) {
    info!(worker, instance = %instance, "materialization claim loop started");
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        // Each worker claims at most one job per pass — concurrency is
        // the worker count, and the scheduler's one-winner arbitration
        // (per-replica composite identity) handles claim races.
        let claimed = client::poll_and_claim(&mut transport, &instance, 1).await;
        for job in claimed {
            info!(
                worker,
                drv_hash = %job.drv_hash,
                exec_id = %job.exec_id,
                origin = %job.origin,
                "materialization job claimed; executing"
            );
            let outcome = executor::execute_job(&ctx, &job).await;
            let acked = client::report_until_acked(
                &mut transport,
                &job.exec_id,
                outcome,
                REPORT_RETRY_BUDGET,
            )
            .await;
            if !acked {
                warn!(
                    worker,
                    drv_hash = %job.drv_hash,
                    exec_id = %job.exec_id,
                    "materialization outcome was never acknowledged \
                     (the scheduler's establishment sweep will close the attempt)"
                );
            }
        }
        // Jittered poll pacing, shutdown-aware.
        let interval = POLL_JITTER.apply(Duration::from_secs(cfg.poll_interval_secs.max(1)));
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// The per-replica executor identity (BC-1): the pod name.
///
/// `HOSTNAME` is set by the kubelet to the pod name — a DNS-1123 label
/// (lowercase alphanumerics + interior hyphens, ≤63 chars), which is
/// exactly the alphabet the scheduler validates `executor_instance`
/// against (the composite ExecutorId `{intent}@{instance}` must stay
/// unambiguous). `RIO_STORE_REPLICA_ID` (the pod *IP* injected for the
/// TailLog proxy) is NOT used here: an IP literal contains dots/colons
/// and is not a DNS-1123 label.
///
/// Values that fail the label check (non-k8s dev hosts with uppercase
/// or dotted hostnames) are sanitized: lowercased, invalid bytes
/// replaced with `-`, trimmed to 63 chars, stripped of edge hyphens.
/// Empty/unset falls back to `"rio-store-dev"`.
// r[impl store.materialize.executor]
pub fn executor_instance() -> String {
    let raw = std::env::var("HOSTNAME").unwrap_or_default();
    sanitize_dns1123_label(&raw)
}

/// Sanitize an arbitrary hostname into a DNS-1123 label (the
/// scheduler-side validation alphabet — keep in sync with
/// `is_dns1123_label` in rio-scheduler/src/grpc/executor_service.rs).
fn sanitize_dns1123_label(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .take(63)
        .collect();
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "rio-store-dev".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify store.materialize.executor]
    /// THE store-side dormancy proof (Phase A charter): with the flag
    /// off — the deployed default, `MaterializationConfig::default()` —
    /// the spawner spawns ZERO claim loops and returns without touching
    /// the pool, the substituter, or the network. The store process is
    /// byte-for-byte the as-built store.
    ///
    /// Flag-on (with a well-formed address) it spawns exactly
    /// `executor_concurrency` loops — proving the 0 above comes from
    /// the flag gate, not from a broken spawner.
    #[tokio::test]
    async fn executor_not_spawned_flag_off() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();

        // Flag-off (the default): nothing spawns.
        let spawned = spawn_materialization_executor(
            crate::config::MaterializationConfig::default(),
            db.pool.clone(),
            std::sync::Arc::clone(&substituter),
            None,
            shutdown.clone(),
        );
        assert_eq!(
            spawned, 0,
            "the dormancy proof: enabled=false spawns no claim loops"
        );

        // Flag-on positive control: the count comes from the flag, not
        // from a spawner that never works.
        let enabled_cfg = crate::config::MaterializationConfig {
            enabled: true,
            executor_concurrency: 3,
            poll_interval_secs: 1,
            scheduler_addr: "localhost:19001".to_string(),
        };
        let spawned = spawn_materialization_executor(
            enabled_cfg,
            db.pool.clone(),
            substituter,
            None,
            shutdown.clone(),
        );
        assert_eq!(
            spawned, 3,
            "flag-on spawns exactly executor_concurrency claim loops"
        );
        // Tear the loops down (they poll a dead address; shutdown stops
        // them at the next pacing point).
        shutdown.cancel();
    }

    /// The instance derivation produces a scheduler-acceptable DNS-1123
    /// label from every input shape: a real pod name passes through
    /// unchanged; uppercase/dotted dev hostnames are sanitized; empty
    /// falls back to the dev constant. (The Wave-4 instance-attestation
    /// obligation, Phase-A form: identity from the pod's own
    /// environment, alphabet-validated on both sides.)
    // r[verify store.materialize.executor]
    #[test]
    fn executor_instance_is_always_a_dns1123_label() {
        let is_label = |s: &str| {
            !s.is_empty()
                && s.len() <= 63
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !s.starts_with('-')
                && !s.ends_with('-')
        };

        // A real pod name is unchanged.
        assert_eq!(
            sanitize_dns1123_label("rio-store-7d4b8f9c6-x2vpl"),
            "rio-store-7d4b8f9c6-x2vpl"
        );
        // Uppercase / dots / underscores are sanitized, not rejected.
        for raw in [
            "MyDevBox.local",
            "host_with_underscores",
            "UPPER",
            "a".repeat(100).as_str(),
            "-leading-and-trailing-",
            "",
        ] {
            let label = sanitize_dns1123_label(raw);
            assert!(
                is_label(&label),
                "sanitize({raw:?}) produced a non-label: {label:?}"
            );
        }
        // Empty input → the dev fallback.
        assert_eq!(sanitize_dns1123_label(""), "rio-store-dev");
        assert_eq!(sanitize_dns1123_label("..."), "rio-store-dev");
    }
}
