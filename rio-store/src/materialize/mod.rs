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
//! **Spawn condition:** everything here runs ONLY when a
//! `scheduler_addr` is configured (PD-D2). Without one, the executor
//! task set never spawns and the store serves its data plane alone
//! (the schedulerless pure-store deployment).
//!
//! **Identity** (BC-1 + the Wave-3/4 security obligations):
//! - The *credential* is the kind-attested store-service token —
//!   `ServiceClaims { caller: "rio-store", instance: Some(<pod>) }`
//!   signed with the service HMAC key (`service_hmac_key_path`),
//!   attached per-request by
//!   [`rio_auth::hmac::ServiceTokenInterceptor::with_instance`].
//!   Executor tokens are builder/fetcher pod-class credentials and
//!   never authorize materialization operations; the scheduler rejects
//!   them.
//! - The *replica identity* (`executor_instance`) is derived from this
//!   pod's own identity ([`executor_instance`]: the `HOSTNAME` pod
//!   name, a DNS-1123 label), bound INTO the signed claims (T-5.1 —
//!   the Phase B instance-attestation obligation, discharged), and
//!   verified scheduler-side: a claim whose `executor_instance` differs
//!   from the token's bound instance is rejected, so a compromised or
//!   misconfigured replica cannot claim under another replica's
//!   identity.
//!
//! Spec: `store.materialize.executor`; design §2.2 (store as pull
//! client), §5 (pin-at-ingest).
// r[impl store.materialize.executor+4]

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
/// **The spawn gate lives HERE and is unit-testable:** with no
/// `scheduler_addr` configured this function spawns NOTHING and
/// returns 0 (PD-D2 — the schedulerless pure-store deployment).
/// main.rs calls this unconditionally; the gate is not duplicated at
/// the call site so there is exactly one tested gate.
///
/// Returns the number of claim loops spawned (0 without an address;
/// also 0 when the scheduler address is malformed — logged, never
/// fatal: a broken materialization executor must not take down the
/// store data plane).
// r[impl store.materialize.executor+4]
pub fn spawn_materialization_executor(
    cfg: crate::config::MaterializationConfig,
    pool: sqlx::PgPool,
    substituter: std::sync::Arc<crate::substitute::Substituter>,
    service_signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    shutdown: rio_common::signal::Token,
) -> usize {
    if cfg.scheduler_addr.is_empty() {
        info!("materialization executor disabled: no scheduler_addr configured");
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
    // T-6.2 (Phase B): pre-register the executor lifecycle counters at 0
    // (the gc-collect pre-registration pattern) so dashboards/alerts have
    // series from boot and the metrics-registered VM assertion sees them
    // before the first job executes. (Without a scheduler_addr this
    // whole function returned above — no executor, no series.)
    for outcome in ["success", "unobtainable", "infra", "aborted"] {
        metrics::counter!(
            "rio_store_materialization_executions_total",
            "outcome" => outcome
        )
        .absolute(0);
    }
    metrics::counter!("rio_store_materialization_pinned_paths_total").absolute(0);
    let mut spawned = 0;
    for worker in 0..cfg.executor_concurrency {
        // merged_bug_158: the concurrency unit is the WORKER, not the
        // pod — the scheduler's one-winner arbiter keys on the
        // composite {drv}@{identity}, so two workers sharing one
        // identity could both believe they hold the same attempt.
        // Mint `{pod}-w{n}` per worker for BOTH the claim field and
        // the token binding (T-5.1: claim and credential agree); a
        // restarted worker n re-claims as the same `…-w{n}`.
        let worker_instance = format!("{instance}-w{worker}");
        let transport = match client::SchedulerTransport::connect_lazy(
            &cfg.scheduler_addr,
            service_signer.clone(),
            // T-5.1: the same identity the claim loop asserts as
            // executor_instance is bound INTO every minted service
            // token, so the scheduler verifies the pair instead of
            // trusting the request field.
            &worker_instance,
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
        let instance_for_worker = worker_instance.clone();
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
async fn claim_loop<T>(
    worker: usize,
    cfg: crate::config::MaterializationConfig,
    ctx: executor::ExecutorContext,
    mut transport: T,
    instance: String,
    shutdown: rio_common::signal::Token,
) where
    T: client::MaterializeTransport + Clone + Send + Sync + 'static,
{
    info!(worker, instance = %instance, "materialization claim loop started");
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        // Each worker claims at most one job per pass — concurrency is
        // the worker count, and the scheduler's one-winner arbitration
        // (per-replica composite identity) handles claim races.
        let claimed = client::poll_and_claim(&mut transport, &instance, 1, &shutdown).await;
        for job in claimed {
            info!(
                worker,
                drv_hash = %job.drv_hash,
                exec_id = %job.exec_id,
                origin = %job.origin,
                "materialization job claimed; executing"
            );
            // BC-4 progress relay: a bounded channel + a per-job relay
            // task with its own cloned transport, so display traffic
            // never blocks the walk (try_send drops on a full queue) or
            // contends with the claim/report transport. The sender is
            // owned by the walk's callback; when the execution returns,
            // the callback (and sender) drop → the relay drains and
            // exits. Each relay send is bounded (10 s) and raced
            // against shutdown — progress is droppable by contract, so
            // TimedOut/Shutdown drop the report and Shutdown exits the
            // relay (merged_bug_189: a black-holed leader used to park
            // the relay task forever).
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<
                rio_proto::types::ReportMaterializationProgressRequest,
            >(16);
            let mut progress_transport = transport.clone();
            let shutdown_for_relay = shutdown.clone();
            rio_common::task::spawn_monitored("materialization-progress-relay", async move {
                use rio_common::transport::{BoundedOutcome, bounded};
                while let Some(req) = progress_rx.recv().await {
                    match bounded(
                        &shutdown_for_relay,
                        Duration::from_secs(10),
                        progress_transport.report_progress(req),
                    )
                    .await
                    {
                        BoundedOutcome::Shutdown => return,
                        BoundedOutcome::TimedOut { .. } | BoundedOutcome::Resolved(_) => {}
                    }
                }
            });
            let exec_id_for_progress = job.exec_id.clone();
            let execute = executor::execute_job_with_progress(
                &ctx,
                &job,
                move |bytes_done, bytes_expected, upstream| {
                    let _ = progress_tx.try_send(
                        rio_proto::types::ReportMaterializationProgressRequest {
                            exec_id: exec_id_for_progress.clone(),
                            bytes_done,
                            bytes_expected,
                            upstream_uri: upstream.to_string(),
                        },
                    );
                },
            );
            // SIGTERM aborts the walk by drop (in-flight upstream
            // fetches are torn down with their futures) and reports
            // the NEW Aborted outcome through the single bounded
            // SIGTERM attempt — the scheduler closes the attempt
            // charge-free (AD5 parity, owner default Q3) instead of
            // letting the charged establishment sweep classify a
            // routine rollout as infrastructure failure.
            let outcome = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!(
                        worker,
                        drv_hash = %job.drv_hash,
                        exec_id = %job.exec_id,
                        "SIGTERM during the materialization walk: aborting and reporting Aborted"
                    );
                    rio_proto::types::MaterializationOutcome {
                        outcome: Some(
                            rio_proto::types::materialization_outcome::Outcome::Aborted(
                                rio_proto::types::materialization_outcome::Aborted {
                                    detail: "walk aborted by SIGTERM (store shutdown/rollout)"
                                        .into(),
                                },
                            ),
                        ),
                    }
                }
                outcome = execute => outcome,
            };
            let acked = client::report_until_acked(
                &mut transport,
                &job.exec_id,
                outcome,
                REPORT_RETRY_BUDGET,
                &shutdown,
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
// r[impl store.materialize.executor+4]
pub fn executor_instance() -> String {
    let raw = std::env::var("HOSTNAME").unwrap_or_default();
    sanitize_dns1123_label(&raw)
}

/// FNV-1a 64 over the raw identity — the deterministic disambiguation
/// salt for sanitized labels (the same raw always maps to the same
/// identity across restarts; distinct raws that fold to the same
/// sanitized base get distinct salts).
fn fnv1a_64(raw: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Sanitize an arbitrary hostname into a DNS-1123 label (the
/// scheduler-side validation alphabet — keep in sync with
/// `is_dns1123_label` in rio-scheduler/src/grpc/executor_service.rs).
///
/// merged_bug_158: sanitization must stay injective-enough — `Host_A`,
/// `host-a` and `host.a` all used to fold to `host-a`, and two
/// replicas folding to one label can both claim under the same
/// composite identity. When sanitization ALTERS the raw, a 4-hex
/// FNV-1a salt of the raw is appended (base truncated to 58 so the
/// result stays ≤63); an empty/garbage raw gets a per-process random
/// salt with a loud warning (two unidentifiable replicas must still
/// not share an identity).
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
        // No usable identity at all: salt randomly (per process) so two
        // such replicas never collide, and say so loudly.
        use std::hash::{BuildHasher, Hasher};
        let nonce = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        warn!(
            raw,
            "HOSTNAME provides no usable identity; using a random-salted dev identity"
        );
        return format!("rio-store-dev-{:04x}", nonce & 0xffff);
    }
    if out == raw {
        return out;
    }
    // Sanitization altered the raw: disambiguate with a deterministic
    // salt so distinct raws cannot fold to one label.
    out.truncate(58);
    let out = out.trim_matches('-');
    format!("{out}-{:04x}", fnv1a_64(raw) & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify store.materialize.executor+4]
    /// PD-D2 spawn condition: no `scheduler_addr` → zero claim loops
    /// (the schedulerless pure-store deployment); a configured address
    /// spawns exactly `executor_concurrency` loops (connect_lazy — no
    /// scheduler needs to be listening).
    #[tokio::test]
    async fn executor_spawns_iff_scheduler_addr_configured() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();

        let mut cfg = crate::config::MaterializationConfig::default();
        assert!(cfg.scheduler_addr.is_empty(), "default: no address");
        let spawned = spawn_materialization_executor(
            cfg.clone(),
            db.pool.clone(),
            std::sync::Arc::clone(&substituter),
            None,
            shutdown.clone(),
        );
        assert_eq!(spawned, 0, "no scheduler_addr => no executor (PD-D2)");

        // Positive control: an address spawns the configured loops
        // (connect_lazy — no scheduler needs to be listening).
        cfg.scheduler_addr = "http://127.0.0.1:1".into();
        cfg.executor_concurrency = 3;
        let spawned = spawn_materialization_executor(
            cfg,
            db.pool.clone(),
            substituter,
            None,
            shutdown.clone(),
        );
        assert_eq!(spawned, 3, "an address spawns the configured loops");
        shutdown.cancel();
    }

    /// The instance derivation produces a scheduler-acceptable DNS-1123
    /// label from every input shape: a real pod name passes through
    /// unchanged; uppercase/dotted dev hostnames are sanitized; empty
    /// falls back to the dev constant. (The Wave-4 instance-attestation
    /// obligation, Phase-A form: identity from the pod's own
    /// environment, alphabet-validated on both sides.)
    // r[verify store.materialize.executor+4]
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

        // A real pod name is unchanged (already a valid label — no salt).
        assert_eq!(
            sanitize_dns1123_label("rio-store-7d4b8f9c6-x2vpl"),
            "rio-store-7d4b8f9c6-x2vpl"
        );
        // Uppercase / dots / underscores are sanitized, not rejected —
        // and stay valid labels with the disambiguation salt appended.
        for raw in [
            "MyDevBox.local",
            "host_with_underscores",
            "UPPER",
            "a".repeat(100).as_str(),
            "-leading-and-trailing-",
            "",
            "...",
        ] {
            let label = sanitize_dns1123_label(raw);
            assert!(
                is_label(&label),
                "sanitize({raw:?}) produced a non-label: {label:?}"
            );
        }
        // merged_bug_158: identities that USED to fold to one label are
        // now distinct (deterministic FNV salt over the raw).
        let a = sanitize_dns1123_label("Host_A");
        let b = sanitize_dns1123_label("host-a");
        let c = sanitize_dns1123_label("host.a");
        assert_eq!(
            b, "host-a",
            "an already-valid label passes through unsalted"
        );
        assert_ne!(a, b, "Host_A no longer folds onto host-a");
        assert_ne!(c, b, "host.a no longer folds onto host-a");
        assert_ne!(a, c, "distinct raws get distinct salts");
        // Determinism: the same raw maps to the same identity across
        // restarts (re-claims must resume as the same identity).
        assert_eq!(a, sanitize_dns1123_label("Host_A"));
        // Empty/garbage input → the dev fallback, randomly salted so two
        // unidentifiable replicas still cannot share an identity.
        let e = sanitize_dns1123_label("");
        assert!(
            e.starts_with("rio-store-dev-"),
            "empty input gets the salted dev fallback: {e:?}"
        );
    }

    /// merged_bug_189 (the claim-loop SIGTERM arm): SIGTERM landing
    /// after a claim delivers aborts the walk (the execute future is
    /// never driven — biased select) and reports exactly one
    /// MaterializationOutcome::Aborted through the bounded SIGTERM
    /// attempt, then the loop exits. (The pre-fix loop had no shutdown
    /// arm around execute at all and report_until_acked took no
    /// shutdown — this shape was unexpressible: compile-level red.)
    // r[verify store.materialize.executor+4]
    #[tokio::test]
    async fn claim_loop_sigterm_aborts_walk_and_reports_aborted() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct SigtermAfterClaim {
            shutdown: rio_common::signal::Token,
            reports: Arc<Mutex<Vec<rio_proto::types::ReportOutcomeRequest>>>,
        }
        impl client::MaterializeTransport for SigtermAfterClaim {
            async fn list_jobs(
                &mut self,
                _req: rio_proto::types::ListMaterializationJobsRequest,
            ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status>
            {
                Ok(rio_proto::types::ListMaterializationJobsResponse {
                    jobs: vec![rio_proto::types::MaterializationJobDescriptor {
                        job_id: uuid::Uuid::now_v7().to_string(),
                        drv_hash: "sigterm-drv".into(),
                        tenant_id: String::new(),
                        origin: "cache_opportunity".into(),
                    }],
                })
            }
            async fn pull(
                &mut self,
                _req: rio_proto::types::PullAssignmentRequest,
            ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
                // SIGTERM lands right after the claim delivers.
                self.shutdown.cancel();
                Ok(rio_proto::types::PullAssignmentResponse {
                    outcome: Some(
                        rio_proto::types::pull_assignment_response::Outcome::Assignment(
                            rio_proto::types::WorkAssignment {
                                drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv"
                                    .into(),
                                exec_id: "exec-sigterm-1".into(),
                                ..Default::default()
                            },
                        ),
                    ),
                })
            }
            async fn report(
                &mut self,
                req: rio_proto::types::ReportOutcomeRequest,
            ) -> Result<(), tonic::Status> {
                self.reports.lock().unwrap().push(req);
                Ok(())
            }
            async fn report_progress(
                &mut self,
                _req: rio_proto::types::ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let transport = SigtermAfterClaim {
            shutdown: shutdown.clone(),
            reports: Arc::clone(&reports),
        };
        let ctx = executor::ExecutorContext {
            pool: db.pool.clone(),
            substituter,
        };
        tokio::time::timeout(
            Duration::from_secs(30),
            claim_loop(
                0,
                crate::config::MaterializationConfig::default(),
                ctx,
                transport,
                "store-replica-0-w0".into(),
                shutdown,
            ),
        )
        .await
        .expect("the loop must exit promptly after the SIGTERM-aborted job");

        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 1, "exactly one Aborted report");
        assert_eq!(reports[0].exec_id, "exec-sigterm-1");
        match &reports[0].materialization_outcome {
            Some(rio_proto::types::MaterializationOutcome {
                outcome: Some(rio_proto::types::materialization_outcome::Outcome::Aborted(aborted)),
            }) => {
                assert!(aborted.detail.contains("SIGTERM"));
            }
            other => panic!("expected the Aborted outcome, got {other:?}"),
        }
    }
}
