//! Build CRD reconciler: K8s-native build submission.
//!
//! Apply → SubmitBuild → spawn a watch task that drains the event
//! stream and patches `.status`. Cleanup → CancelBuild.
// r[impl ctrl.crd.build]
// r[impl ctrl.build.sentinel]
//!
//! # Single-node DAG (phase3a scope)
//!
//! `SubmitBuildRequest` wants a full derivation DAG (nodes +
//! edges). The gateway builds this via BFS over `input_drvs`,
//! fetching each transitive .drv from the store. That's
//! `reconstruct_dag` — ~200 lines + per-input store RPCs.
//!
//! For phase3a we submit a SINGLE-node DAG: just the root .drv,
//! zero edges. This works when the full closure is already in
//! rio-store (which the CRD docs REQUIRE — "upload via `nix copy
//! --to ssh-ng://` first"). The scheduler sees no children →
//! derivation immediately Ready → dispatched → worker's synth_db
//! resolves inputDrv outputs from the store (they're there,
//! that's the prerequisite).
//!
//! What this CAN'T do: build a .drv whose inputDrvs aren't
//! already built. That's `nix build`'s job, not `nix copy`'s.
//! Phase 4 deferral: full DAG reconstruction in the controller
//! (or a scheduler-side `SubmitBuildByDrvPath` RPC that does it
//! server-side — TBD which).
//!
//! # Idempotence
//!
//! Reconcile is NOT "call SubmitBuild every time" — that'd spawn
//! a new build per reconcile. Gate on `.status.build_id`: empty
//! → first apply, do the submit. Non-empty → already submitted,
//! the watch task is running (or died; status stops updating,
//! operator sees stale Progress). `await_change()` either way.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::{CustomResourceExt, ResourceExt};
use rio_proto::types::{self, build_event::Event as BuildEv};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use crate::crds::build::{Build, BuildStatus};
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::Ctx;

const FINALIZER: &str = "rio.build/build-cleanup";
const MANAGER: &str = "rio-controller";

/// .drv files are small (KB range). 256 KiB is the scheduler's
/// defensive per-node cap; matching here means a .drv that'd be
/// rejected anyway doesn't waste a GB of memory first.
const MAX_DRV_NAR_SIZE: u64 = 256 * 1024;

