//! Per-job materialization execution: tenant re-resolution → live
//! wanted-set read → in-process reference-closure walk → pin-at-ingest
//! → outcome (substitution-replacement design §2.2 items 2–4, §5.1).
//!
//! The control loop the scheduler's `walk_substitute_closure` used to
//! be — now in-process next to the machinery it drives
//! ([`Substituter::try_substitute`], [`Substituter::check_available`],
//! the admission gate). The store substitutes ONE path per
//! `try_substitute` call (no closure walk — that is its documented
//! contract), so this module owns closure completeness.
// r[impl store.materialize.executor+3]

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use sqlx::PgPool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use rio_proto::types::{MaterializationOutcome, materialization_outcome};

use crate::substitute::Substituter;

use super::client::ClaimedJob;

/// Closure-walk node cap — the same bound the scheduler's
/// `closure_cap_exceeded` (actor/dispatch.rs) enforces, from the same
/// shared constant. A hostile upstream's reference chain must not walk
/// unboundedly on the store side either.
const CLOSURE_WALK_CAP: usize = rio_common::limits::MAX_SUBSTITUTE_CLOSURE;

/// Per-path deadline for the miss-classification probe
/// ([`Substituter::check_available`] on a single path).
const MISS_PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// What one job execution needs from the store process: the shared PG
/// pool (scheduler tables: jobs/wanted/builds/pins) and this replica's
/// substitution machinery.
pub struct ExecutorContext {
    pub pool: PgPool,
    pub substituter: std::sync::Arc<Substituter>,
}

/// Execute one claimed materialization job end-to-end and produce the
/// outcome to report.
///
/// Order (design §2.2 items 2–4, all against LIVE state — never a
/// creation-time snapshot):
///
/// 1. **Tenant re-resolution** (AS-4 / adjudication PDQ-8): the
///    recorded creating-build tenant ([`ClaimedJob::tenant_hint`]) is
///    honored only while a live interested build still carries it;
///    otherwise any live interested build's tenant; otherwise
///    `InfraFailure{no-tenant-context}` — never a silent miss, never
///    `Unobtainable`.
/// 2. **Live wanted set**: `build_wanted_outputs` of live builds,
///    saturated by the `'{}'` = all-outputs convention, mapped to
///    store paths through the derivation row's parallel arrays.
/// 3. **Closure walk**: BFS over narinfo references from the wanted
///    seeds; every path `try_substitute`d in-process; every
///    ingested/verified path pinned (`pin_kind='materialization'`)
///    BEFORE the Success report — closure-complete or no Success.
/// 4. **Final verification pass**: the wanted set is RE-READ after the
///    walk; growth re-enters the walk (the loop), so the reported
///    coverage is against execution-end live wanted, not a snapshot.
// r[impl store.materialize.executor+3]
// r[impl sched.materialize.pinning]
pub async fn execute_job(ctx: &ExecutorContext, claimed: &ClaimedJob) -> MaterializationOutcome {
    execute_job_with_progress(ctx, claimed, |_, _, _| {}).await
}

/// [`execute_job`] with a byte-progress callback (BC-4 / Phase B).
///
/// `on_progress(bytes_done, bytes_expected, upstream_uri)` fires once
/// per ingested/verified path with CUMULATIVE byte counts across the
/// job's whole closure walk (the sum of processed paths' NAR sizes so
/// far). Monotone non-decreasing in `bytes_done`, and
/// `bytes_done <= bytes_expected` at every call; the final call covers
/// the whole closure. Display-only and droppable: the callback must be
/// cheap and non-blocking (it runs on the walk); the caller forwards it
/// to `ReportMaterializationProgress` fire-and-forget.
// r[impl store.materialize.executor+3]
// r[impl obs.metric.store]
pub async fn execute_job_with_progress(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    on_progress: impl Fn(u64, u64, &str) + Send + Sync,
) -> MaterializationOutcome {
    let outcome = execute_job_inner(ctx, claimed, on_progress).await;
    // T-6.2 lifecycle counter: one increment per finished execution,
    // labeled by outcome class — the dashboard's execution rates and
    // the upstream-health signal (a rising infra/unobtainable share).
    // Sited at this single chokepoint so every return path of the
    // walk is counted exactly once.
    let label = match &outcome.outcome {
        Some(materialization_outcome::Outcome::Success(_)) => "success",
        Some(materialization_outcome::Outcome::Unobtainable(_)) => "unobtainable",
        Some(materialization_outcome::Outcome::InfraFailure(_)) | None => "infra",
        // Synthesized by the claim loop's SIGTERM arm, never by the
        // walk itself — but counted here like every other outcome so
        // the chokepoint stays total over the alphabet.
        Some(materialization_outcome::Outcome::Aborted(_)) => "aborted",
    };
    metrics::counter!("rio_store_materialization_executions_total", "outcome" => label)
        .increment(1);
    outcome
}

/// The walk body behind [`execute_job_with_progress`] (split so the
/// outcome counter has a single increment site over every return path).
async fn execute_job_inner(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    on_progress: impl Fn(u64, u64, &str) + Send + Sync,
) -> MaterializationOutcome {
    // ── 1. Tenant re-resolution (AS-4, live-interest-first) ──────────
    let tenant_id = match resolve_tenant(ctx, claimed).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            // The AS-4 posture: no resolvable tenant context is an
            // infrastructure condition (upstream selection is
            // impossible), NEVER an Unobtainable verdict and never a
            // silent skip.
            return infra_failure(format!(
                "no-tenant-context: no live interested build carries a tenant \
                 for {} (recorded hint: {:?})",
                claimed.drv_hash, claimed.tenant_hint
            ));
        }
        Err(e) => {
            return infra_failure(format!("tenant resolution query failed: {e}"));
        }
    };

    // ── 2–4. Walk loop with final-verification re-read ───────────────
    let mut visited: HashSet<String> = HashSet::new();
    let mut ingested: Vec<String> = Vec::new();
    let mut verified: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    // BC-4 cumulative progress accounting: bytes of fully-processed
    // paths. The per-path fetch callback adds the in-flight path's
    // streamed bytes on top, so reports are cumulative across the
    // closure, monotone in done, and done ≤ expected at every call.
    let mut completed_bytes: u64 = 0;

    loop {
        // The live wanted read: first iteration = the at-claim read;
        // subsequent iterations = the final verification passes
        // (design §2.2 item 3: never snapshot at creation).
        let wanted = match live_wanted_paths(ctx, claimed).await {
            Ok(w) => w,
            Err(e) => return infra_failure(format!("wanted-set query failed: {e}")),
        };
        let new_seeds: Vec<String> = wanted
            .into_iter()
            .filter(|p| !visited.contains(p))
            .collect();
        if new_seeds.is_empty() {
            // The final verification pass found no growth: coverage is
            // complete against execution-end live wanted.
            break;
        }

        let mut frontier: VecDeque<String> = new_seeds.into();
        while let Some(path) = frontier.pop_front() {
            if !visited.insert(path.clone()) {
                continue;
            }
            if visited.len() > CLOSURE_WALK_CAP {
                return infra_failure(format!(
                    "closure walk exceeded {CLOSURE_WALK_CAP} paths \
                     (hostile upstream reference chain?)"
                ));
            }
            // Present-before-substitution probe: distinguishes
            // "verified already-present" from "ingested" in the
            // Success coverage report.
            let already_present = crate::metadata::query_path_info(&ctx.pool, &path)
                .await
                .ok()
                .flatten()
                .is_some();

            match ctx.substituter.try_substitute(tenant_id, &path).await {
                Ok(Some(path_info)) => {
                    // r[impl sched.materialize.pinning]
                    // Pin-at-ingest (design §5.1): the pin lands BEFORE
                    // the path can appear in any Success report. A pin
                    // failure is an infrastructure failure — reporting
                    // Success for an unpinned path would re-open the
                    // GC-after-vouch window (B2-strong) the pin exists
                    // to close.
                    if let Err(e) = pin_materialized_path(ctx, claimed, &path).await {
                        return infra_failure(format!("pin-at-ingest failed for {path}: {e}"));
                    }
                    if already_present {
                        verified.push(path.clone());
                    } else {
                        ingested.push(path.clone());
                    }
                    // BC-4: the path is fully processed — fold its NAR
                    // size into the cumulative total and fire one
                    // progress tick per path ingest (the plan's
                    // "cumulative bytes per path" granularity: monotone
                    // non-decreasing done, done ≤ expected, the final
                    // tick covering the whole closure). Empty upstream
                    // URI: the gateway omits the "from <uri>" suffix
                    // when the field is empty.
                    completed_bytes = completed_bytes.saturating_add(path_info.nar_size);
                    on_progress(completed_bytes, completed_bytes, "");
                    // Extend the frontier with the narinfo references —
                    // the closure-completeness obligation.
                    for reference in &path_info.references {
                        let r = reference.as_str().to_string();
                        if r != path && !visited.contains(&r) {
                            frontier.push_back(r);
                        }
                    }
                }
                Ok(None) => {
                    // try_substitute folds per-upstream errors into a
                    // clean miss (one bad upstream must not block
                    // substitution). The executor needs the B3
                    // distinction — confirmed-absent routes
                    // Unobtainable, infrastructure trouble routes
                    // InfraFailure — so classify the miss with the
                    // HEAD-probe machinery.
                    match classify_miss(ctx, tenant_id, &path).await {
                        MissClass::ConfirmedAbsent => {
                            debug!(path = %path, "wanted path confirmed absent upstream");
                            missing.push(path.clone());
                        }
                        MissClass::Infra(detail) => {
                            return infra_failure(format!(
                                "substitution of {path} hit infrastructure trouble: {detail}"
                            ));
                        }
                    }
                }
                Err(e) => {
                    // B3: upstream 5xx/timeout/store-internal — nothing
                    // is confirmed; never routes from-source, never
                    // fail-fasts.
                    return infra_failure(format!("substitution of {path} failed: {e}"));
                }
            }
        }
    }

    // ── 5. Outcome ────────────────────────────────────────────────────
    if missing.is_empty() {
        info!(
            drv_hash = %claimed.drv_hash,
            ingested = ingested.len(),
            verified = verified.len(),
            "materialization job complete (closure walked, pinned, coverage held)"
        );
        MaterializationOutcome {
            outcome: Some(materialization_outcome::Outcome::Success(
                materialization_outcome::Success {
                    ingested_paths: ingested,
                    verified_paths: verified,
                },
            )),
        }
    } else {
        // Confirmed-404 wanted paths: the Unobtainable verdict carries
        // both the missing set and what WAS obtained (the scheduler's
        // routing consumes both).
        let mut covered = ingested;
        covered.extend(verified);
        warn!(
            drv_hash = %claimed.drv_hash,
            missing = missing.len(),
            covered = covered.len(),
            "materialization job unobtainable (confirmed-absent wanted paths)"
        );
        MaterializationOutcome {
            outcome: Some(materialization_outcome::Outcome::Unobtainable(
                materialization_outcome::Unobtainable {
                    cause: format!(
                        "{} wanted path(s) confirmed absent at every configured upstream",
                        missing.len()
                    ),
                    missing_paths: missing,
                    verified_paths: covered,
                },
            )),
        }
    }
}

/// Shorthand for the InfraFailure outcome.
fn infra_failure(detail: impl Into<String>) -> MaterializationOutcome {
    MaterializationOutcome {
        outcome: Some(materialization_outcome::Outcome::InfraFailure(
            materialization_outcome::InfraFailure {
                detail: detail.into(),
            },
        )),
    }
}