/// Fetch timeout. .drv files are tiny — 30s is generous. A stuck
/// store shouldn't block the reconciler indefinitely (other
/// Builds queue behind this one in the controller's work queue).
const DRV_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[tracing::instrument(
    skip(b, ctx),
    fields(reconciler = "build", build = %b.name_any(), ns = b.namespace().as_deref().unwrap_or(""))
)]
pub async fn reconcile(b: Arc<Build>, ctx: Arc<Ctx>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(b, ctx).await;
    metrics::histogram!("rio_controller_reconcile_duration_seconds",
        "reconciler" => "build")
    .record(start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(b: Arc<Build>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = b
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("Build has no namespace".into()))?;
    let api: Api<Build> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER, b, |event| async {
        match event {
            Event::Apply(b) => apply(b, &ctx).await,
            Event::Cleanup(b) => cleanup(b, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn apply(b: Arc<Build>, ctx: &Ctx) -> Result<Action> {
    let ns = b.namespace().expect("checked in reconcile()");
    let name = b.name_any();
    // Watch dedup key. drain_stream patches status → API server
    // emits watch event → reconcile → apply() runs again. Without
    // this gate, each cycle spawns a duplicate drain_stream.
    let watch_key = format!("{ns}/{name}");

    // Idempotence gate. build_id non-empty → already submitted.
    // Phase 3b: if phase is non-terminal, reconnect the watch via
    // WatchBuild(build_id, since_sequence=status.last_sequence).
    // Scheduler replays from build_event_log for events > that
    // sequence. The watch task died (controller restart, panic);
    // reconnecting resumes status updates.
    //
    // If phase IS terminal: no reconnect needed, the last status
    // was patched before the watch task exited. await_change.
    if let Some(status) = &b.status
        && !status.build_id.is_empty()
    {
        // Check for REAL build_id (not the "submitted" sentinel —
        // that means apply() set it but SubmitBuild hasn't
        // responded yet, no build_id to WatchBuild with). We don't
        // bother validating UUID format — if the scheduler gave
        // us a non-UUID, WatchBuild will just return NotFound.
        let is_real_uuid = status.build_id != "submitted";
        let is_terminal = matches!(status.phase.as_str(), "Succeeded" | "Failed" | "Cancelled");

        // X2 fix: orphaned sentinel check. If build_id=="submitted"
        // AND no watch is running for this Build (watching doesn't
        // contain the key), the previous apply's watch died before
        // the first event (scheduler crashed between MergeDag and
        // BuildStarted, OR the SubmitBuild stream dropped). No
        // external trigger will unstick this — we can't WatchBuild(
        // build_id="submitted"), that's not a real UUID.
        //
        // If watching DOES contain the key, the watch is still
        // alive and trying (its reconnect loop) — let it work.
        let orphaned_sentinel = !is_real_uuid && !ctx.watching.contains_key(&watch_key);

        if orphaned_sentinel {
            // Fall through to the "first apply" path below — we'll
            // resubmit. The scheduler's idempotence (MergeDag on
            // existing DAG is a no-op) means this is safe even if
            // the original SubmitBuild DID succeed and we just lost
            // the stream.
            info!(build = %name, "orphaned 'submitted' sentinel with no watch; resubmitting");
        } else if is_real_uuid && !is_terminal {
            // Dedup: if drain_stream already running for this Build,
            // don't spawn another. The running task will handle
            // status updates; this reconcile was triggered by its
            // own status patch.
            if ctx.watching.contains_key(&watch_key) {
                debug!(build = %name, "watch already running, skipping reconnect spawn");
                return Ok(Action::await_change());
            }
            // Reconnect: WatchBuild + spawn fresh drain_stream.
            // since_sequence from status.last_sequence (0 for
            // pre-Phase-3b rows = replay all, safe default).
            info!(
                build = %name,
                build_id = %status.build_id,
                since_seq = status.last_sequence,
                "reconnecting WatchBuild (controller restart or watch died)"
            );
            let sched = rio_proto::client::connect_scheduler(&ctx.scheduler_addr)
                .await
                .map_err(|e| {
                    Error::SchedulerUnavailable(tonic::Status::unavailable(e.to_string()))
                })?;
            ctx.watching.insert(watch_key.clone(), ());
            metrics::counter!("rio_controller_build_watch_spawns_total").increment(1);
            spawn_reconnect_watch(
                name.clone(),
                Api::namespaced(ctx.client.clone(), &ns),
                sched,
                status.build_id.clone(),
                status.last_sequence as u64,
                ctx.scheduler_addr.clone(),
                ctx.watching.clone(),
                watch_key,
            );
            return Ok(Action::await_change());
        } else {
            // Sentinel with watch still alive, OR terminal.
            debug!(build = %name, build_id = %status.build_id, "already submitted");
            return Ok(Action::await_change());
        }
    }

    // ---- First apply: fetch .drv, build node, submit ----
    info!(build = %name, drv = %b.spec.derivation, "submitting");

    // Lazy connect. Two separate connections (store for the .drv
    // fetch, scheduler for SubmitBuild) rather than holding them
    // in Ctx — a transient outage during ONE Build's submit
    // shouldn't affect other reconciles. connect_* already
    // handles retry/timeout internally.
    let mut store = rio_proto::client::connect_store(&ctx.store_addr)
        .await
        .map_err(|e| Error::SchedulerUnavailable(tonic::Status::unavailable(e.to_string())))?;
    let mut sched = rio_proto::client::connect_scheduler(&ctx.scheduler_addr)
        .await
        .map_err(|e| Error::SchedulerUnavailable(tonic::Status::unavailable(e.to_string())))?;

    let node = fetch_and_build_node(&mut store, &b.spec.derivation).await?;

    // Single-node DAG. See module docs. Zero edges = leaf → the
    // scheduler's cache-check dispatches immediately (assuming
    // outputs aren't already in store; if they are, instant
    // success).
    //
    // tenant: CRD uses string, scheduler wants UUID string.
    // Pass through as-is — if it's not a valid UUID the
    // scheduler rejects with InvalidArgument and we surface that
    // in a Condition. Empty = untenanted.
    let req = types::SubmitBuildRequest {
        tenant_id: b.spec.tenant.clone().unwrap_or_default(),
        priority_class: priority_to_class(b.spec.priority).to_string(),
        nodes: vec![node],
        edges: vec![],
        max_silent_time: 0,
        build_timeout: b.spec.timeout_seconds.max(0) as u64,
        build_cores: 0,
        keep_going: false,
    };

    let stream = sched.submit_build(req).await?.into_inner();

    let api: Api<Build> = Api::namespaced(ctx.client.clone(), &ns);

    // ---- Sentinel patch FIRST, then spawn watch ----
    // Set phase=Pending immediately so the idempotence gate
    // above doesn't fire AGAIN if we're re-reconciled before the
    // first BuildStarted arrives. build_id is set by the watch
    // task (from BuildEvent.build_id — the scheduler assigns it).
    // But there's a race: this reconcile returns → controller
    // re-enqueues (finalizer added triggers a change event) →
    // apply() runs again → build_id still empty → DOUBLE SUBMIT.
    //
    // Close it: set build_id to a SENTINEL ("submitted") here.
    // The watch task overwrites with the real UUID on first
    // event. The idempotence gate checks !is_empty(), not
    // validity — sentinel passes.
    //
    // ORDER MATTERS: the sentinel patch MUST land before the
    // watch task's first patch. Previously the spawn came first,
    // which meant a fast build's first event (BuildStarted with
    // real UUID) could race the sentinel: drain_stream patches
    // {phase:Building, build_id:<uuid>}, then apply() overwrites
    // with {phase:Pending, build_id:"submitted"}. For a build
    // that completes before the next event (already cached), the
    // CRD would be stuck at Pending/submitted despite success.
    // Awaiting this patch first eliminates the race.
    patch_status(
        &api,
        &name,
        BuildStatus {
            phase: "Pending".into(),
            build_id: "submitted".into(),
            ..Default::default()
        },
    )
    .await?;

    // ---- Spawn watch task ----
    // Runs until the stream ends (build terminal) or PATCH fails
    // (RBAC revoked, apiserver gone). spawn_monitored: if it
    // panics, logged; the reconciler keeps going (this Build's
    // status goes stale, other Builds unaffected).
    //
    // The task owns the stream. We DON'T await it here —
    // returning from apply() lets the controller move on to the
    // next reconcile. The stream drains in the background.
    //
    // scheduler_addr cloned in for reconnect on stream error.
    //
    // Dedup gate: should already be empty (first apply, build_id
    // was empty above), but guard anyway — the sentinel patch
    // above triggers a reconcile, and if that races with THIS
    // code path (unlikely, but reconcile queue is async) we'd
    // double-spawn.
    if ctx.watching.contains_key(&watch_key) {
        debug!(build = %name, "watch already running, skipping initial spawn");
        return Ok(Action::await_change());
    }
    ctx.watching.insert(watch_key.clone(), ());
    metrics::counter!("rio_controller_build_watch_spawns_total").increment(1);
    rio_common::task::spawn_monitored(
        "build-watch",
        drain_stream(
            name.clone(),
            api,
            stream,
            ctx.scheduler_addr.clone(),
            // since_seq=0 for initial connect — this IS the
            // SubmitBuild stream, it starts from the beginning.
            0,
            // No known build_id yet — scheduler assigns it on
            // MergeDag, first event carries it. (X11)
            None,
            ctx.watching.clone(),
            watch_key,
        ),
    );

    Ok(Action::await_change())
}

/// Reconnect: call WatchBuild with since_sequence, spawn
/// drain_stream on the resulting stream. Used by apply()'s
/// idempotence gate when build_id is real + phase non-terminal
/// (controller restarted mid-build).
///
/// Fire-and-forget like the initial drain_stream spawn. If
/// WatchBuild fails (scheduler down, unknown build_id after
/// recovery didn't find it), logs and exits — status stays stale.
/// Next controller restart retries.
#[allow(clippy::too_many_arguments)]
fn spawn_reconnect_watch(
    name: String,
    api: Api<Build>,
    mut sched: rio_proto::SchedulerServiceClient<tonic::transport::Channel>,
    build_id: String,
    since_seq: u64,
    scheduler_addr: String,
    watching: Arc<dashmap::DashMap<String, ()>>,
    watch_key: String,
) {
    rio_common::task::spawn_monitored("build-watch-reconnect", async move {
        // No outer scopeguard here (X14 fix). drain_stream has its
        // own guard (build.rs:~590) that removes the watching entry.
        // A double guard (outer + inner) creates a race: inner drops
        // → new reconcile inserts → outer drops stale removal. The
        // ONLY exit path where drain_stream's guard doesn't run is
        // WatchBuild-fails-immediately (Err branch below) — we
        // handle that explicitly with watching.remove().
        match sched
            .watch_build(types::WatchBuildRequest {
                build_id: build_id.clone(),
                since_sequence: since_seq,
            })
            .await
        {
            Ok(resp) => {
                info!(build = %name, %build_id, since_seq, "WatchBuild reconnect ok");
                drain_stream(
                    name,
                    api,
                    resp.into_inner(),
                    scheduler_addr,
                    since_seq,
                    // Pass known build_id (X11 fix): we HAVE it
                    // (from CRD status). Without this, stream-
                    // error-before-first-event → status.build_id
                    // empty → drain_stream exits → status stale
                    // until controller restart.
                    Some(build_id),
                    watching,
                    watch_key,
                )
                .await;
                // drain_stream's guard removed the entry on exit.
            }
            Err(e) => {
                // Scheduler down, or build not found (recovery
                // didn't reconstruct it — PG was cleared, or
                // it's from a very old pre-recovery scheduler).
                // Log and exit; status stays stale. Next restart
                // retries.
                warn!(build = %name, %build_id, error = %e,
                      "WatchBuild reconnect failed; status will stop updating");
                // Manual cleanup for the WatchBuild-fails path.
                // drain_stream never ran → its guard didn't fire.
                watching.remove(&watch_key);
            }
        }
    });
}

async fn cleanup(b: Arc<Build>, ctx: &Ctx) -> Result<Action> {
    let name = b.name_any();
    let ns = b.namespace().unwrap_or_default();

    // X15: remove watching entry so a delete-then-recreate with
    // the same name isn't blocked by a stale entry. The in-flight
    // drain_stream for the old Build will PATCH-fail on the deleted
    // CRD and exit on its own (its own guard removes the entry too
    // — this is idempotent, removing twice is a no-op).
    let watch_key = format!("{ns}/{name}");
    ctx.watching.remove(&watch_key);

    // Read build_id from status. If never submitted (empty) or
    // still the sentinel (watch never got the first event),
    // there's nothing to cancel — the scheduler doesn't know
    // about us.
    let build_id = b.status.as_ref().map(|s| s.build_id.as_str()).unwrap_or("");
    if build_id.is_empty() || build_id == "submitted" {
        info!(build = %name, "cleanup: never submitted (nothing to cancel)");
        return Ok(Action::await_change());
    }

    info!(build = %name, %build_id, "cancelling");
    // Best-effort. Scheduler down → log and proceed — blocking
    // delete on a down scheduler means the CRD sticks around
    // until the scheduler comes back, which is operationally
    // annoying (can't `kubectl delete -f build.yaml` during an
    // outage). The scheduler's terminal-cleanup eventually reaps
    // the zombie build anyway (60s delay + DAG reap).
    match rio_proto::client::connect_scheduler(&ctx.scheduler_addr).await {
        Ok(mut sched) => {
            let res = sched
                .cancel_build(types::CancelBuildRequest {
                    build_id: build_id.to_string(),
                    reason: "k8s Build CRD deleted".into(),
                })
                .await;
            match res {
                Ok(resp) => {
                    debug!(build = %name, cancelled = resp.into_inner().cancelled, "CancelBuild OK");
                }
                Err(e) if e.code() == tonic::Code::NotFound => {
                    // Build already terminal (cleaned up). Fine.
                    debug!(build = %name, "scheduler: build not found (already terminal)");
                }
                Err(e) => {
                    warn!(build = %name, error = %e, "CancelBuild failed (proceeding with delete anyway)");
                }
            }
        }
        Err(e) => {
            warn!(build = %name, error = %e, "scheduler unreachable during cleanup (proceeding anyway)");
        }
    }

    Ok(Action::await_change())
}

pub fn error_policy(_b: Arc<Build>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    // Per observability.md:133. error_kind uses the variant
    // discriminator — coarse but stable (won't change when inner
    // error messages do). Dashboards can slice on kind without
    // getting a cardinality explosion from dynamic messages.
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => "build", "error_kind" => error_kind(err))
    .increment(1);

    match err {
        Error::InvalidSpec(msg) => {
            warn!(error = %msg, "invalid Build spec; fix the CRD");
            Action::requeue(Duration::from_secs(300))
        }
        _ => {
            // warn! not debug! — a 30s silent retry loop at debug
            // is invisible at INFO and cost us ~10min of vm-phase3a
            // debugging once (workerpool.rs had the fix + this
            // comment; build.rs never got it).
            warn!(error = %err, "Build reconcile failed; retrying");
            Action::requeue(Duration::from_secs(30))
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Fetch a .drv from rio-store and construct the corresponding
/// `DerivationNode`. Same logic as gateway's `derivation_to_node`
/// but sourced from rio-store (GetPath NAR) rather than the
/// session's per-client .drv cache.
async fn fetch_and_build_node(
    store: &mut rio_proto::StoreServiceClient<Channel>,
    drv_path: &str,
) -> Result<types::DerivationNode> {
    // Fetch. `get_path_nar` already handles the stream chunking
    // + timeout + size bound. 256 KiB is plenty for a .drv
    // (typically 1-10 KB; largest stdenv .drvs are ~50 KB).
    let result =
        rio_proto::client::get_path_nar(store, drv_path, DRV_FETCH_TIMEOUT, MAX_DRV_NAR_SIZE)
            .await
            .map_err(|e| {
                Error::SchedulerUnavailable(tonic::Status::unavailable(format!(
                    "GetPath({drv_path}): {e}"
                )))
            })?;

    let Some((_, nar_data)) = result else {
        // .drv not in store. The CRD docs say "upload via nix
        // copy first" — user forgot. InvalidSpec (not a
        // transient error; retrying won't help until they fix
        // it). Slow requeue via error_policy.
        return Err(Error::InvalidSpec(format!(
            ".drv not found in store: {drv_path} (upload via `nix copy --to ssh-ng://` first)"
        )));
    };

    // Parse. extract_single_file + UTF-8 + ATerm. All in
    // parse_from_nar. Errors here are malformed .drv content —
    // also InvalidSpec (the store accepted bad bytes somehow, or
    // the path isn't actually a .drv).
    let drv = rio_nix::derivation::Derivation::parse_from_nar(&nar_data)
        .map_err(|e| Error::InvalidSpec(format!("failed to parse .drv ATerm: {e}")))?;

    Ok(derivation_to_node(drv_path, &drv))
}

/// Convert a parsed Derivation to a proto DerivationNode. This
/// is a stripped-down version of gateway's `derivation_to_node`
/// — we don't have `node_common_fields` (it's in rio-gateway
/// internals) so we extract what matters directly.
///
/// `pub(crate)` for unit tests (no store needed — feed a parsed
/// Derivation directly).
pub(crate) fn derivation_to_node(
    drv_path: &str,
    drv: &rio_nix::derivation::Derivation,
) -> types::DerivationNode {
    // pname from env (nix convention: derivations set `pname` or
    // `name`). The scheduler's estimator keys on (pname, system)
    // for duration prediction. Missing = empty = no history
    // match = 30s default. Fine for phase3a.
    let pname = drv
        .env()
        .get("pname")
        .or_else(|| drv.env().get("name"))
        .cloned()
        .unwrap_or_default();

    // requiredSystemFeatures: space-separated string in env.
    // Empty for most derivations (the worker's `features` set
    // must be a superset; empty is always satisfied).
    let required_features: Vec<String> = drv
        .env()
        .get("requiredSystemFeatures")
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    // Outputs: unzip name + path. The scheduler validates
    // output_names and expected_output_paths are the same length
    // (it doesn't — it stores them separately — but let's keep
    // them parallel anyway).
    let (output_names, expected_output_paths): (Vec<_>, Vec<_>) = drv
        .outputs()
        .iter()
        .map(|o| (o.name().to_string(), o.path().to_string()))
        .unzip();

    types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Input-addressed convention: drv_hash = drv_path. The
        // scheduler uses this as the DAG primary key. Unique by
        // construction (store paths are content-addressed).
        // Matches what the gateway does (translate.rs:318).
        drv_hash: drv_path.to_string(),
        pname,
        system: drv.platform().to_string(),
        required_features,
        output_names,
        is_fixed_output: drv.is_fixed_output(),
        expected_output_paths,
        // Empty: worker fetches from store. We COULD inline (we
        // have the NAR right here), but the scheduler's
        // cache-check might short-circuit to Completed if the
        // outputs are already in store, and then the inline was
        // wasted. Let the worker's fetch_drv_from_store handle
        // it — same path as gateway's non-inlined nodes.
        drv_content: Vec::new(),
        // 0 = no-signal. Estimator falls through to the 30s
        // default. Populating this needs QueryPathInfo on every
        // input_src — another round-trip for a marginal
        // prediction improvement on a single-node DAG.
        input_srcs_nar_size: 0,
    }
}

/// Map CRD's integer priority to the scheduler's priority_class
/// string. Higher integer → more urgent class.
///
/// Ranges chosen so 0 (the default) → Scheduled (lowest). CRD
/// authors bump it up if they want CI or interactive treatment.
fn priority_to_class(p: i32) -> &'static str {
    match p {
        // >= 20: interactive (highest, same as IFD builds).
        // The scheduler's INTERACTIVE_BOOST applies.
        20.. => "interactive",
        // 10..20: CI. Normal priority.
        10..20 => "ci",
        // < 10 (including negative, though CEL on the CRD
        // could reject that): scheduled. Background.
        _ => "scheduled",
    }
}

/// Drain the BuildEvent stream, patching `.status` on each
/// state-transition event. Runs until stream EOF (build
/// terminal, scheduler closed channel) or PATCH fails.
///
/// Event::Log filtered out — a chatty rustc sends ~20/sec, and
/// PATCHing status on each is apiserver abuse. Progress events
/// are emitted on derivation state transitions (1/sec worst
/// case), and that's what the printer column shows anyway.
///
/// `scheduler_addr` + `start_seq`: for reconnect on stream error
/// (scheduler failover). Backoff-retry up to 5 attempts; WatchBuild
/// with the LAST seq we saw (or start_seq on first reconnect).
/// After max retries: log warn, exit — status stale.
///
/// `known_build_id`: if Some, initializes status.build_id directly
/// (X11 fix). For the reconnect path, the caller already knows the
/// build_id (from the CRD's persisted status). Passing it means the
/// internal reconnect loop below can WatchBuild even if the stream
/// errors before the first event. For the initial spawn (SubmitBuild
/// stream), pass None — the first event carries the scheduler-assigned
/// build_id.
#[allow(clippy::too_many_arguments)]
async fn drain_stream(
    name: String,
    api: Api<Build>,
    mut stream: tonic::Streaming<types::BuildEvent>,
    scheduler_addr: String,
    start_seq: u64,
    known_build_id: Option<String>,
    watching: Arc<dashmap::DashMap<String, ()>>,
    watch_key: String,
) {
    // Remove watching entry on exit (any path — terminal, error,
    // panic, reconnect-exhausted). Next apply() can re-spawn if
    // needed (e.g., controller restart, or this task died early).
    let _guard = scopeguard::guard((), move |()| {
        watching.remove(&watch_key);
    });

    // Accumulate status across events. We PATCH the FULL status
    // object each time (server-side apply merges), so this tracks
    // the "last known good" state. last_sequence initialized from
    // start_seq (0 for initial SubmitBuild stream, persisted seq
    // for reconnects). build_id from known_build_id if reconnecting
    // (X11 fix) — without this, reconnect + stream-error-before-
    // first-event → status.build_id empty → exit at line 682 →
    // status never updates.
    //
    // phase: "Pending" for initial spawn. For reconnect with a
    // known build_id, we don't know the actual phase (CRD may have
    // old value); Progress events (X12 fix) will bump to Building.
    let mut status = BuildStatus {
        phase: "Pending".into(),
        build_id: known_build_id.unwrap_or_default(),
        last_sequence: start_seq as i64,
        ..Default::default()
    };

    // Reconnect retry state. Reset on successful event.
    let mut reconnect_attempts = 0u32;
    const MAX_RECONNECT: u32 = 5;

    loop {
        match stream.message().await {
            Ok(Some(ev)) => {
                // Reset reconnect counter: stream is healthy.
                reconnect_attempts = 0;

                // First event always carries the real build_id
                // (scheduler assigns it on MergeDag). `status` is
                // TASK-LOCAL, initialized to empty above — not the
                // CRD's persisted value (which has the "submitted"
                // sentinel from apply()). So is_empty() is the only
                // check needed; the sentinel never appears here.
                if status.build_id.is_empty() {
                    status.build_id = ev.build_id.clone();
                }

                // Track sequence BEFORE apply_event: even if this
                // event doesn't trigger a patch (e.g., Log), the
                // NEXT patch will persist the correct last_sequence.
                // ev.sequence is u64; cast to i64 for K8s schema
                // compat (overflow at ~9e18, not a concern).
                status.last_sequence = ev.sequence as i64;

                let Some(inner) = ev.event else { continue };
                let should_patch = apply_event(&mut status, inner);

                if should_patch && let Err(e) = patch_status(&api, &name, status.clone()).await {
                    // PATCH failed. Most likely: CRD deleted
                    // mid-build (finalizer ran, object gone).
                    // Not much we can do — exit the watch.
                    warn!(
                        build = %name,
                        error = %e,
                        "status PATCH failed; exiting watch (CRD deleted?)"
                    );
                    return;
                }
            }
            Ok(None) => {
                // Stream EOF: build terminal, scheduler closed.
                // Final status was already patched (Completed/
                // Failed/Cancelled all trigger a patch above).
                debug!(build = %name, "BuildEvent stream closed");
                return;
            }
            Err(e) => {
                // Scheduler dropped (restart, failover, network).
                // Reconnect via WatchBuild with last seq. Backoff
                // 1s/2s/4s/8s/16s; after MAX_RECONNECT fails, give
                // up — status stale until next controller restart.
                reconnect_attempts += 1;
                if reconnect_attempts > MAX_RECONNECT {
                    warn!(
                        build = %name,
                        error = %e,
                        attempts = reconnect_attempts,
                        "BuildEvent stream error: max reconnects exhausted; status will stop updating"
                    );
                    // Patch phase=Unknown so operator sees "watch
                    // lost" not just "stuck at last state". Terminal-
                    // ish (not Succeeded/Failed/Cancelled) so the
                    // idempotence gate WOULD reconnect on controller
                    // restart. Best-effort; ignore PATCH error.
                    status.phase = "Unknown".into();
                    let _ = patch_status(&api, &name, status).await;
                    return;
                }

                let backoff = std::time::Duration::from_secs(1 << (reconnect_attempts - 1).min(4));
                warn!(
                    build = %name,
                    error = %e,
                    attempt = reconnect_attempts,
                    backoff_secs = backoff.as_secs(),
                    "BuildEvent stream error; reconnecting"
                );
                tokio::time::sleep(backoff).await;

                // Reconnect: fresh scheduler client + WatchBuild.
                // Need build_id — if we never got the first event
                // (status.build_id empty), we can't reconnect.
                // That's the "SubmitBuild stream dropped before
                // first event" case — rare (scheduler crashed
                // between MergeDag and first BuildStarted). Just
                // exit; apply()'s idempotence gate will retry on
                // next reconcile (build_id is still "submitted"
                // sentinel → resubmit).
                if status.build_id.is_empty() {
                    warn!(build = %name, "no build_id yet; cannot reconnect, exiting watch");
                    return;
                }

                match rio_proto::client::connect_scheduler(&scheduler_addr).await {
                    Ok(mut sched) => {
                        match sched
                            .watch_build(types::WatchBuildRequest {
                                build_id: status.build_id.clone(),
                                since_sequence: status.last_sequence as u64,
                            })
                            .await
                        {
                            Ok(resp) => {
                                info!(build = %name, "reconnected WatchBuild");
                                stream = resp.into_inner();
                                // Loop continues: next iteration
                                // reads from the new stream.
                            }
                            Err(wb_err) => {
                                // WatchBuild itself failed (build
                                // not found — recovery didn't
                                // reconstruct it). Don't retry
                                // THIS error — it's not transient.
                                warn!(build = %name, error = %wb_err,
                                      "WatchBuild failed (build unknown?); exiting watch");
                                return;
                            }
                        }
                    }
                    Err(conn_err) => {
                        // Connect failed. Count as reconnect
                        // attempt; next iteration of the outer
                        // loop will Error again (stream is still
                        // the dead one) → another backoff. After
                        // MAX, give up.
                        warn!(build = %name, error = %conn_err,
                              "scheduler connect failed during reconnect");
                        // Don't continue — the dead stream will
                        // just error again immediately. Sleep was
                        // already done above.
                    }
                }
            }
        }
    }
}

/// Apply one event to the accumulated status. Returns whether
/// this event warrants a PATCH (state transitions yes, logs no).
///
/// `pub(crate)` for unit testing without a live stream.
pub(crate) fn apply_event(status: &mut BuildStatus, ev: BuildEv) -> bool {
    match ev {
        BuildEv::Started(s) => {
            status.phase = "Building".into();
            status.total_derivations = s.total_derivations as i32;
            status.cached_derivations = s.cached_derivations as i32;
            status.progress = format!("0/{}", s.total_derivations);
            status.started_at = Some(Time(k8s_openapi::jiff::Timestamp::now()));
            true
        }
        BuildEv::Progress(p) => {
            status.completed_derivations = p.completed as i32;
            // Progress also carries total — use it (don't rely
            // on status.total_derivations from Started, which we
            // might have missed on a reconnect race).
            status.total_derivations = p.total as i32;
            status.progress = format!("{}/{}", p.completed, p.total);
            // X12: Progress only fires AFTER Started, so the build
            // IS building. On reconnect with since_seq > Started's
            // seq, we never see Started → phase stays Pending (set
            // at drain_stream init). kubectl get build shows Pending
            // for an actively-building job. This bump fixes it.
            if status.phase == "Pending" {
                status.phase = "Building".into();
            }
            true
        }
        BuildEv::Completed(_) => {
            status.phase = "Succeeded".into();
            true
        }
        BuildEv::Failed(f) => {
            status.phase = "Failed".into();
            // Record the error in a condition. Find-or-push so
            // repeated Failed events (shouldn't happen, but
            // defensive) don't duplicate.
            set_condition(
                &mut status.conditions,
                "Failed",
                "True",
                "BuildFailed",
                &format!("{}: {}", f.failed_derivation, f.error_message),
            );
            true
        }
        BuildEv::Cancelled(c) => {
            status.phase = "Cancelled".into();
            set_condition(
                &mut status.conditions,
                "Cancelled",
                "True",
                "BuildCancelled",
                &c.reason,
            );
            true
        }
        // DerivationEvent: per-drv state change. Interesting for
        // dashboards but we only track aggregates. Skip patch —
        // the next Progress event carries the updated count.
        BuildEv::Derivation(_) => false,
        // Log: filtered, same as everywhere else. Too chatty.
        BuildEv::Log(_) => false,
    }
}

/// Find-or-push a Condition. Sets `lastTransitionTime` only when
/// the status actually changes (K8s convention — watchers key on
/// transition time to detect "NEW failure vs still failing").
fn set_condition(
    conditions: &mut Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition>,
    type_: &str,
    new_status: &str,
    reason: &str,
    message: &str,
) {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
    let now = Time(k8s_openapi::jiff::Timestamp::now());

    if let Some(c) = conditions.iter_mut().find(|c| c.type_ == type_) {
        if c.status != new_status {
            c.last_transition_time = now;
        }
        c.status = new_status.into();
        c.reason = reason.into();
        c.message = message.into();
    } else {
        conditions.push(Condition {
            type_: type_.into(),
            status: new_status.into(),
            reason: reason.into(),
            message: message.into(),
            last_transition_time: now,
            observed_generation: None,
        });
    }
}

/// PATCH the status subresource via server-side apply. Separate
/// fn so both apply() (sentinel) and drain_stream() (real
/// updates) share the `json!({"status": ...})` ceremony.
async fn patch_status(api: &Api<Build>, name: &str, status: BuildStatus) -> Result<()> {
    // `apiVersion` + `kind` are required by server-side apply
    // even for status patches — the apiserver uses them to
    // resolve the schema. Without them: 400 "apiVersion must be
    // set in apply patch". kube-rs's api_resource() has both.
    let ar = Build::api_resource();
    let patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": status,
    });
    api.patch_status(
        name,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(&patch),
    )
    .await?;
    Ok(())
}

// r[verify ctrl.crd.build]
#[cfg(test)]
mod tests {
    use super::*;

    /// Priority mapping: 0 (default) → scheduled, bumps up.
    #[test]
    fn priority_ranges() {
        assert_eq!(priority_to_class(0), "scheduled");
        assert_eq!(priority_to_class(-5), "scheduled"); // tolerant
        assert_eq!(priority_to_class(9), "scheduled");
        assert_eq!(priority_to_class(10), "ci");
        assert_eq!(priority_to_class(19), "ci");
        assert_eq!(priority_to_class(20), "interactive");
        assert_eq!(priority_to_class(100), "interactive");
    }

    /// apply_event: state events patch, Log/Derivation don't.
    /// Mirrors emit_build_event's Log filter — same "too chatty"
    /// rationale, different target (apiserver not PG).
    #[test]
    fn apply_event_filters_noise() {
        let mut status = BuildStatus::default();

        assert!(apply_event(
            &mut status,
            BuildEv::Started(types::BuildStarted {
                total_derivations: 10,
                cached_derivations: 3,
            })
        ));
        assert_eq!(status.phase, "Building");
        assert_eq!(status.total_derivations, 10);
        assert_eq!(status.progress, "0/10");

        // Log → no patch, status untouched.
        assert!(!apply_event(
            &mut status,
            BuildEv::Log(types::BuildLogBatch::default())
        ));
        assert_eq!(status.phase, "Building", "Log didn't clobber phase");

        // DerivationEvent → no patch either.
        assert!(!apply_event(
            &mut status,
            BuildEv::Derivation(types::DerivationEvent::default())
        ));

        // Progress → patch, counts update.
        assert!(apply_event(
            &mut status,
            BuildEv::Progress(types::BuildProgress {
                completed: 5,
                running: 2,
                queued: 3,
                total: 10,
            })
        ));
        assert_eq!(status.progress, "5/10");

        // Completed → terminal.
        assert!(apply_event(
            &mut status,
            BuildEv::Completed(types::BuildCompleted::default())
        ));
        assert_eq!(status.phase, "Succeeded");
    }

    /// derivation_to_node extracts pname/system/outputs from a
    /// parsed .drv. No store needed — feed ATerm directly.
    #[test]
    fn node_from_derivation() {
        // Minimal valid ATerm. One output, system set, pname in
        // env. Same format as gateway/translate.rs test fixtures.
        let aterm = r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello","","")],[],[],"x86_64-linux","/bin/sh",[],[("pname","hello"),("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello")])"#;
        let drv = rio_nix::derivation::Derivation::parse(aterm).expect("valid ATerm");

        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello.drv";
        let node = derivation_to_node(drv_path, &drv);

        assert_eq!(node.drv_path, drv_path);
        assert_eq!(node.drv_hash, drv_path, "input-addressed: hash = path");
        assert_eq!(node.pname, "hello");
        assert_eq!(node.system, "x86_64-linux");
        assert_eq!(node.output_names, vec!["out"]);
        assert_eq!(
            node.expected_output_paths,
            vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello"]
        );
        assert!(!node.is_fixed_output);
        assert!(node.drv_content.is_empty(), "worker fetches from store");
    }

    /// set_condition: find-or-push, transition time only on change.
    #[test]
    fn condition_find_or_push() {
        let mut conds = vec![];

        set_condition(&mut conds, "Ready", "True", "R", "ok");
        assert_eq!(conds.len(), 1);
        let first_time = conds[0].last_transition_time.clone();

        // Same status → no transition (time stays).
        std::thread::sleep(Duration::from_millis(2));
        set_condition(&mut conds, "Ready", "True", "R", "still ok");
        assert_eq!(conds.len(), 1, "find-or-push: no duplicate");
        assert_eq!(
            conds[0].last_transition_time, first_time,
            "same status → transition time unchanged"
        );
        assert_eq!(conds[0].message, "still ok", "message does update");

        // Status change → transition (time moves).
        std::thread::sleep(Duration::from_millis(2));
        set_condition(&mut conds, "Ready", "False", "F", "broken");
        assert_ne!(
            conds[0].last_transition_time, first_time,
            "status change → transition time updated"
        );

        // Different type → new entry.
        set_condition(&mut conds, "Failed", "True", "X", "boom");
        assert_eq!(conds.len(), 2);
    }
}