/// AS-4 / PDQ-8 tenant re-resolution against live interest.
///
/// The recorded creating-build tenant ([`ClaimedJob::tenant_hint`]) is
/// a preference honored only while a live interested build still
/// carries it; otherwise any live interested build's tenant; otherwise
/// `None` (the caller reports InfraFailure). A terminal creating
/// build's tenant may have been deleted (its `tenant_upstreams`
/// cascade-removed) or carry sig-trust the live interest cannot see —
/// re-resolution is the authority, never the recorded value.
async fn resolve_tenant(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
) -> Result<Option<Uuid>, sqlx::Error> {
    // 086: live interest derives from build_derivations membership
    // (the live_wanted_interest view) — a row-less live build's tenant
    // is discoverable (merged_bug_176).
    let live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT i.tenant_id \
           FROM materialization_jobs j \
           JOIN live_wanted_interest i USING (derivation_id) \
          WHERE j.drv_hash = $1 \
            AND i.tenant_id IS NOT NULL \
          ORDER BY i.tenant_id",
    )
    .bind(&claimed.drv_hash)
    .fetch_all(&ctx.pool)
    .await?;

    if let Some(hint) = claimed.tenant_hint
        && live.contains(&hint)
    {
        return Ok(Some(hint));
    }
    Ok(live.first().copied())
}

/// The live wanted set for the claimed derivation, as store paths.
///
/// Reads `build_wanted_outputs` rows of LIVE builds (the
/// materialization_interest view's predicate), unions the wanted
/// output names with the `'{}'` = all saturation convention (the 062
/// convention; the scheduler-side single home is db/wanted.rs —
/// cross-reference), and maps names → paths through the derivation
/// row's parallel `output_names` ↔ `expected_output_paths` arrays.
async fn live_wanted_paths(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(Vec<String>, Vec<String>, Vec<String>)> = sqlx::query_as(
        "SELECT d.output_names, d.expected_output_paths, i.wanted_output_names \
           FROM derivations d \
           JOIN live_wanted_interest i USING (derivation_id) \
          WHERE d.drv_hash = $1",
    )
    .bind(&claimed.drv_hash)
    .fetch_all(&ctx.pool)
    .await?;

    let Some((output_names, expected_paths, _)) = rows.first() else {
        return Ok(Vec::new());
    };

    // Union the live builds' wanted names; any '{}' saturates to all.
    let mut all = false;
    let mut union: Vec<&str> = Vec::new();
    for (_, _, wanted) in &rows {
        if wanted.is_empty() {
            all = true;
            break;
        }
        for name in wanted {
            if !union.contains(&name.as_str()) {
                union.push(name);
            }
        }
    }

    let mut paths: Vec<String> = output_names
        .iter()
        .zip(expected_paths.iter())
        .filter(|(name, path)| (all || union.contains(&name.as_str())) && !path.is_empty())
        .map(|(_, path)| path.clone())
        .collect();

    // r[impl sched.merge.stale-substitutable+3]
    // Realized-path carrier (migration 082, the floating-CA
    // stale-reset lane): the empty-path slots filtered above are the
    // floating-CA placeholders — for a carried job the realized paths
    // ride the job row, written at creation by the stale_reset origin
    // (an immutable content-addressed snapshot; the wanted NAME set
    // above stays live). The scheduler-side consumption coverage reads
    // the same column, so seed set and coverage agree by construction.
    // Unconditional since bug_233's parse-don't-validate: every
    // ClaimedJob carries a real job_id, so the carrier read can never
    // be silently skipped for a carried job.
    let carried: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT carried_realized_paths FROM materialization_jobs \
          WHERE job_id = $1",
    )
    .bind(claimed.job_id)
    .fetch_optional(&ctx.pool)
    .await?
    .flatten();
    for p in carried.unwrap_or_default() {
        if !p.is_empty() && !paths.contains(&p) {
            paths.push(p);
        }
    }

    Ok(paths)
}

/// The store-side pin-at-ingest INSERT (design §5.1).
///
/// Executes `rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL` — the
/// ONE shared text `SchedulerDb::pin_materialized_paths`
/// (rio-scheduler/src/db/live_pins.rs) also runs (bug_192; PD-13:
/// rio-store cannot link rio-scheduler, both link rio-migrations).
/// Binds 1-element arrays. Under the 093 key the materialization pin
/// is its OWN row: a build_input pin for the same (path, drv) is never
/// re-kinded (bug_253), and re-pinning refreshes job_id idempotently.
// r[impl sched.materialize.pinning]
// r[impl obs.metric.store]
async fn pin_materialized_path(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    store_path: &str,
) -> Result<(), sqlx::Error> {
    use sha2::Digest;
    let path_hash = sha2::Sha256::digest(store_path.as_bytes()).to_vec();
    sqlx::query(rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL)
        .bind(std::slice::from_ref(&path_hash))
        .bind(std::slice::from_ref(&claimed.drv_hash))
        .bind(claimed.job_id)
        .execute(&ctx.pool)
        .await?;
    // T-6.2: the pin-supply counter — pairs with the scheduler's §5.3
    // release lifecycle for pin-leak detection (a pinned-paths rate with
    // no matching release activity after jobs resolve means pins are
    // accumulating).
    metrics::counter!("rio_store_materialization_pinned_paths_total").increment(1);
    Ok(())
}

/// Classification of a `try_substitute` clean miss (B3's executor
/// half).
enum MissClass {
    /// Every configured upstream definitively answered "not present"
    /// → the Unobtainable route.
    ConfirmedAbsent,
    /// At least one upstream could not be classified (5xx / timeout /
    /// 429), or the probe itself failed → the InfraFailure route
    /// (nothing is confirmed).
    Infra(String),
}

/// Distinguish confirmed-absent from infrastructure trouble after a
/// `try_substitute` miss, using the HEAD-probe machinery
/// ([`Substituter::check_available`]): a path in `indeterminate` could
/// not be classified (infra); a path in `hits` is present upstream but
/// substitution did not ingest it (also infra — something is wrong on
/// our side); a path in neither set is a confirmed miss.
async fn classify_miss(ctx: &ExecutorContext, tenant_id: Uuid, store_path: &str) -> MissClass {
    let deadline = tokio::time::Instant::now() + MISS_PROBE_DEADLINE;
    let paths = [store_path.to_string()];
    match ctx
        .substituter
        .check_available(tenant_id, &paths, deadline)
        .await
    {
        Ok(result) => {
            if result.indeterminate.iter().any(|p| p == store_path) {
                MissClass::Infra(
                    "availability probe indeterminate (upstream 5xx/timeout/429)".to_string(),
                )
            } else if result.hits.iter().any(|p| p == store_path) {
                MissClass::Infra(
                    "upstream reports the path present but substitution did not ingest it"
                        .to_string(),
                )
            } else {
                MissClass::ConfirmedAbsent
            }
        }
        Err(e) => MissClass::Infra(format!("availability probe failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// PG + fake-upstream battery
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{self, SigMode};
    use crate::signing::Signer;
    use crate::test_helpers::seed_tenant;
    use rio_nix::narinfo::fingerprint;
    use rio_nix::store_path::StorePath;
    use rio_test_support::TestDb;
    use std::net::SocketAddr;

    // ── Fixtures ──────────────────────────────────────────────────────

    /// A store path with a DISTINCT hash part per tag (the substitute.rs
    /// fixture uses one fixed hash for every path, which collides when a
    /// fake upstream serves several paths — narinfo URLs key on the hash
    /// part).
    fn store_path(tag: u8, name: &str) -> String {
        // The REAL nixbase32 alphabet (no e/o/u/t) — an invalid char
        // fails StorePath::parse with InvalidBase32Char.
        const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let c = char::from(ALPHABET[(tag as usize) % ALPHABET.len()]);
        let hash: String = std::iter::repeat_n(c, 32).collect();
        format!("/nix/store/{hash}-{name}")
    }

    /// In-process multi-path upstream: serves narinfo + NAR for each
    /// `(store_path, nar_bytes, full_path_references)` triple, all
    /// signed by one key. References appear as basenames in the
    /// narinfo text and as full paths in the signed fingerprint (the
    /// Nix convention).
    struct FakeUpstream {
        url: String,
        trusted_key: String,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn spawn_multi_upstream(
        paths: Vec<(String, Vec<u8>, Vec<String>)>,
        key_name: &str,
    ) -> FakeUpstream {
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;

        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let mut app = Router::new().route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
        );
        for (path, nar, refs) in paths {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &refs);
            let sig = signer.sign(&fp);
            let sp = StorePath::parse(&path).unwrap();
            let hash_part = sp.hash_part();
            // narinfo references are basenames (store dir implicit).
            let ref_basenames: Vec<&str> = refs
                .iter()
                .map(|r| r.strip_prefix("/nix/store/").unwrap_or(r))
                .collect();
            let narinfo = format!(
                "StorePath: {path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: {}\n\
                 Sig: {sig}\n",
                nar.len(),
                ref_basenames.join(" ")
            );
            app = app
                .route(
                    &format!("/{hash_part}.narinfo"),
                    get(move || async move { narinfo }),
                )
                .route(
                    &format!("/nar/{hash_part}.nar"),
                    get(move || async move { nar }),
                );
        }

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    /// An upstream that answers every request with the given status
    /// (the substitute.rs `spawn_status_upstream` precedent).
    async fn spawn_status_upstream(status: axum::http::StatusCode) -> FakeUpstream {
        use axum::{Router, routing::any};
        let app = Router::new().fallback(any(move || async move { status }));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key: "unused:AAAA".to_string(),
            _task: task,
        }
    }

    /// Sandbox-safe reqwest client (the substitute.rs precedent: no CA
    /// bundle in the nix sandbox; the fake upstream is plaintext).
    fn sandbox_http() -> reqwest::Client {
        reqwest::Client::builder()
            .tls_certs_only(std::iter::empty())
            .build()
            .expect("empty-cert client build never fails")
    }

    fn make_ctx(pool: PgPool) -> ExecutorContext {
        ExecutorContext {
            substituter: std::sync::Arc::new(
                Substituter::new(pool.clone(), None).with_http_client(sandbox_http()),
            ),
            pool,
        }
    }

    /// Seed the scheduler-side rows one claimed job needs: a
    /// derivation (with output↔path arrays), a live build wanting
    /// `wanted_names` of it, a pending materialization job, and the
    /// wanted relation. Returns the ClaimedJob the executor would have
    /// been handed after a successful claim.
    struct SeededJob {
        claimed: ClaimedJob,
        build_id: Uuid,
        derivation_id: Uuid,
        job_id: Uuid,
    }

    async fn seed_job(
        pool: &PgPool,
        drv_hash: &str,
        outputs: &[(&str, &str)], // (output_name, expected_path)
        build_tenant: Option<Uuid>,
        job_tenant: Option<Uuid>,
        wanted_names: &[&str],
    ) -> SeededJob {
        let names: Vec<String> = outputs.iter().map(|(n, _)| n.to_string()).collect();
        let paths: Vec<String> = outputs.iter().map(|(_, p)| p.to_string()).collect();
        let derivation_id: Uuid = sqlx::query_scalar(
            "INSERT INTO derivations \
                 (drv_hash, drv_path, system, status, output_names, expected_output_paths) \
             VALUES ($1, $2, 'x86_64-linux', 'ready', $3, $4) \
             RETURNING derivation_id",
        )
        .bind(drv_hash)
        .bind(format!("/nix/store/{drv_hash}.drv"))
        .bind(&names)
        .bind(&paths)
        .fetch_one(pool)
        .await
        .expect("derivation seeded");

        let build_id = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_id)
            .bind(build_tenant)
            .execute(pool)
            .await
            .expect("build seeded");

        // 086: interest derives from membership; production records it
        // at merge (db/batch.rs).
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_id)
            .bind(derivation_id)
            .execute(pool)
            .await
            .expect("membership seeded");
        let wanted: Vec<String> = wanted_names.iter().map(|s| s.to_string()).collect();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, $3)",
        )
        .bind(build_id)
        .bind(derivation_id)
        .bind(&wanted)
        .execute(pool)
        .await
        .expect("wanted relation seeded");

        let job_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO materialization_jobs \
                 (job_id, derivation_id, drv_hash, tenant_id, origin, created_generation) \
             VALUES ($1, $2, $3, $4, 'cache_opportunity', 1)",
        )
        .bind(job_id)
        .bind(derivation_id)
        .bind(drv_hash)
        .bind(job_tenant)
        .execute(pool)
        .await
        .map(|_| ())
        .unwrap_or_else(|e| panic!("job seeded: {e}"));

        SeededJob {
            claimed: ClaimedJob {
                job_id,
                drv_hash: drv_hash.to_string(),
                tenant_hint: job_tenant,
                origin: "cache_opportunity".to_string(),
                exec_id: Uuid::now_v7().to_string(),
                drv_path: format!("/nix/store/{drv_hash}.drv"),
            },
            build_id,
            derivation_id,
            job_id,
        }
    }

    /// Configure `upstream` for `tenant`.
    async fn wire_upstream(pool: &PgPool, tenant: Uuid, upstream: &FakeUpstream) {
        metadata::upstreams::insert(
            pool,
            tenant,
            &upstream.url,
            50,
            std::slice::from_ref(&upstream.trusted_key),
            SigMode::Keep,
        )
        .await
        .expect("upstream wired");
    }

    /// Pin count for a drv by kind (the assertion the pinning tests use).
    async fn pin_count(pool: &PgPool, drv: &str, kind: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = $1 AND pin_kind = $2",
        )
        .bind(drv)
        .bind(kind)
        .fetch_one(pool)
        .await
        .expect("pin count")
    }

    fn outcome_success(o: &MaterializationOutcome) -> Option<&materialization_outcome::Success> {
        match o.outcome.as_ref()? {
            materialization_outcome::Outcome::Success(s) => Some(s),
            _ => None,
        }
    }

    fn outcome_unobtainable(
        o: &MaterializationOutcome,
    ) -> Option<&materialization_outcome::Unobtainable> {
        match o.outcome.as_ref()? {
            materialization_outcome::Outcome::Unobtainable(u) => Some(u),
            _ => None,
        }
    }

    fn outcome_infra(o: &MaterializationOutcome) -> Option<&materialization_outcome::InfraFailure> {
        match o.outcome.as_ref()? {
            materialization_outcome::Outcome::InfraFailure(f) => Some(f),
            _ => None,
        }
    }

    // ── The battery ───────────────────────────────────────────────────

    // r[verify store.materialize.executor+3]
    // r[verify sched.materialize.pinning]
    /// (1) The walk: BFS over narinfo references from the wanted seed
    /// path; every closure member try_substitute'd in-process; every
    /// ingested path pinned (pin_kind='materialization', job_id
    /// stamped) BEFORE the Success report exists. Closure-complete or
    /// no Success: the report covers root + transitive dep.
    #[tokio::test]
    async fn execution_walks_references_pins_and_reports_success() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-walk").await;

        // A two-node closure: root references dep.
        let root = store_path(1, "mat-root");
        let dep = store_path(2, "mat-dep");
        let (root_nar, _) = rio_test_support::fixtures::make_nar(b"root contents");
        let (dep_nar, _) = rio_test_support::fixtures::make_nar(b"dep contents");
        let upstream = spawn_multi_upstream(
            vec![
                (root.clone(), root_nar, vec![dep.clone()]),
                (dep.clone(), dep_nar, vec![]),
            ],
            "cache.walk",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-walk-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[], // '{}' = all outputs wanted
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let mut covered: Vec<&str> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .map(String::as_str)
            .collect();
        covered.sort_unstable();
        let mut expected = vec![root.as_str(), dep.as_str()];
        expected.sort_unstable();
        assert_eq!(
            covered, expected,
            "the closure walk covers the wanted root AND its narinfo reference"
        );

        // Pin-at-ingest: BOTH closure members are pinned with the
        // materialization kind and the job id.
        assert_eq!(
            pin_count(&db.pool, "mat-walk-drv", "materialization").await,
            2,
            "every ingested/verified path is pinned at ingest"
        );
        let pinned_jobs: Vec<Option<Uuid>> =
            sqlx::query_scalar("SELECT job_id FROM scheduler_live_pins WHERE drv_hash = $1")
                .bind("mat-walk-drv")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert!(
            pinned_jobs.iter().all(|j| *j == Some(seeded.job_id)),
            "pins carry the resolving job's id"
        );

        // The paths actually landed in the store (narinfo rows exist).
        for p in [&root, &dep] {
            assert!(
                crate::metadata::query_path_info(&db.pool, p)
                    .await
                    .unwrap()
                    .is_some(),
                "{p} was ingested into the store"
            );
        }
    }

    // r[verify store.materialize.executor+3]
    /// (2) The wanted set is read at execution time and RE-READ at the
    // r[verify sched.merge.stale-substitutable+3]
    /// Floating-CA stale-reset carrier (migration 082): a job whose
    /// derivation row has the empty-path placeholder (`("out", "")`)
    /// resolves its wanted set through the job row's
    /// `carried_realized_paths` — the executor fetches the realized
    /// path instead of producing the vacuous `Success{[],[]}` the
    /// empty-filtered map yields pre-fix (zero fetches, zero pins; the
    /// scheduler then "re-completed" the node with `[""]`).
    #[tokio::test]
    async fn floating_ca_carried_paths_seed_the_walk() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-fca").await;

        let real = store_path(7, "mat-fca-realized");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"fca contents");
        let upstream = spawn_multi_upstream(vec![(real.clone(), nar, vec![])], "cache.fca").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        // Floating-CA shape: the expected slot is the "" placeholder.
        let seeded = seed_job(
            &db.pool,
            "mat-fca-drv",
            &[("out", "")],
            Some(tenant),
            Some(tenant),
            &[], // '{}' = all outputs wanted
        )
        .await;
        // The stale_reset origin wrote the carrier at creation.
        sqlx::query(
            "UPDATE materialization_jobs                 SET origin = 'stale_reset', carried_realized_paths = $2               WHERE job_id = $1",
        )
        .bind(seeded.job_id)
        .bind(vec![real.clone()])
        .execute(&db.pool)
        .await
        .expect("carrier seeded");

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let covered: Vec<&str> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .map(String::as_str)
            .collect();
        assert!(
            covered.contains(&real.as_str()),
            "the carried realized path must seed the walk (pre-fix: \
             vacuous Success {{ingested: [], verified: []}}, zero \
             fetches); covered={covered:?}"
        );
        // Pin-at-ingest applies to the carried path like any other.
        assert_eq!(
            pin_count(&db.pool, "mat-fca-drv", "materialization").await,
            1,
            "the carried fetch pins at ingest"
        );
    }

    /// final verification pass — never snapshotted at creation. The
    /// job is created while only `out` is wanted; by execution time a
    /// second live build wants `dev` too; the executor fetches BOTH.
    /// Then: mid-execution growth — a third build adds `doc` while the
    /// walk runs; the final verification pass picks it up.
    #[tokio::test]
    async fn execution_rereads_wanted_at_final_verification() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-reread").await;

        let out_path = store_path(3, "mat-out");
        let dev_path = store_path(4, "mat-dev");
        let (out_nar, _) = rio_test_support::fixtures::make_nar(b"out");
        let (dev_nar, _) = rio_test_support::fixtures::make_nar(b"dev");
        let upstream = spawn_multi_upstream(
            vec![
                (out_path.clone(), out_nar, vec![]),
                (dev_path.clone(), dev_nar, vec![]),
            ],
            "cache.reread",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        // Created wanting only "out"...
        let seeded = seed_job(
            &db.pool,
            "mat-reread-drv",
            &[("out", out_path.as_str()), ("dev", dev_path.as_str())],
            Some(tenant),
            Some(tenant),
            &["out"],
        )
        .await;

        // ...but by execution time, a SECOND live build wants "dev".
        // (The creation-time snapshot would miss it; the execution-time
        // read must not.)
        let build2 = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build2)
            .bind(tenant)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, $3)",
        )
        .bind(build2)
        .bind(seeded.derivation_id)
        .bind(vec!["dev".to_string()])
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build2)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let covered: HashSet<&str> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .map(String::as_str)
            .collect();
        assert!(
            covered.contains(out_path.as_str()) && covered.contains(dev_path.as_str()),
            "execution-time wanted (out + dev) is covered, not the creation-time \
             snapshot (out only): covered = {covered:?}"
        );
    }

    // r[verify store.materialize.executor+3]
    /// (3) Tenant resolution (AS-4): job tenant NULL + no live
    /// interested build with a tenant → InfraFailure{no-tenant-context}.
    /// Never Unobtainable, never silent success.
    #[tokio::test]
    async fn no_tenant_context_reports_infra_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = store_path(5, "mat-no-tenant");

        // Job tenant NULL; the live interested build also has NULL
        // tenant (single-tenant/dev shape).
        let seeded = seed_job(
            &db.pool,
            "mat-no-tenant-drv",
            &[("out", path.as_str())],
            None,
            None,
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let infra = outcome_infra(&outcome)
            .unwrap_or_else(|| panic!("expected InfraFailure, got {outcome:?}"));
        assert!(
            infra.detail.contains("no-tenant-context"),
            "the detail names the AS-4 condition: {}",
            infra.detail
        );
        assert_eq!(
            pin_count(&db.pool, "mat-no-tenant-drv", "materialization").await,
            0,
            "nothing is pinned when nothing is fetched"
        );
    }

    // r[verify store.materialize.executor+3]
    /// (4) Stale recorded tenant (PDQ-8): the creating build is
    /// TERMINAL (its recorded tenant no longer carries live interest)
    /// while a live interested build with a DIFFERENT tenant remains →
    /// the executor fetches under the live build's tenant. Proven by
    /// upstream wiring: only the live tenant has the upstream
    /// configured, so success is only possible under re-resolution.
    #[tokio::test]
    async fn execution_stale_recorded_tenant_uses_live_interest() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let stale_tenant = seed_tenant(&db.pool, "mat-stale").await;
        let live_tenant = seed_tenant(&db.pool, "mat-live").await;

        let path = store_path(6, "mat-stale-tenant");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"stale tenant contents");
        let upstream = spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.stale").await;
        // ONLY the live tenant can reach the upstream.
        wire_upstream(&db.pool, live_tenant, &upstream).await;

        // The creating build (stale tenant, recorded on the job) is
        // TERMINAL; a different live build carries live_tenant.
        let seeded = seed_job(
            &db.pool,
            "mat-stale-drv",
            &[("out", path.as_str())],
            Some(stale_tenant),
            Some(stale_tenant),
            &[],
        )
        .await;
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(seeded.build_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let live_build = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(live_build)
            .bind(live_tenant)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(live_build)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(live_build)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let success = outcome_success(&outcome).unwrap_or_else(|| {
            panic!(
                "expected Success under the LIVE tenant's upstream (re-resolution), \
                 got {outcome:?} — the executor used the stale recorded tenant"
            )
        });
        assert_eq!(
            success.ingested_paths.len() + success.verified_paths.len(),
            1,
            "the wanted path was fetched under the live tenant"
        );
    }

    // r[verify store.materialize.executor+3]
    /// (5) A confirmed-404 wanted path (every upstream definitively
    /// answers "not present") → Unobtainable{missing_paths=[it]},
    /// with whatever WAS obtained in verified_paths.
    #[tokio::test]
    async fn confirmed_missing_reports_unobtainable() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-missing").await;

        let present = store_path(7, "mat-present");
        let absent = store_path(8, "mat-absent");
        let (present_nar, _) = rio_test_support::fixtures::make_nar(b"present");
        // The upstream serves `present` but NOT `absent` → 404 for it.
        let upstream =
            spawn_multi_upstream(vec![(present.clone(), present_nar, vec![])], "cache.miss").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-missing-drv",
            &[("out", present.as_str()), ("doc", absent.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let unobtainable = outcome_unobtainable(&outcome)
            .unwrap_or_else(|| panic!("expected Unobtainable, got {outcome:?}"));
        assert_eq!(
            unobtainable.missing_paths,
            vec![absent.clone()],
            "the confirmed-absent path is reported missing"
        );
        assert_eq!(
            unobtainable.verified_paths,
            vec![present.clone()],
            "what WAS obtained rides the same report"
        );
        assert!(
            !unobtainable.cause.is_empty(),
            "the cause string is populated"
        );
    }

    // r[verify store.materialize.executor+3]
    // r[verify store.substitute.stall-abort]
    /// (6b) A WEDGED upstream download (headers, then no body bytes)
    /// is ended by the substituter's owner-side stall abort and
    /// surfaces to the executor as **InfraFailure** — the retryable
    /// infrastructure class that feeds the scheduler's materialization
    /// re-attempt budget. Never Unobtainable (nothing confirmed
    /// absent), never Success, never a hang: the stall abort is the
    /// only clock on the NAR body.
    #[tokio::test]
    async fn stalled_download_reports_infra_failure() {
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-stall").await;

        // A signed narinfo whose NAR endpoint never sends body bytes.
        let path = store_path(12, "mat-stalled");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"never arrives");
        let seed = [0x42u8; 32];
        let signer = Signer::from_seed("cache.stall", &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "cache.stall:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        let sig = signer.sign(&fp);
        let sp = StorePath::parse(&path).unwrap();
        let hash_part = sp.hash_part();
        let narinfo = format!(
            "StorePath: {path}\n\
             URL: nar/{hash_part}.nar\n\
             Compression: none\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {}\n\
             References: \n\
             Sig: {sig}\n",
            nar.len()
        );
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(
                &format!("/{hash_part}.narinfo"),
                get(move || async move { narinfo }),
            )
            .route(
                &format!("/nar/{hash_part}.nar"),
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Vec::<u8>::new()
                }),
            );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let _task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        metadata::upstreams::insert(
            &db.pool,
            tenant,
            &format!("http://{addr}"),
            50,
            std::slice::from_ref(&trusted_key),
            SigMode::Keep,
        )
        .await
        .expect("upstream wired");

        let seeded = seed_job(
            &db.pool,
            "mat-stall-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        // 1s stall window so the abort fires within the test budget.
        let ctx = ExecutorContext {
            substituter: std::sync::Arc::new(
                Substituter::new(db.pool.clone(), None)
                    .with_http_client(sandbox_http())
                    .with_stall_window(std::time::Duration::from_secs(1)),
            ),
            pool: db.pool.clone(),
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            execute_job(&ctx, &seeded.claimed),
        )
        .await
        .expect("the stall abort must end the wedged download (no hang)");

        let infra = outcome_infra(&outcome).unwrap_or_else(|| {
            panic!("a stalled download must classify InfraFailure, got {outcome:?}")
        });
        assert!(
            infra.detail.contains("stalled"),
            "the detail names the stall: {}",
            infra.detail
        );
        assert_eq!(
            pin_count(&db.pool, "mat-stall-drv", "materialization").await,
            0,
            "nothing is pinned when nothing is ingested"
        );
    }

    // r[verify store.materialize.executor+3]
    /// (6) Upstream 5xx → InfraFailure (B3's executor half): nothing is
    /// confirmed, so the verdict must be infrastructure trouble — never
    /// Unobtainable (which would route from-source), never Success.
    #[tokio::test]
    async fn upstream_5xx_reports_infra_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-5xx").await;

        let path = store_path(9, "mat-5xx");
        let upstream = spawn_status_upstream(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-5xx-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;

        let infra = outcome_infra(&outcome).unwrap_or_else(|| {
            panic!("expected InfraFailure for a 5xx-only upstream, got {outcome:?}")
        });
        assert!(
            !infra.detail.is_empty(),
            "the infra detail is populated: {infra:?}"
        );
        assert_eq!(
            pin_count(&db.pool, "mat-5xx-drv", "materialization").await,
            0,
            "nothing is pinned when nothing is confirmed"
        );
    }

    // r[verify store.materialize.executor+3]
    /// (7) T-1.2 / BC-4: the executor reports cumulative, monotone byte
    /// progress through the callback while walking a multi-path closure.
    /// Every call satisfies done ≤ expected; done never decreases; the
    /// final call's done equals the sum of the closure's NAR sizes.
    #[tokio::test]
    async fn execution_reports_cumulative_monotone_progress() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-progress").await;

        // A two-node closure: root references dep (two NARs of
        // different sizes so the cumulative accounting is visible).
        let root = store_path(10, "mat-progress-root");
        let dep = store_path(11, "mat-progress-dep");
        let (root_nar, _) = rio_test_support::fixtures::make_nar(b"root contents with some bytes");
        let (dep_nar, _) = rio_test_support::fixtures::make_nar(b"dep");
        let total_nar_bytes = (root_nar.len() + dep_nar.len()) as u64;
        let upstream = spawn_multi_upstream(
            vec![
                (root.clone(), root_nar, vec![dep.clone()]),
                (dep.clone(), dep_nar, vec![]),
            ],
            "cache.progress",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-progress-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let calls: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());
        let outcome = execute_job_with_progress(&ctx, &seeded.claimed, |done, expected, _| {
            calls.lock().unwrap().push((done, expected));
        })
        .await;

        outcome_success(&outcome).unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));

        let calls = calls.into_inner().unwrap();
        assert!(
            !calls.is_empty(),
            "the walk must report progress through the callback (BC-4 relay source)"
        );
        let mut prev_done = 0u64;
        for (done, expected) in &calls {
            assert!(
                *done >= prev_done,
                "bytes_done must be monotone non-decreasing: {prev_done} then {done} in {calls:?}"
            );
            assert!(
                done <= expected,
                "bytes_done must never exceed bytes_expected: ({done}, {expected}) in {calls:?}"
            );
            prev_done = *done;
        }
        let (final_done, final_expected) = calls.last().expect("non-empty");
        assert_eq!(
            *final_done, total_nar_bytes,
            "the final report's done covers the whole closure ({calls:?})"
        );
        assert_eq!(
            *final_expected, total_nar_bytes,
            "the final report's expected equals the closure total ({calls:?})"
        );
    }

    // r[verify obs.metric.store]
    // r[verify store.materialize.executor+3]
    /// T-6.2 (red-first): the execution-outcome and pin counters the
    /// dashboards consume —
    ///
    ///   rio_store_materialization_executions_total{outcome}: one
    ///     increment per finished job execution, labeled
    ///     success | unobtainable | infra;
    ///   rio_store_materialization_pinned_paths_total: one increment
    ///     per path pinned at ingest (the §5.3 pin lifecycle's supply
    ///     side).
    #[tokio::test]
    async fn execution_metrics_count_outcomes_and_pins() {
        use metrics_util::debugging::DebuggingRecorder;
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-metrics").await;

        // A two-node closure: root references dep (both pinned on
        // success → pinned_paths_total = 2).
        let root = store_path(7, "matm-root");
        let dep = store_path(8, "matm-dep");
        let (root_nar, _) = rio_test_support::fixtures::make_nar(b"matm root");
        let (dep_nar, _) = rio_test_support::fixtures::make_nar(b"matm dep");
        let upstream = spawn_multi_upstream(
            vec![
                (root.clone(), root_nar, vec![dep.clone()]),
                (dep.clone(), dep_nar, vec![]),
            ],
            "cache.metrics",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded = seed_job(
            &db.pool,
            "matm-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed).await;
        assert!(
            outcome_success(&outcome).is_some(),
            "precondition: the execution succeeds, got {outcome:?}"
        );

        // The counters: one success execution, two pinned paths.
        use metrics_util::debugging::DebugValue;
        let mut executions: std::collections::BTreeMap<String, u64> = Default::default();
        let mut pinned: u64 = 0;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            let k = ck.key();
            match k.name() {
                "rio_store_materialization_executions_total" => {
                    let outcome_label = k
                        .labels()
                        .find(|l| l.key() == "outcome")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default();
                    *executions.entry(outcome_label).or_default() += c;
                }
                "rio_store_materialization_pinned_paths_total" => pinned += c,
                _ => {}
            }
        }
        assert_eq!(
            executions.get("success").copied().unwrap_or(0),
            1,
            "one success execution counted; executions: {executions:?}"
        );
        assert_eq!(
            pinned, 2,
            "both closure members pinned at ingest are counted"
        );
    }

    /// merged_bug_176 (bughunt wave): tenant re-resolution discovers a
    /// LIVE build that is a member (`build_derivations`) but has no
    /// `build_wanted_outputs` row. Pre-fix the query joined the wanted
    /// relation, so a row-less live build's tenant was invisible and
    /// the executor reported InfraFailure ("no tenant context") for a
    /// job a live build genuinely wants.
    #[tokio::test]
    async fn resolve_tenants_discovers_rowless_live_build() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "rowless").await;

        // Seed the derivation + job WITHOUT the wanted row: a build
        // that is a member only.
        let drv_hash = "rowless-tenant-drv";
        let derivation_id: Uuid = sqlx::query_scalar(
            "INSERT INTO derivations \
                 (drv_hash, drv_path, system, status, output_names, expected_output_paths) \
             VALUES ($1, $2, 'x86_64-linux', 'ready', $3, $4) \
             RETURNING derivation_id",
        )
        .bind(drv_hash)
        .bind(format!("/nix/store/{drv_hash}.drv"))
        .bind(vec!["out".to_string()])
        .bind(vec!["/nix/store/aaa-rowless".to_string()])
        .fetch_one(&db.pool)
        .await
        .expect("derivation seeded");
        let build_id = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_id)
            .bind(tenant)
            .execute(&db.pool)
            .await
            .expect("build seeded");
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_id)
            .bind(derivation_id)
            .execute(&db.pool)
            .await
            .expect("membership seeded");
        let job_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO materialization_jobs \
                 (job_id, derivation_id, drv_hash, tenant_id, origin, created_generation) \
             VALUES ($1, $2, $3, NULL, 'cache_opportunity', 1)",
        )
        .bind(job_id)
        .bind(derivation_id)
        .bind(drv_hash)
        .execute(&db.pool)
        .await
        .expect("job seeded");

        let ctx = make_ctx(db.pool.clone());
        let claimed = ClaimedJob {
            job_id,
            drv_hash: drv_hash.to_string(),
            tenant_hint: None,
            origin: "cache_opportunity".to_string(),
            exec_id: Uuid::now_v7().to_string(),
            drv_path: format!("/nix/store/{drv_hash}.drv"),
        };
        let resolved = resolve_tenant(&ctx, &claimed).await.expect("query ok");
        assert_eq!(
            resolved,
            Some(tenant),
            "a row-less live member's tenant is discoverable for the walk"
        );

        // The live wanted set saturates to all declared outputs for the
        // row-less member (the '{}' default), not to nothing.
        let paths = live_wanted_paths(&ctx, &claimed).await.expect("query ok");
        assert_eq!(
            paths,
            vec!["/nix/store/aaa-rowless".to_string()],
            "row-less live interest saturates the wanted width to all declared"
        );
    }
}
