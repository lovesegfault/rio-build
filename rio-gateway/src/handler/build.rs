//! Build event → STDERR translation and build opcode handlers.

use std::collections::{BTreeSet, HashMap, HashSet};

use rio_common::tenant::NormalizedName;
use rio_nix::derivation::Derivation;
use rio_nix::protocol::build::{
    BuildMode, BuildResult, BuildStatus, read_basic_derivation, write_build_result,
};
use rio_nix::protocol::derived_path::{DerivedPath, OutputSpec};
use rio_nix::protocol::stderr::{
    ActivityType, ResultField, ResultType, StderrError, StderrWriter, verbosity,
};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::{SchedulerServiceClient, StoreServiceClient, types};
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::Channel;
use tracing::{debug, instrument, warn};

use super::grpc::{grpc_is_valid_path, resolve_floating_outputs};
use super::log_tail::{LogTailSet, TaggedLogChunk};
use super::{GatewayError, PROGRAM_NAME, SessionContext, with_jwt};
use crate::drv_cache::resolve_derivation;
use crate::quota::{QuotaCache, QuotaVerdict, human_bytes};
use crate::translate;

/// Error from `process_build_events`. Distinguishes transport
/// errors and stream EOF-without-terminal (both reconnect-worthy
/// failover signatures — see the per-variant docs) from `Wire`
/// (client-side disconnect — not retried).
#[derive(Debug, thiserror::Error)]
enum StreamProcessError {
    /// gRPC-level error (connection reset, timeout). Scheduler
    /// may have failed over — reconnecting via WatchBuild has
    /// a good chance of resuming.
    #[error("build event stream error: {0}")]
    Transport(tonic::Status),
    /// Stream returned `Ok(None)` (EOF) without a Completed/
    /// Failed/Cancelled event. This IS the primary failover
    /// signature: k8s pod kill → SIGTERM → graceful shutdown →
    /// TCP FIN → clean stream close. NOT a Transport error.
    /// Reconnect-worthy for the same reason as Transport.
    /// (live_064 retired the "(scheduler disconnected?)" parenthetical
    /// this Display used to carry: the owner's incident surfaced it as
    /// the build's failure cause while the scheduler was perfectly
    /// healthy and the real cause — eleven UNAUTHENTICATED re-attach
    /// rejections — never reached the message. Stream-death
    /// classification stays descriptive; causes belong to the
    /// re-attach evidence below.)
    #[error("build event stream ended without a terminal event")]
    EofWithoutTerminal,
    /// Error writing STDERR to the client (WireError). The Nix
    /// client disconnected or the SSH channel closed. NOT
    /// reconnect-worthy — scheduler is fine, client is gone.
    #[error("client disconnected: {0}")]
    Wire(#[from] rio_nix::protocol::wire::WireError),
    /// The scheduler signalled irrecoverable event loss for this
    /// watcher (broadcast ring overrun → `BuildEvent.resync_required`).
    /// Reconnect-worthy with ZERO backoff: the server is healthy, this
    /// WATCHER fell behind — the fresh WatchBuild's snapshot is the
    /// recovery (gw.resync.loss-signal).
    #[error("scheduler signalled event loss; resync via snapshot")]
    ResyncRequired,
    /// live_064: a WatchBuild re-attach was rejected `UNAUTHENTICATED`
    /// even after the one-shot session-token re-mint — the session's
    /// key material no longer verifies (revoked tenant, signer
    /// mismatch). NOT retried further: every later cycle would carry
    /// the same verdict, and burning the budget on it is exactly how
    /// the live_064 incident spent its last six minutes. The client
    /// message states the auth-rejection FACT only; the upstream
    /// Status body is operator-log material (the warn at the branch
    /// site carries it with the build_id) — scheduler-internal error
    /// detail never rides the client wire.
    #[error("WatchBuild re-attach rejected as unauthenticated even after a session-token re-mint")]
    ReattachAuthRejected,
    /// live_064: the re-attach budget exhausted. The surfaced cause is
    /// the LAST RE-ATTACH failure (what actually kept the watch from
    /// resuming), never the benign stream EOF that opened the episode
    /// — the incident's exhaustion message blamed a healthy scheduler
    /// while the auth rejections never reached it. `last_reattach` is
    /// a SANITIZED classification (fixed sentences + gRPC codes, built
    /// at the recording sites); the full upstream Status bodies live
    /// in the per-attempt operator warns, never on the client wire.
    #[error("WatchBuild re-attach failed {attempts} times; last attempt: {last_reattach}")]
    ReattachExhausted {
        attempts: u32,
        last_reattach: String,
    },
}

// r[impl gw.reject.build-mode]
/// Read `build_mode` from the wire and reject anything other than `Normal`.
///
/// rio does not implement Repair (force-rebuild a corrupted path) or Check
/// (rebuild-and-diff for reproducibility) — `SubmitBuildRequest` has no mode
/// field. Erroring is correct: a silent Normal build gives the user a
/// false-positive determinism/repair result (`nix build --rebuild` would
/// always "pass").
///
/// Called at the exact wire position of the `build_mode` field for each of
/// the three build opcodes — DO NOT reorder relative to other reads.
async fn read_build_mode_normal_only<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    opcode: &str,
) -> anyhow::Result<()> {
    let raw = wire::read_u64(reader).await?;
    match BuildMode::try_from(raw) {
        Ok(BuildMode::Normal) => Ok(()),
        Ok(mode) => {
            stderr_err!(
                stderr,
                "{opcode}: rio-gateway does not support build mode {mode:?} (--repair/--rebuild); use a local store"
            );
        }
        Err(_) => {
            stderr_err!(stderr, "{opcode}: unsupported build mode {raw}");
        }
    }
}

/// Check the per-tenant rate limit before `SubmitBuild`. On violation:
/// sends `STDERR_ERROR` with a wait-hint and returns `Ok(true)` so the
/// caller short-circuits. On pass: returns `Ok(false)`.
///
/// The bool-return shape (instead of `Result`) avoids an error type
/// for what is a normal rate-limited outcome — the SSH connection
/// stays open, the client retries after the hinted delay. The session
/// loop in `session.rs` continues to the next opcode.
///
/// `STDERR_ERROR` is the terminal frame for THIS OPCODE, not the
/// session. The Nix client's `processStderr()` returns the error up
/// to the calling code (e.g., `nix build` prints it and exits 1), but
/// the SSH channel stays open for a retry.
#[instrument(skip_all, fields(tenant = tenant_name.map(|n| n.as_str())))]
async fn rate_limit_check<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    limiter: &crate::ratelimit::TenantLimiter,
    tenant_name: Option<&NormalizedName>,
) -> anyhow::Result<bool> {
    // `NormalizedName` derefs to str; `Option::map(Deref)` gives
    // `Option<&str>` which is what the limiter takes. `None` (single-
    // tenant mode) and `Some("")` both bucket under `__anon__` there,
    // but the `Some("")` case is unreachable now: `NormalizedName`
    // can't be empty by construction.
    match limiter.check(tenant_name.map(|n| n.as_str())) {
        Ok(()) => Ok(false),
        Err(wait) => {
            // Display string for the error message. `None` →
            // single-tenant mode → "anon". `Some(n)` is guaranteed
            // non-empty/trimmed so the name goes straight in.
            let tenant_disp = tenant_name.map(|n| n.as_str()).unwrap_or("anon");
            // Round up: 0.3s → "~1s". The wait is an advisory hint, not
            // an exact contract — a client that retries exactly at the
            // hinted second may still be a hair early.
            let secs = wait.as_secs().max(1);
            warn!(
                tenant = %tenant_disp,
                wait_secs = secs,
                "rate limit: rejecting build submit"
            );
            metrics::counter!("rio_gateway_errors_total", "type" => "rate_limited").increment(1);
            stderr
                .error(&StderrError::simple(
                    "rio-gateway",
                    format!(
                        "rate limit: too many builds from tenant '{tenant_disp}' — retry in ~{secs}s"
                    ),
                ))
                .await?;
            Ok(true)
        }
    }
}

// r[impl store.gc.tenant-quota-enforce]
/// Check the per-tenant store quota before `SubmitBuild`. Sibling to
/// [`rate_limit_check`]: same `STDERR_ERROR`+early-return shape, same
/// connection-stays-open semantics so the user can GC and retry
/// without reconnecting.
///
/// Quota is eventually-enforcing. `tenant_store_bytes` is cached
/// 30s in `quota_cache`; a few MB of race-window overflow is
/// acceptable per the spec. MVP: rejects on CURRENT overflow only
/// (`used > limit`), not predictive (`estimated_new_bytes = 0`).
///
/// `store_client` is the RPC fallback on cache miss/stale. Fail-open:
/// a transient store error logs + returns `Ok(false)` — quota is a
/// resource gate, not a security gate; a stuck store shouldn't
/// deadlock the build pipeline.
#[instrument(skip_all, fields(tenant = tenant_name.map(|n| n.as_str())))]
async fn quota_check<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    quota_cache: &QuotaCache,
    store_client: &mut StoreServiceClient<Channel>,
    tenant_name: Option<&NormalizedName>,
    jwt_token: Option<&str>,
) -> anyhow::Result<bool> {
    match quota_cache
        .check(store_client, tenant_name, jwt_token)
        .await
    {
        QuotaVerdict::Under { .. } | QuotaVerdict::Unlimited => Ok(false),
        QuotaVerdict::Over { used, limit } => {
            // `Over` is unreachable for `None` (quota_cache.check
            // early-returns Unlimited on None), but the display
            // branch stays defensive: "anon" if it ever fires.
            let tenant_disp = tenant_name.map(|n| n.as_str()).unwrap_or("anon");
            warn!(
                tenant = %tenant_disp,
                used_bytes = used,
                limit_bytes = limit,
                "quota: rejecting build submit (over gc_max_store_bytes)"
            );
            metrics::counter!(
                "rio_gateway_quota_rejections_total",
                "tenant" => tenant_disp.to_string()
            )
            .increment(1);
            stderr
                .error(&StderrError::simple(
                    "rio-gateway",
                    format!(
                        "tenant '{tenant_disp}' over store quota: {} / {} — \
                         run `nix store gc` or request a quota increase",
                        human_bytes(used),
                        human_bytes(limit)
                    ),
                ))
                .await?;
            Ok(true)
        }
    }
}

/// STDERR-activity state that survives `process_build_events`
/// reconnects. Hoisted to `submit_and_process_build` so a WatchBuild
/// resume after scheduler failover keeps the activity-ID map intact —
/// otherwise the gateway can't `stop_activity` derivations whose
/// `Started` arrived on the previous stream, and nom shows them as
/// stuck forever.
/// Paired activity IDs for one derivation's upstream substitute. The
/// parent `actSubstitute` starts on `Substituting` and is what nom
/// shows in its "substituting" line; the child `actCopyPath` is
/// DEFERRED (live_043): it starts inside the progress relay on the
/// FIRST tick carrying a NON-EMPTY `upstream_uri`, with `from` pinned
/// to that URI — matching stock `store-api.cc copyStorePath`, where
/// the activity is constructed when the copy from the chosen
/// substituter begins. Job-level commit ticks pass `uri=""` (including
/// locally-present paths), so a zero-fetch materialization never
/// starts a copy — truthful display: nothing was downloaded.
/// `resProgress` rides the copy child once started and ONLY there —
/// the parent is structural, per stock convention (live_045) — and
/// the pair stops together (child first, iff started) through the
/// ONE close chokepoint (`stop_subst_pair`), which carries its
/// [`SubstCloseCause`]: only a cause that PROVES transfer completion
/// (`Cached`, the substitution-success terminal) licenses the
/// completing synthesis on the copy aid; a disproving or unknown
/// close freezes the last truthful relayed bar (merged_bug_003).
#[derive(Debug, Clone)]
struct SubstAids {
    subst: u64,
    /// `None` ⇔ no sourced tick seen yet (or ever).
    copy: Option<u64>,
    /// Last-relayed `(done, expected)` — the close-synthesis input.
    progress: Option<(u64, u64)>,
    /// The path shown in activity text/fields (first output path or
    /// the drv path) — the deferred copy START frame needs it after
    /// `start_subst_display` returned.
    out: String,
}

// r[impl gw.display.single-map]
/// One derivation's display family. A derivation holds exactly ONE
/// display treatment at a time — either a per-drv `actBuild` activity
/// (builder executions) or an `actSubstitute`+`actCopyPath` pair
/// (upstream substitutions / store-side materializations). The old
/// two-map scheme made omission representable: a reconcile loop that
/// iterated only one family's map silently skipped the other (a
/// substitution that completed while the gateway was detached survived
/// the snapshot gone-reconcile and rendered stuck forever). One map
/// with a family-valued entry makes every sweep total over THE key
/// set.
#[derive(Debug, Clone)]
enum DrvDisplay {
    /// Per-drv `actBuild` activity ID (start/stop, and the attach point
    /// for `BuildLogLine`/`SetPhase` results).
    Build(u64),
    /// `actSubstitute` + child `actCopyPath` pair. Started by
    /// `DerivationEventKind::Substituting` (or a kinded materialization
    /// snapshot entry), stopped by `Cached` (success) or `Started`
    /// (fetch failed → fell through to build); every close carries its
    /// [`SubstCloseCause`], and only a completion-proving cause
    /// licenses the close synthesis.
    /// r[gw.activity.subst-progress+4]
    Subst(SubstAids),
}

struct BuildActivityState {
    /// Per-derivation display family — the ONLY per-drv display
    /// tracking. See [`DrvDisplay`].
    display: HashMap<String, DrvDisplay>,
    /// Top-level `actBuilds` activity ID. `None` until `BuildStarted`
    /// arrives. `Progress`/`SetExpected` results attach here.
    builds_root: Option<u64>,
    /// Root `SetExpected{actCopyPath}` denominator for nom's "X/Y
    /// copied". Conservation law (bug_123): at every pair-close
    /// boundary the last emitted denominator equals the count of
    /// `actCopyPath` activities actually STARTED; at build terminus
    /// expected == started. Three lifecycle edges — stock
    /// `expectedSubstitutions` MaintainCount parity (mint AND retire):
    /// - MINT at `Substituting` ([`start_subst_display`], the sole
    ///   inserter of `DrvDisplay::Subst` — 1:1 with entry creation);
    /// - REALIZE at the deferred copy start (no arithmetic — the mint
    ///   already counted this pair);
    /// - RETIRE at a copy-less close ([`stop_subst_pair`], total over
    ///   closes via the single display map + stop-parity): the
    ///   expectation never realized, so the close re-emits the
    ///   decremented count (`SetExpected` is absolute on the wire — a
    ///   lowered re-emission is protocol-legal, exactly how stock
    ///   retires `expectedSubstitutions`).
    ///
    /// The scheduler doesn't know upfront how many drvs will go
    /// `Substituting` (it's discovered as the DAG runs), so the value
    /// moves both ways rather than being set once.
    subst_expected: u64,
    /// Stable cluster identifier emitted as `actBuild` field 1
    /// (`machineName`). NOT per-pod — see the comment at the
    /// `Status::Started` arm. Read once from `RIO_GATEWAY_MACHINE_NAME`.
    machine_name: String,
}

impl Default for BuildActivityState {
    fn default() -> Self {
        Self {
            display: HashMap::default(),
            builds_root: None,
            subst_expected: 0,
            machine_name: rio_common::config::env_or("RIO_GATEWAY_MACHINE_NAME", String::new()),
        }
    }
}

// r[impl gw.activity.subst-progress+4]
/// Why a [`SubstAids`] pair is closing — the close-cause axis of THE
/// close chokepoint ([`stop_subst_pair`]). One variant per production
/// call site, 1:1, so rustc enumerates the caller set (a new close
/// arm cannot compile cause-free) and [`completion_proven`]'s
/// exhaustive match enumerates the proof status of every cause (a new
/// variant cannot compile policy-free) — the two generated memberships
/// merged_bug_003 lacked.
///
/// | variant | trigger arm | proves completion? | close emission |
/// |---|---|---|---|
/// | `Cached` | `DerivationEventKind::Cached` — the scheduler's substitution-success terminal | YES | synthesize completing `resProgress` on the copy aid iff the bar is partial |
/// | `Completed` | `DerivationEventKind::Completed` — defensive terminal-arm symmetry; NOT a normal FSM transition for a substituting drv | NO (anomaly or event loss) | freeze the last truthful bar |
/// | `FellThroughToBuild` | `DerivationEventKind::Started` — the fetch FAILED and the drv fell through to a build | NO (disproof) | freeze |
/// | `Failed` | `DerivationEventKind::Failed` — failure cascade | NO (disproof) | freeze |
/// | `SnapshotKindFlip` | snapshot reconcile: tracked subst running as a BUILD (fell through while detached) | NO (disproof) | freeze |
/// | `SnapshotGone` | snapshot reconcile: tracked drv absent from running (outcome unobserved while detached) | NO (unknown) | freeze |
/// | `TerminalDrain` | build-terminus drain of unstopped activities (upstream event loss) | NO (unknown) | freeze |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubstCloseCause {
    Cached,
    Completed,
    FellThroughToBuild,
    Failed,
    SnapshotKindFlip,
    SnapshotGone,
    TerminalDrain,
}

impl SubstCloseCause {
    /// Every variant exactly once — the policy-matrix census input
    /// (`close_synthesis_policy_is_total_over_causes` iterates it; its
    /// completeness is pinned by `subst_close_cause_all_is_complete`'s
    /// exhaustive index match). Test-only consumer BY DESIGN:
    /// production code matches the closed enum exhaustively instead of
    /// iterating it.
    #[cfg_attr(not(test), expect(dead_code))]
    const ALL: [Self; 7] = [
        Self::Cached,
        Self::Completed,
        Self::FellThroughToBuild,
        Self::Failed,
        Self::SnapshotKindFlip,
        Self::SnapshotGone,
        Self::TerminalDrain,
    ];
}

/// Does `cause` PROVE the pair's transfer completed? THE synthesis
/// license for [`stop_subst_pair`]: the completing frame claims "the
/// transfer finished", and that claim is entailed only when the close
/// trigger proves it. ONE exhaustive match so the trigger→proof table
/// has one reviewable owner — collapsing the proof class at the call
/// sites would re-introduce per-caller judgment (the merged_bug_003
/// leak shape: a success-arm tolerance hoisted cause-free generalizes
/// to disproving callers).
fn completion_proven(cause: SubstCloseCause) -> bool {
    match cause {
        // The scheduler's substitution-success terminal: the fetch
        // committed; a dropped final tick is covered by this proof.
        SubstCloseCause::Cached => true,
        // NOT proof: `Substituting → Completed` is not a normal FSM
        // transition (see the relay arm's comment) — the arm is
        // reachable only via FSM anomaly or event loss, and under the
        // lost-`Started` path (fetch failed, drv BUILT, `Completed`
        // arriving with the Subst display still open) the completion
        // claim is exactly false. Conservative cost in the
        // never-observed legal-symmetry case: a frozen partial bar —
        // cosmetic. A future FSM change that legalizes the transition
        // re-litigates this row at the policy matrix.
        SubstCloseCause::Completed => false,
        // Disproof: `Started` after Substituting fires on fetch
        // FAILURE (the drv reverted to Ready and dispatched as a
        // build).
        SubstCloseCause::FellThroughToBuild => false,
        // Disproof: failure cascade.
        SubstCloseCause::Failed => false,
        // Disproof: the drv runs as a BUILD in the snapshot — the
        // substitute fell through while the gateway was detached.
        SubstCloseCause::SnapshotKindFlip => false,
        // Unknown: the drv left the running set while detached — the
        // outcome was never observed.
        SubstCloseCause::SnapshotGone => false,
        // Unknown: upstream event loss at build terminus.
        SubstCloseCause::TerminalDrain => false,
    }
}

/// THE close chokepoint for a [`SubstAids`] pair (live_043): child
/// (`actCopyPath`, iff started) before parent (`actSubstitute`).
/// Factored out of every arm that closes a substitute on terminal —
/// including the terminal drain — so the display-close tolerance has
/// ONE owner, and CAUSE-BEARING (merged_bug_003): the synthesized
/// `[expected, expected, 0, 0]` claims "the transfer completed", and
/// that claim is entailed only when the close trigger PROVES it. When
/// the copy child is STARTED, the last-relayed progress is partial
/// (`done < expected`), AND [`completion_proven`] holds for `cause`
/// (the `Cached` terminal covering a dropped final tick), the
/// completing `resProgress` is synthesized on the COPY aid — the only
/// progress lane (live_045: the parent is structural) — before the
/// stops. A close whose cause does NOT prove completion emits no
/// synthesis: the bar freezes at the last truthful relayed frame
/// (truth is asymmetric — claiming completion falsely is the lie; a
/// frozen partial bar on an unobserved success is cosmetic). A pair
/// with no started copy closes subst-only with NO synthesis frame —
/// truthful (no sourced byte was ever reported), never the broken
/// empty bar — and RETIRES its `subst_expected` mint (bug_123): the
/// expectation never realized into a copy, so after both stops the
/// close re-emits the decremented `SetExpected{actCopyPath}` on the
/// root (iff the root exists — mirroring the mint's emission guard).
/// The retire is CAUSE-INDEPENDENT — the cause axis licenses
/// synthesis; the copy discriminant drives retirement — and total
/// over closes: `start_subst_display` is the sole mint (1:1 with
/// Subst-entry creation) and every Subst-entry removal routes here
/// (single map + stop-parity), so at every close boundary the last
/// emitted denominator equals the count of copy children actually
/// started.
async fn stop_subst_pair<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    aids: SubstAids,
    cause: SubstCloseCause,
) -> Result<(), StreamProcessError> {
    let copyless = aids.copy.is_none();
    if let Some(copy) = aids.copy {
        if completion_proven(cause)
            && let Some((done, expected)) = aids.progress
            && done < expected
        {
            let fields = [
                ResultField::Int(expected),
                ResultField::Int(expected),
                ResultField::Int(0),
                ResultField::Int(0),
            ];
            stderr.result(copy, ResultType::Progress, &fields).await?;
        }
        stderr.stop_activity(copy).await?;
    }
    stderr.stop_activity(aids.subst).await?;
    if copyless {
        // Structurally non-underflowing: the mint is 1:1 with
        // Subst-entry creation and each entry closes exactly once.
        debug_assert!(
            act.subst_expected > 0,
            "copy-less close without a live mint (subst_expected == 0)"
        );
        act.subst_expected = act.subst_expected.saturating_sub(1);
        if let Some(root) = act.builds_root {
            stderr
                .result(
                    root,
                    ResultType::SetExpected,
                    &[
                        ResultField::Int(ActivityType::CopyPath as u64),
                        ResultField::Int(act.subst_expected),
                    ],
                )
                .await?;
        }
    }
    Ok(())
}

/// Start the substitute display family for one derivation: the
/// `actSubstitute` activity, the display-map entry, and the root's
/// `SetExpected{actCopyPath}` bump. Shared by the live `Substituting`
/// event arm and the snapshot reconcile's materialization-running arm
/// — both render the same family. The `actCopyPath` child is NOT
/// started here (live_043 deferred-start): it starts inside
/// `relay_substitute_progress` on the first sourced tick, so its
/// `from` field can carry the upstream URI the way stock
/// `copyStorePath` does. The `SetExpected` bump STAYS here at
/// Substituting time — denominator semantics match stock
/// expected-substitutions-at-goal-creation — and its INVERSE edge
/// lives in [`stop_subst_pair`]: a close with no started copy retires
/// this mint (bug_123), so a drv that re-enters `Substituting` after
/// a fell-through close CONVERGES instead of double-counting (the
/// first attempt's mint was retired at its close). The deferred start
/// additionally removes the phantom done-copy on a
/// fetch-failed→build fallback.
///
/// `out` is the path shown in the activity text: the first output path
/// when the event carries one, else the drv path (snapshot entries
/// carry no output paths).
async fn start_subst_display<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    drv_path: &str,
    out: &str,
) -> Result<(), StreamProcessError> {
    // actSubstitute fields per upstream
    // `substitution-goal.cc`: [storePath, substituterUri].
    // The store picks the upstream — the scheduler doesn't
    // know which yet (S2's first SubstituteProgress carries
    // it), so the URI starts empty; backfilled via SetPhase
    // on the first sourced tick (sh-034: nxb host-dedup
    // needs parent-aid linkage, out-of-tree).
    let subst = stderr
        .start_activity(
            ActivityType::Substitute,
            &format!("substituting '{out}'"),
            verbosity::INFO,
            act.builds_root.unwrap_or(0),
            &[
                ResultField::String(out.to_string()),
                ResultField::String(String::new()),
            ],
        )
        .await?;
    act.display.insert(
        drv_path.to_string(),
        DrvDisplay::Subst(SubstAids {
            subst,
            copy: None,
            progress: None,
            out: out.to_string(),
        }),
    );
    // Bump the root's CopyPath expected so nom's "X/Y copied"
    // denominator tracks. Idempotent across reconnects: callers guard
    // on an existing Subst entry, so this counts first-time
    // substituting per drv per gateway-session.
    act.subst_expected += 1;
    if let Some(root) = act.builds_root {
        stderr
            .result(
                root,
                ResultType::SetExpected,
                &[
                    ResultField::Int(ActivityType::CopyPath as u64),
                    ResultField::Int(act.subst_expected),
                ],
            )
            .await?;
    }
    Ok(())
}

// r[impl gw.stderr.result.build-log-line]
/// Relay one `LogBatch` to the client. Lines attach to the per-drv
/// activity as `STDERR_RESULT{aid, BuildLogLine, [line]}` so nom and
/// `--log-format bar` show the last line under the owning build;
/// fallback to `STDERR_NEXT` when no activity exists for this drv
/// (logs arriving before `Derivation::Started`, or gateway-originated
/// diagnostics like the `trace_id` line). Lines arriving in the
/// 0–2 s post-terminal drain window also take the fallback: the
/// activity is stopped in the same `Completed`/`Failed` iteration that
/// arms the tail's drain grace, so a failed build's final lines print
/// inline rather than under a (now-closed) activity — the designed
/// cost of draining the log tail rather than cancelling it.
async fn relay_log_batch<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &BuildActivityState,
    log_batch: types::BuildLogBatch,
) -> Result<(), StreamProcessError> {
    let aid = match act.display.get(&log_batch.derivation_path) {
        Some(DrvDisplay::Build(aid)) => Some(*aid),
        _ => None,
    };
    for line in &log_batch.lines {
        // Log display, not parse-path. Build log lines are
        // arbitrary builder output (may contain invalid UTF-8
        // from whatever the build script printed); lossy
        // replacement is the correct behavior for display.
        #[allow(clippy::disallowed_methods)]
        let text = String::from_utf8_lossy(line);
        match aid {
            Some(aid) => {
                stderr
                    .result(
                        aid,
                        ResultType::BuildLogLine,
                        &[ResultField::String(text.into_owned())],
                    )
                    .await?;
            }
            None => stderr.log(&text).await?,
        }
    }
    Ok(())
}

/// Render the `rio: retry` marker's optional ` — re-dispatching at
/// <size>` suffix (sh-042). Exhaustive over [`PredecessorFloorAxis`]:
/// `None` (no floor bumped — every reason except the two grace=0
/// promote arms) renders no suffix; `Mem`/`Disk` render the new
/// mint's solved bytes via [`rio_common::fmt::fmt_size_iec`] (so the
/// marker → next `rio: builder` header transition reads coherently).
/// The VM regex pinning this is `^rio: retry\s+.*\d+ (GiB|MiB)` — the
/// property under test is "a sizing suffix appears", not its rung.
///
/// [`PredecessorFloorAxis`]: types::PredecessorFloorAxis
fn sizing_suffix(axis: types::PredecessorFloorAxis, bytes: u64) -> String {
    use types::PredecessorFloorAxis as A;
    match axis {
        A::None => String::new(),
        A::Mem | A::Disk => {
            format!(
                " — re-dispatching at {}",
                rio_common::fmt::fmt_size_iec(Some(bytes))
            )
        }
    }
}

/// The display family an event (or snapshot running entry) demands for
/// its derivation — the input alphabet of [`flip_to_family`]. Closed:
/// the gateway renders exactly two families ([`DrvDisplay`]), so the
/// incoming side is the same two-letter alphabet, with the build letter
/// carrying the [`SubstCloseCause`] its flip stamps on the closed pair.
enum IncomingFamily {
    /// The derivation is (re)entering builder execution: an open
    /// substitute pair closes with `subst_close`.
    Build { subst_close: SubstCloseCause },
    /// The derivation is entering store-side materialization: an open
    /// build activity stops and its dead execution's log tail is cut.
    Subst,
}

// r[impl gw.display.single-map]
// r[impl gw.display.family-flip]
/// THE family-flip chokepoint: every display-family transition — live
/// relay or snapshot reconcile — projects from this one total function
/// over (current display, incoming family). Returns `true` iff a flip
/// closed the previous family (the caller then starts the incoming
/// family's display); same-family and absent cells are no-ops so every
/// caller may invoke it unconditionally. Closing a build display cuts
/// the dead execution's log tail here too — display map and tail set
/// have ONE lifecycle owner.
async fn flip_to_family<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    tails: &mut LogTailSet,
    drv: &str,
    incoming: IncomingFamily,
) -> Result<bool, StreamProcessError> {
    // Take the entry out; non-flip cells restore it unchanged. Total
    // over (family × incoming) — no wildcard arms, so a third display
    // family cannot compile without minting its flip cells here.
    match (act.display.remove(drv), incoming) {
        (Some(DrvDisplay::Build(aid)), IncomingFamily::Subst) => {
            // The attempt kind flipped to materialization: the old
            // execution is dead — stop its activity and cut its tail.
            tails.on_terminal(drv);
            stderr.stop_activity(aid).await?;
            debug!(aid, %drv, "family flip: build display closed for substitute");
            Ok(true)
        }
        (Some(DrvDisplay::Subst(aids)), IncomingFamily::Build { subst_close }) => {
            // Substitute fell through to a build (or the kind flipped
            // while detached): close the dangling pair under its cause.
            stop_subst_pair(stderr, act, aids, subst_close).await?;
            Ok(true)
        }
        (Some(entry @ DrvDisplay::Build(_)), IncomingFamily::Build { .. })
        | (Some(entry @ DrvDisplay::Subst(_)), IncomingFamily::Subst) => {
            act.display.insert(drv.to_string(), entry);
            Ok(false)
        }
        (None, IncomingFamily::Build { .. }) | (None, IncomingFamily::Subst) => Ok(false),
    }
}

/// The scope axis of the client-facing CANCELLATION vocabulary
/// (live_051(f1) — KD-scope, the slot invariant's sixth instance): the
/// terminal vocabulary PARTITIONS attempt/cycle-scoped (retry-implying)
/// from build-scoped (final) terminations, each with exactly ONE mint.
/// A drv-level `Failed{status: Cancelled}` event is CYCLE-scoped — the
/// scheduler's drv-terminal Cancelled is "retriable on EXPLICIT
/// resubmit only" and the synthesized attempt closes (reap self-heal,
/// preemption, failover) stamp cancellation-family vocabulary while
/// the still-wanted drv requeues and keeps building — so it renders in
/// the attempt vocabulary below and structurally CANNOT mint the
/// build-terminal phrase ("build cancelled", whose sole mint is the
/// outcome fold in `submit_and_process_build`). Derived total from the
/// closed `BuildResultStatus` alphabet — adding a status forces a
/// scope decision here (zero wildcard arms).
enum TerminalScope {
    /// Attempt/cycle-scoped cancellation: retry-implying — the build
    /// is NOT over unless its own terminal arrives.
    AttemptCancelled,
    /// The generic failure lane (every non-Cancelled failure status).
    Failure,
}

impl TerminalScope {
    /// THE scope classifier — one total function over the wire status
    /// alphabet; both render sites project from it.
    fn of(status: types::BuildResultStatus) -> Self {
        use types::BuildResultStatus as S;
        match status {
            S::Cancelled => TerminalScope::AttemptCancelled,
            S::Unspecified
            | S::Built
            | S::Substituted
            | S::AlreadyValid
            | S::PermanentFailure
            | S::TransientFailure
            | S::CachedFailure
            | S::DependencyFailed
            | S::LogLimitExceeded
            | S::OutputRejected
            | S::InfrastructureFailure
            | S::TimedOut
            | S::NotDeterministic
            | S::InputRejected
            | S::ExecutorVariantFailure => TerminalScope::Failure,
        }
    }
}

// r[impl gw.stderr.activity+2]
/// Relay one `DerivationEvent` (Started/Completed/Failed/Cached/Queued)
/// to the client as `actBuild` start/stop activity frames. Mutates
/// `act.display` to track which display family belongs to which
/// derivation across reconnects, and owns the live-tail subscription
/// lifecycle for the same events: `on_started` fires before the
/// duplicate-`Started` early-return (a duplicate `Started` carrying a
/// NEW exec_id must still replace the subscription — the old
/// execution's log is dead), `on_terminal` fires at the terminal arms'
/// heads. Display map and tail set move together — one lifecycle owner.
async fn relay_derivation_status<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    tails: &mut LogTailSet,
    drv_event: types::DerivationEvent,
) -> Result<(), StreamProcessError> {
    match drv_event.kind() {
        types::DerivationEventKind::Substituting => {
            // Idempotent across reconnects / duplicate events.
            if let Some(DrvDisplay::Subst(_)) = act.display.get(&drv_event.derivation_path) {
                return Ok(());
            }
            // The attempt IS a materialization now (the scheduler's
            // claim intake emits SUBSTITUTING for a re-dispatch through
            // the store lane, with no terminal event for the dead build
            // execution — an uncharged requeue leaves the drv
            // non-terminal). Flip an open build display through the
            // family-transition authority, then start the substitute
            // family exactly as the snapshot reconcile does.
            flip_to_family(
                stderr,
                act,
                tails,
                &drv_event.derivation_path,
                IncomingFamily::Subst,
            )
            .await?;
            let out = drv_event
                .output_paths
                .first()
                .cloned()
                .unwrap_or_else(|| drv_event.derivation_path.clone());
            start_subst_display(stderr, act, &drv_event.derivation_path, &out).await?;
        }
        types::DerivationEventKind::Started => {
            // r[impl gw.display.redispatch-footer]
            // sh-042: when this Started supersedes a closed
            // predecessor, emit the scheduler-authoritative `rio:
            // retry` marker on the EXISTING actBuild aid BEFORE the
            // tail supersession below — at this point neither
            // `tails.on_started` nor `flip_to_family` nor the
            // duplicate-check has run, so the old tail and
            // `act.display[drv] = DrvDisplay::Build(old_aid)` are
            // both still alive. The emit goes via `stderr.result`
            // directly to the wire (same shape as `relay_log_batch`),
            // not through the tail's `out_tx`. The `rio: retry`
            // prefix (NOT `rio: result` — `footer_lines` never emits
            // `rio: retry`) is order-independent against any
            // builder-emitted `rio: result cancelled (sigterm)` that
            // may still be queued in the predecessor tail's `out_tx`:
            // the unbiased `tokio::select!` and the 256-deep mpsc
            // mean a queued footer can drain on the NEXT iteration
            // onto the same reused aid in either order, and neither
            // line contradicts the other (one is the attempt's local
            // outcome; one is the scheduler's re-dispatch decision).
            if let Some(p) = drv_event.predecessor.as_ref() {
                let line = format!(
                    "rio: retry    {}{}",
                    rio_common::classify::attempt_terminal_reason_human(&p.termination_reason),
                    sizing_suffix(p.floor_bumped(), p.new_axis_bytes),
                );
                match act.display.get(&drv_event.derivation_path) {
                    Some(DrvDisplay::Build(aid)) => {
                        stderr
                            .result(*aid, ResultType::BuildLogLine, &[ResultField::String(line)])
                            .await?;
                    }
                    // Gateway↔scheduler reconnect cold-start (no aid
                    // open yet) or a Subst→Build flip the rfind's
                    // attempt_kind == Build guard normally precludes:
                    // STDERR_NEXT fallback, same as relay_log_batch.
                    _ => stderr.log(&format!("{line}\n")).await?,
                }
            }
            // Tail subscription first — BEFORE the duplicate-Started
            // early-return below: its arm re-uses the existing activity
            // id, while a duplicate Started carrying a NEW exec_id must
            // still replace the subscription (the old execution's log
            // is dead). `on_started` is idempotent for an unchanged
            // exec_id and ignores an empty one.
            tails.on_started(&drv_event.derivation_path, &drv_event.exec_id);
            // A failed substitute fetch reverts to Ready → may later
            // dispatch as a build. Flip the dangling actSubstitute +
            // actCopyPath pair closed so nom doesn't show it stuck
            // forever.
            flip_to_family(
                stderr,
                act,
                tails,
                &drv_event.derivation_path,
                IncomingFamily::Build {
                    subst_close: SubstCloseCause::FellThroughToBuild,
                },
            )
            .await?;
            // Re-dispatch (in-connection reassign, or replay after a
            // gateway↔scheduler reconnect) sends Started again for a
            // drv we already track. The existing aid is still valid on
            // the client (client→gateway never dropped), so reuse it.
            // Emitting a fresh start_activity makes nom count the
            // re-dispatch as a new build (live QA: 27 unique drvs →
            // 43 starts → "43/29"). I-206's prior start-then-stop
            // balanced the running count but still inflated total.
            if matches!(
                act.display.get(&drv_event.derivation_path),
                Some(DrvDisplay::Build(_))
            ) {
                debug!(
                    drv = %drv_event.derivation_path,
                    "duplicate Started; reusing existing activity"
                );
                return Ok(());
            }
            // actBuild fields per upstream
            // `derivation-building-goal.cc`:
            // [drvPath, machineName, curRound, nrRounds].
            // nom reads fields[0] for the drv name and
            // fields[1] for the "on <machine>" suffix;
            // rounds are fixed (1,1) — rio doesn't repeat.
            //
            // machineName: NOT the executor pod ID — that
            // leaks ephemeral pod names to the client and
            // breaks the "cluster is one machine" abstraction
            // (a build with 200 pods would show 200 machine
            // names cycling). Use a stable cluster identifier
            // from RIO_GATEWAY_MACHINE_NAME (helm sets it to
            // the cluster name); empty default = upstream's
            // local-build semantics (nom shows no suffix).
            // `drv_event.executor_id` intentionally unused — see above.
            let aid = stderr
                .start_activity(
                    ActivityType::Build,
                    &format!("building '{}'", drv_event.derivation_path),
                    verbosity::INFO,
                    act.builds_root.unwrap_or(0),
                    &[
                        ResultField::String(drv_event.derivation_path.clone()),
                        ResultField::String(act.machine_name.clone()),
                        ResultField::Int(1),
                        ResultField::Int(1),
                    ],
                )
                .await?;
            act.display
                .insert(drv_event.derivation_path.clone(), DrvDisplay::Build(aid));
        }
        types::DerivationEventKind::Completed => {
            // Terminal: stop the subscription from re-opening and let
            // its current stream drain, then close whichever display
            // family is open. Substituting → Completed shouldn't happen
            // via the normal scheduler FSM, but terminal-arm symmetry
            // costs nothing and guards future scheduler changes.
            tails.on_terminal(&drv_event.derivation_path);
            match act.display.remove(&drv_event.derivation_path) {
                Some(DrvDisplay::Subst(aids)) => {
                    stop_subst_pair(stderr, act, aids, SubstCloseCause::Completed).await?;
                }
                Some(DrvDisplay::Build(aid)) => {
                    stderr.stop_activity(aid).await?;
                    debug!(aid, drv = %drv_event.derivation_path, "stop_activity sent");
                }
                None => {
                    // Completed for a drv we never (or no
                    // longer) have an aid for — path-key
                    // mismatch (dispatch.rs drv_path vs
                    // completion.rs path_or_hash_fallback), or
                    // Started was dropped by a state-channel
                    // Lagged window. Display-only; the
                    // terminal drain below covers it.
                    debug!(
                        drv = %drv_event.derivation_path,
                        tracked = act.display.len(),
                        "Completed with no tracked activity"
                    );
                }
            }
        }
        types::DerivationEventKind::Failed => {
            // Terminal: cut the tail (as Completed above), close
            // whichever display family is open.
            // Scheduler path Substituting → (silent revert to Queued
            // via `handle_substitute_complete(ok=false)` with
            // `!all_deps_completed`) → DependencyFailed cascade emits
            // Failed without ever emitting Started/Cached — the subst
            // aid was never closed and nom showed a stuck
            // "substituting 'X'" line until build terminus.
            tails.on_terminal(&drv_event.derivation_path);
            match act.display.remove(&drv_event.derivation_path) {
                Some(DrvDisplay::Subst(aids)) => {
                    stop_subst_pair(stderr, act, aids, SubstCloseCause::Failed).await?;
                }
                Some(DrvDisplay::Build(aid)) => stderr.stop_activity(aid).await?,
                None => {
                    debug!(
                        drv = %drv_event.derivation_path,
                        tracked = act.display.len(),
                        "Failed with no tracked activity (I-206)"
                    );
                }
            }
            // Log failure via STDERR_NEXT, with a copy-pasteable
            // `rio-cli logs` hint for failures a fresh execution
            // backs. No `--build-id` needed — logs are keyed by
            // `(drv_hash, exec_id)` and `rio-cli logs <drv>` resolves
            // the latest execution, which is the one that just failed.
            // The drv path is single-quoted so the line is shell-safe
            // to copy-paste even when the drv name contains shell
            // metacharacters.
            //
            // Gated on the STATED fact `has_execution` (bug_080 — the
            // KD-fact law): only the emitter knows whether a fresh
            // execution of the current attempt cycle backs this
            // failure event, and it says so on the wire. The previous
            // gate INFERRED validity from a correlated status
            // (`!= DEPENDENCY_FAILED`) — it discharged exactly one
            // no-execution population (cascaded ancestors, who now
            // state NoExecution, preserving that behavior as a derived
            // consequence) while printing stale prior-execution hints
            // for the reportless family (spawn-gate poisons with no
            // pod and no attempt — real but WRONG-attempt logs, the
            // operator debugs the wrong attempt). Fail-closed: absent
            // => false => no hint (mixed-version skew loses a hint,
            // never mints a misleading one). In a `--keep-going` build
            // with N cascaded ancestors, suppressing N misleading
            // hints is the difference between a copy-pasteable failure
            // tail and noise.
            // r[impl gw.stderr.failure-hint]
            let hint = if drv_event.has_execution {
                format!("\n  ↳ rio-cli logs '{}'", drv_event.derivation_path)
            } else {
                String::new()
            };
            // The vocabulary partition (live_051(f1)): cycle-scoped
            // cancellations say so — operators reading the stream must
            // never mistake a self-healing attempt close for a final
            // build cancellation (the live incident read reap-raced
            // requeues as "cancelled python builds" while every
            // affected build was requeued and running).
            let line = match TerminalScope::of(drv_event.failure_status()) {
                TerminalScope::AttemptCancelled => format!(
                    "derivation '{}' attempt cancelled (cycle closed; retriable via \
                     explicit resubmit): {}{hint}",
                    drv_event.derivation_path, drv_event.error_message
                ),
                TerminalScope::Failure => format!(
                    "derivation '{}' failed: {}{hint}",
                    drv_event.derivation_path, drv_event.error_message
                ),
            };
            stderr.log(&line).await?;
        }
        types::DerivationEventKind::Cached => {
            // Terminal: cut the tail (as Completed/Failed above), then
            // close whichever display family is open — TOTAL over
            // DrvDisplay (sh-035). Substituting → Cached is the common
            // path (close the actSubstitute + actCopyPath pair).
            // Started → Cached is the reap→store-hit path: the
            // scheduler reaps an in-flight execution, re-probes the
            // store and hits (`dispatch.rs`
            // `complete_ready_from_store_batch`), emitting Cached with
            // no intervening Completed — the open `actBuild` aid must
            // close here (a plain `stop_activity`; no progress synth —
            // build aids carry no resProgress bar). Merge-time cache
            // hits never opened a display → None → no-op.
            tails.on_terminal(&drv_event.derivation_path);
            match act.display.remove(&drv_event.derivation_path) {
                Some(DrvDisplay::Subst(aids)) => {
                    stop_subst_pair(stderr, act, aids, SubstCloseCause::Cached).await?;
                }
                Some(DrvDisplay::Build(aid)) => {
                    stderr.stop_activity(aid).await?;
                    debug!(aid, drv = %drv_event.derivation_path,
                           "Cached after Build (reap→store-hit) — stop_activity sent");
                }
                None => {}
            }
        }
        // Queued: non-terminal; no display family to close, no STDERR.
        types::DerivationEventKind::Queued => {}
    }
    Ok(())
}

// r[impl gw.activity.stop-parity]
/// Emit `stop_activity` for every aid still tracked in `act.display`
/// (both display families). Called once at build terminus before the
/// root `actBuilds` stop.
///
/// A non-empty drain set means an upstream `DerivationEvent` was lost:
/// scheduler state-channel `Lagged` (rare post-split), a Started/
/// Completed path-key mismatch (`dispatch.rs drv_path` vs `completion.rs
/// path_or_hash_fallback`), or a future bug. Without this the client's
/// nom shows the drv stuck at its last phase forever — the Apr-7
/// large-shallow repro had 44 `start` / 34 `stop` on the wire.
async fn drain_unstopped_activities<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
) -> Result<(), StreamProcessError> {
    if !act.display.is_empty() {
        tracing::warn!(
            tracked = act.display.len(),
            "draining unstopped activities at terminal (upstream event loss)"
        );
    }
    // ONE drain over THE key set — both display families, by
    // construction. Collect-first: the close chokepoint needs `act`
    // for the copy-less denominator retire, so the drain cannot hold
    // the map borrow across the calls. r[impl gw.display.single-map]
    let drained: Vec<(String, DrvDisplay)> = act.display.drain().collect();
    for (drv, display) in drained {
        match display {
            DrvDisplay::Subst(aids) => {
                let (subst, copy) = (aids.subst, aids.copy);
                stop_subst_pair(stderr, act, aids, SubstCloseCause::TerminalDrain).await?;
                debug!(subst, ?copy, %drv,
                       "stop_activity sent (terminal drain, subst pair)");
            }
            DrvDisplay::Build(aid) => {
                stderr.stop_activity(aid).await?;
                debug!(aid, %drv, "stop_activity sent (terminal drain)");
            }
        }
    }
    Ok(())
}

/// Outcome of one progress relay — the straggler lane typed
/// (live_043): a tick arriving for an already-closed (or never
/// tracked) pair is TOLERATED — dropped with a counted debug naming
/// drv + done/expected — never reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubstRelay {
    /// The tracked pair absorbed the tick: the copy child possibly
    /// started, and frames were emitted iff the copy child exists
    /// (the parent is structural — a pre-copy empty-URI tick is
    /// absorbed frameless).
    Relayed,
    /// No pair tracked: the pair already closed (post-outcome
    /// straggler) or `Substituting` was lost — display-only.
    DroppedUntracked,
}

// r[impl gw.activity.subst-progress+4]
/// THE substitute-progress relay chokepoint (live_043; emission lane
/// corrected in live_045): one code path owns, per tick, (i) the
/// DEFERRED `actCopyPath` start on the first NON-EMPTY `upstream_uri`
/// (frame `[out, from=uri, to=machine]`, the stock `copyStorePath`
/// shape — `from` is pinned to the FIRST URI; later upstream changes
/// within the aggregate walk are accepted cosmetic loss), and (ii)
/// `resProgress` on the copy child iff started — NEVER on the
/// `actSubstitute` parent, which is structural (stock convention:
/// substitution progress rides the `copyStorePath` child only;
/// direction-aware consumers dedup nested copies by `(path, host)`,
/// and the parent's empty substituter URI parses as Localhost, never
/// matching the child's source, so a parent emission renders a
/// second live row fed identical numbers — every byte displayed 2x).
/// nom renders `[done/expected]` as the download bar. A tick with no
/// tracked pair is dropped-with-debug (the typed
/// [`SubstRelay::DroppedUntracked`] lane).
async fn relay_substitute_progress<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    p: types::SubstituteProgress,
) -> Result<SubstRelay, StreamProcessError> {
    let machine_name = act.machine_name.clone();
    let Some(DrvDisplay::Subst(aids)) = act.display.get_mut(&p.derivation_path) else {
        debug!(
            drv = %p.derivation_path,
            done = p.bytes_done,
            expected = p.bytes_expected,
            "substitute progress for an untracked or already-closed pair; dropped"
        );
        return Ok(SubstRelay::DroppedUntracked);
    };
    // (i) Deferred copy start: first sourced tick only. Fields per
    // upstream `store-api.cc copyStorePath`: [storePath, from, to];
    // `to` is the cluster's stable identifier (same source as
    // actBuild machineName). The text says "fetching closure of"
    // (not "copying path") because resProgress carries
    // CLOSURE-aggregate bytes from `walk_substitute_closure`, not
    // single-path bytes.
    if aids.copy.is_none() && !p.upstream_uri.is_empty() {
        let copy = stderr
            .start_activity(
                ActivityType::CopyPath,
                &format!("fetching closure of '{}'", aids.out),
                verbosity::INFO,
                aids.subst,
                &[
                    ResultField::String(aids.out.clone()),
                    ResultField::String(p.upstream_uri.clone()),
                    ResultField::String(machine_name),
                ],
            )
            .await?;
        aids.copy = Some(copy);
        // Backfill the parent's substituterUri via SetPhase: the wire
        // protocol has no field-update frame, so the empty fields[1]
        // at start cannot be re-sent. nom renders the per-activity
        // phase suffix ("from <uri>"); nxb's SetPhase handler is
        // activity_drv-gated to Build/PostBuildHook so this is
        // nxb-invisible — its substitution_in_flight host-dedup
        // double-count needs parent-aid linkage out-of-tree
        // (sh-034).
        stderr
            .result(
                aids.subst,
                ResultType::SetPhase,
                &[ResultField::String(format!("from {}", p.upstream_uri))],
            )
            .await?;
    }
    // (ii) The copy child iff started — the ONLY progress lane. An
    // empty-URI tick with no copy child (job-level commit walk of a
    // locally-present closure) emits nothing: zero-fetch progress is
    // not a download.
    if let Some(copy) = aids.copy {
        // resProgress fields: [done, expected, running, failed]. The
        // latter two are 0 — the pair is single-transfer, not an
        // aggregate.
        let fields = [
            ResultField::Int(p.bytes_done),
            ResultField::Int(p.bytes_expected),
            ResultField::Int(0),
            ResultField::Int(0),
        ];
        stderr.result(copy, ResultType::Progress, &fields).await?;
    }
    aids.progress = Some((p.bytes_done, p.bytes_expected));
    Ok(SubstRelay::Relayed)
}

/// The `resProgress` field array `[done, expected, running, failed]` —
/// the ONLY producer of the root progress payload. The live `Progress`
/// arm and the snapshot correction both call this, so the two surfaces
/// cannot drift (the live arm hardcoded `failed = 0` while the
/// snapshot carried the real count, and a build's failed column
/// flickered to 0 on every live update after a reconnect).
fn build_progress_fields(done: u64, expected: u64, running: u64, failed: u64) -> [ResultField; 4] {
    [
        ResultField::Int(done),
        ResultField::Int(expected),
        ResultField::Int(running),
        ResultField::Int(failed),
    ]
}

// r[impl gw.stderr.result.progress]
/// Relay `BuildStarted` / `Progress` events to the top-level
/// `actBuilds` activity. `Started` opens the root activity (idempotent
/// across reconnects) and emits `SetExpected{actBuild, N}`; `Progress`
/// emits `resProgress{done, expected, running, failed}`.
async fn relay_build_progress<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    ev: &types::build_event::Event,
) -> Result<(), StreamProcessError> {
    match ev {
        types::build_event::Event::Started(started) => {
            debug!(
                total = started.total_derivations,
                cached = started.cached_derivations,
                "build started"
            );
            // Top-level actBuilds activity. nom and progress-bar
            // aggregate per-drv actBuild children under this; the
            // SetExpected{actBuild, N} result drives the "N/M
            // built" denominator. Idempotent: a second BuildStarted
            // (replayed via WatchBuild after reconnect) re-uses the
            // existing root rather than emitting a duplicate.
            if act.builds_root.is_none() {
                let aid = stderr
                    .start_activity(ActivityType::Builds, "", verbosity::DEBUG, 0, &[])
                    .await?;
                act.builds_root = Some(aid);
            }
            let to_build = u64::from(
                started
                    .total_derivations
                    .saturating_sub(started.cached_derivations),
            );
            if let Some(aid) = act.builds_root {
                stderr
                    .result(
                        aid,
                        ResultType::SetExpected,
                        &[
                            ResultField::Int(ActivityType::Build as u64),
                            ResultField::Int(to_build),
                        ],
                    )
                    .await?;
            }
        }
        types::build_event::Event::Progress(prog) => {
            debug!(
                completed = prog.completed,
                running = prog.running,
                queued = prog.queued,
                total = prog.total,
                "build progress"
            );
            // resProgress fields: [done, expected, running, failed].
            // `expected` is `total` (NOT total-cached): upstream
            // progress-bar takes `max(SetExpected, Progress.expected)`
            // so this is informational.
            if let Some(aid) = act.builds_root {
                stderr
                    .result(
                        aid,
                        ResultType::Progress,
                        &build_progress_fields(
                            u64::from(prog.completed),
                            u64::from(prog.total),
                            u64::from(prog.running),
                            u64::from(prog.failed),
                        ),
                    )
                    .await?;
            }
        }
        _ => unreachable!("relay_build_progress only handles Started/Progress"),
    }
    Ok(())
}

// r[impl gw.reconnect.snapshot-resync]
/// Apply a `BuildSnapshot` (the first message on a WatchBuild stream) to
/// the session's display state: short-circuit to a terminal outcome if the
/// build finished while the gateway was detached, otherwise reconcile
/// per-drv activities and log tails against the snapshot's running set and
/// correct the aggregate progress counters.
///
/// This replaces event replay: instead of re-delivering the events the
/// gateway missed, the scheduler describes the resulting state and the
/// gateway diffs its own tracking against it.
async fn apply_snapshot<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    tails: &mut LogTailSet,
    snap: types::BuildSnapshot,
) -> Result<Option<BuildEventOutcome>, StreamProcessError> {
    // Terminal short-circuit: the build reached its outcome while we were
    // detached. The snapshot carries the same payload the terminal event
    // would have.
    match snap.state() {
        types::BuildState::Succeeded => return Ok(Some(BuildEventOutcome::Completed)),
        types::BuildState::Failed => {
            return Ok(Some(BuildEventOutcome::Failed {
                status: snap.failure_status(),
                error_message: snap.error_message,
            }));
        }
        types::BuildState::Cancelled => {
            return Ok(Some(BuildEventOutcome::Cancelled {
                reason: snap.cancel_reason,
            }));
        }
        _ => {}
    }

    // Ensure the root actBuilds activity exists. A reconnect that lands
    // before BuildStarted arrived (stream broke between submit-accept and
    // event 0) — or any fresh watch attach — needs the root for progress
    // results and as the per-drv activity parent. The scheduler emits
    // BuildStarted exactly once at merge and never re-delivers it, so the
    // snapshot is what (re)creates the root in that case.
    if act.builds_root.is_none() {
        let aid = stderr
            .start_activity(ActivityType::Builds, "", verbosity::DEBUG, 0, &[])
            .await?;
        act.builds_root = Some(aid);
        let to_build = u64::from(
            snap.total_derivations
                .saturating_sub(snap.cached_derivations),
        );
        stderr
            .result(
                aid,
                ResultType::SetExpected,
                &[
                    ResultField::Int(ActivityType::Build as u64),
                    ResultField::Int(to_build),
                ],
            )
            .await?;
    }

    let running: HashMap<&str, (&str, types::AttemptKind)> = snap
        .running
        .iter()
        .map(|r| (r.derivation_path.as_str(), (r.exec_id.as_str(), r.kind())))
        .collect();

    // Tracked drvs that are no longer running: they reached a terminal
    // state while we were detached (their Completed/Failed event is gone
    // with the old stream). ONE sweep over THE key set closes whichever
    // display family is open — a substitution that completed during the
    // detach is closed by the same loop that closes finished builds.
    // r[impl gw.display.single-map]
    let gone: Vec<String> = act
        .display
        .keys()
        .filter(|drv| !running.contains_key(drv.as_str()))
        .cloned()
        .collect();
    for drv in gone {
        match act.display.remove(&drv) {
            Some(DrvDisplay::Build(aid)) => {
                // Arm the log tail's post-terminal drain so its final
                // lines still land.
                tails.on_terminal(&drv);
                stderr.stop_activity(aid).await?;
                debug!(%drv, "stop_activity sent (snapshot reconcile: drv no longer running)");
            }
            Some(DrvDisplay::Subst(aids)) => {
                stop_subst_pair(stderr, act, aids, SubstCloseCause::SnapshotGone).await?;
                debug!(%drv, "subst pair stopped (snapshot reconcile: no longer running)");
            }
            None => {}
        }
    }

    // Running drvs, routed by attempt kind. r[impl sched.pull.kinded-running-surface]
    //
    // BUILD (and UNSPECIFIED, from a scheduler that predates the wire
    // field — degrades to the uniform build treatment): start the
    // actBuild activity and attach the log tail; `on_started` is
    // idempotent for an unchanged exec_id and replaces the subscription
    // on a re-dispatch.
    //
    // MATERIALIZATION: store-side upstream fetch — the substitute
    // display family, NO log tail (the execution never logs), NO
    // actBuild. Mirrors the live event-path split (`Substituting` vs
    // `Started`).
    for (drv, (exec_id, kind)) in &running {
        match kind {
            types::AttemptKind::Materialization => {
                // Kind flip while detached (the build re-dispatched as
                // a materialization): the family-transition authority
                // closes the stale build display and its tail.
                flip_to_family(stderr, act, tails, drv, IncomingFamily::Subst).await?;
                if !act.display.contains_key(*drv) {
                    start_subst_display(stderr, act, drv, drv).await?;
                }
            }
            _ => {
                tails.on_started(drv, exec_id);
                // Kind flip while detached (substitute fell through to
                // a build): the authority closes the dangling pair.
                flip_to_family(
                    stderr,
                    act,
                    tails,
                    drv,
                    IncomingFamily::Build {
                        subst_close: SubstCloseCause::SnapshotKindFlip,
                    },
                )
                .await?;
                if !act.display.contains_key(*drv) {
                    // Same actBuild shape as relay_derivation_status's
                    // Started arm.
                    let aid = stderr
                        .start_activity(
                            ActivityType::Build,
                            &format!("building '{drv}'"),
                            verbosity::INFO,
                            act.builds_root.unwrap_or(0),
                            &[
                                ResultField::String((*drv).to_string()),
                                ResultField::String(act.machine_name.clone()),
                                ResultField::Int(1),
                                ResultField::Int(1),
                            ],
                        )
                        .await?;
                    act.display
                        .insert((*drv).to_string(), DrvDisplay::Build(aid));
                }
            }
        }
    }

    // Correct the aggregate display from the snapshot's absolute counts —
    // the same producer as the live Progress arm.
    if let Some(aid) = act.builds_root {
        stderr
            .result(
                aid,
                ResultType::Progress,
                &build_progress_fields(
                    u64::from(snap.completed_derivations),
                    u64::from(snap.total_derivations),
                    u64::from(snap.running_derivations),
                    u64::from(snap.failed_derivations),
                ),
            )
            .await?;
    }

    Ok(None)
}

/// Process a BuildEvent stream from the scheduler and translate events
/// into STDERR protocol messages for the Nix client.
///
/// Also consumes the build's log-tail output channel: build-log lines
/// do not arrive on the scheduler's event stream — they arrive from
/// per-derivation
/// `TailLog` subscriptions to rio-store, managed by `tails` and fed
/// through `log_rx`. The two sources are `select!`ed so a quiet
/// scheduler stream doesn't starve the live tail and vice versa.
///
/// Returns the final BuildResult on success, or a typed error.
#[instrument(skip_all)]
async fn process_build_events<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    event_stream: &mut tonic::codec::Streaming<types::BuildEvent>,
    budget: &mut ReattachBudget,
    act: &mut BuildActivityState,
    tails: &mut LogTailSet,
    log_rx: &mut tokio::sync::mpsc::Receiver<TaggedLogChunk>,
) -> Result<BuildEventOutcome, StreamProcessError> {
    // Disarms the log-tail branch if the channel ever closes. `None`
    // is unreachable while the LogTailSet (which holds a sender clone
    // for the build's lifetime) is alive — but without this guard,
    // `recv()` on a closed drained channel returns `Ready(None)`
    // immediately, that branch wins every `select!` iteration, and the
    // loop spins at 100% CPU. The guard makes the failure mode "no
    // live tail" instead.
    let mut log_open = true;
    loop {
        // Both arms are cancel-safe: `Streaming::message` keeps its
        // decode state in the stream itself (a partially-received
        // frame resumes on the next poll), and `Receiver::recv` is
        // documented cancel-safe.
        let event = tokio::select! {
            msg = event_stream.message() => {
                match msg.map_err(StreamProcessError::Transport)? {
                    Some(event) => event,
                    // Stream ended without a terminal event (scheduler
                    // disconnected). Do NOT send STDERR_ERROR here:
                    // submit_and_process_build catches this Err and
                    // converts it to Ok(BuildResult::failure), which
                    // callers then send via STDERR_LAST + BuildResult.
                    // Sending STDERR_ERROR first would produce an
                    // invalid STDERR_ERROR -> STDERR_LAST sequence.
                    //
                    // EofWithoutTerminal: clean stream close (Ok(None)).
                    // This IS what a scheduler failover looks like —
                    // k8s pod kill → SIGTERM → graceful shutdown → TCP
                    // FIN. The caller's reconnect loop retries this the
                    // same as Transport.
                    None => return Err(StreamProcessError::EofWithoutTerminal),
                }
            }
            chunk = log_rx.recv(), if log_open => {
                match chunk {
                    Some(chunk) => relay_log_batch(stderr, act, chunk.into_batch()).await?,
                    None => {
                        debug!("log-tail channel closed while the build is still running");
                        log_open = false;
                    }
                }
                continue;
            }
        };

        // Note the event against the re-attach budget. ORGANIC events
        // reset it (not WatchBuild Ok() — that only proves the
        // scheduler accepted the RPC; and not Snapshot/ResyncRequired —
        // connection machinery must consume the budget, or a
        // snapshot-then-resync storm refreshes its own cap forever at
        // zero backoff, which is exactly the pre-fix bug). A scheduler
        // that accepts and immediately drops the stream charges one
        // cycle per drop and exhausts within MAX_RECONNECT.
        if let Some(ref ev) = event.event {
            budget.note_event(ev);
        }

        use types::build_event::Event;
        match event.event {
            Some(Event::Phase(phase)) => {
                // r[impl gw.stderr.result.set-phase]
                // Builder forwarded the daemon's SetPhase. Attach to the
                // owning per-drv activity so nom shows the phase column.
                if let Some(DrvDisplay::Build(aid)) =
                    act.display.get(&phase.derivation_path).cloned()
                {
                    stderr
                        .result(
                            aid,
                            ResultType::SetPhase,
                            &[ResultField::String(phase.phase)],
                        )
                        .await?;
                }
            }
            Some(Event::Derivation(drv_event)) => {
                // Display map AND live-tail subscriptions move together
                // inside the relay (one lifecycle owner — see
                // relay_derivation_status's doc for the ordering the
                // arms preserve).
                relay_derivation_status(stderr, act, tails, drv_event).await?;
            }
            Some(Event::SubstituteProgress(p)) => {
                relay_substitute_progress(stderr, act, p).await?;
            }
            Some(ref ev @ (Event::Started(_) | Event::Progress(_))) => {
                relay_build_progress(stderr, act, ev).await?;
            }
            Some(Event::InputsResolved(_)) => {
                // Scheduler's store cache-check done; dispatch begins
                // next. The info to print (N to build = total - cached)
                // arrived in Started above as SetExpected.
                debug!("build inputs resolved");
            }
            Some(Event::Snapshot(snap)) => {
                // First message of a WatchBuild (reconnect) stream: the
                // build's current state. Either short-circuits to a
                // terminal outcome or reconciles activity/tail tracking.
                if let Some(outcome) = apply_snapshot(stderr, act, tails, snap).await? {
                    return Ok(outcome);
                }
            }
            Some(Event::Completed(_)) => return Ok(BuildEventOutcome::Completed),
            Some(Event::Failed(failed)) => {
                return Ok(BuildEventOutcome::Failed {
                    status: failed.status(),
                    error_message: failed.error_message,
                });
            }
            Some(Event::Cancelled(cancelled)) => {
                return Ok(BuildEventOutcome::Cancelled {
                    reason: cancelled.reason,
                });
            }
            // r[impl gw.resync.loss-signal+1]
            Some(Event::ResyncRequired(_)) => {
                // The scheduler dropped events for THIS watcher
                // (broadcast lag). Everything reconciles from a fresh
                // WatchBuild snapshot — running set (re-opens lagged
                // tails via the idempotent on_started), activities,
                // counters. No per-event-type guesswork.
                return Err(StreamProcessError::ResyncRequired);
            }
            None => {}
        }
    }
}

/// Outcome of processing a build event stream.
enum BuildEventOutcome {
    Completed,
    Failed {
        status: types::BuildResultStatus,
        error_message: String,
    },
    Cancelled {
        reason: String,
    },
}

// r[impl gw.reconnect.backoff+3]
/// Re-attach budget and ladder shared by the watch loop and
/// [`ReattachBudget`]. See the rationale comment at the loop in
/// [`submit_and_process_build`] for why 10.
const MAX_RECONNECT: u32 = 10;
const RECONNECT_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_secs(1),
    mult: 2.0,
    cap: std::time::Duration::from_secs(16),
    jitter: rio_common::backoff::Jitter::None,
};

/// Where the next scheduler `BuildEvent` comes from.
///
/// The moment a loss signal (`ResyncRequired`) or a stream death
/// (`Transport`/`EofWithoutTerminal`) is observed, the prior stream is
/// DROPPED by moving to [`EventSource::NeedsReattach`] — consuming
/// post-gap events from a dead stream is unrepresentable, because
/// [`process_build_events`] is only ever handed the stream owned by
/// [`EventSource::Live`], and the only transition back to `Live` is a
/// successful `WatchBuild` re-attach whose FIRST message is the
/// snapshot (consumed and reconciled before `Live` is constructed).
///
/// The pre-fix shape this forecloses (merged_bug_056): a failed
/// `WatchBuild` left the old `event_stream` binding in place and the
/// next loop iteration re-entered `process_build_events` on it — a
/// dead-but-buffered stream could serve events recorded BEFORE the
/// loss signal as if the display were whole.
// r[impl gw.resync.snapshot-owed]
enum EventSource {
    /// A healthy stream: either the initial `SubmitBuild` response
    /// stream, or a re-attached `WatchBuild` stream whose snapshot has
    /// already been consumed and reconciled. Boxed: the streaming
    /// decoder is large and `NeedsReattach` is a unit
    /// (clippy::large_enum_variant).
    Live(Box<tonic::codec::Streaming<types::BuildEvent>>),
    /// The previous stream is gone (loss signal, transport error, or
    /// clean EOF without a terminal). The owed snapshot has not yet
    /// arrived: no event may be consumed until a fresh `WatchBuild`
    /// open succeeds AND its first message is the snapshot.
    NeedsReattach,
}

/// The closed cause alphabet of [`ReattachBudget::next_backoff`]
/// (merged_bug_083): every way a watch cycle can die is a variant
/// here, and the chokepoint matches it exhaustively — a new death arm
/// cannot compile without deciding its pacing posture at the
/// chokepoint. Per-arm axis choice (the pre-fix bypass: the rate axis
/// consulted only on the resync arm) is unwritable, because the arms
/// no longer see an axis API at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackoffCause {
    /// Server-signalled watcher lag (`ResyncRequired`): the scheduler
    /// is healthy, this watcher fell behind. Zero backoff inside the
    /// consecutive non-organic streak; silent to the client.
    ResyncSignal,
    /// gRPC-level death of the live stream.
    Transport,
    /// The live stream ended without a terminal event (the scheduler
    /// failover signature).
    EofWithoutTerminal,
    /// A `NeedsReattach` cycle failed before any stream existed
    /// (`WatchBuild` open rejected, or no/garbled snapshot).
    ReattachCycleFailed,
}

impl BackoffCause {
    /// Label value for the rate-paced counter's `cause` label.
    fn label(self) -> &'static str {
        match self {
            BackoffCause::ResyncSignal => "resync_signal",
            BackoffCause::Transport => "transport",
            BackoffCause::EofWithoutTerminal => "eof_without_terminal",
            BackoffCause::ReattachCycleFailed => "reattach_cycle_failed",
        }
    }
}

/// Per-cycle effects record (the merged_bug_083 closure set), minted
/// ONLY by [`ReattachBudget::next_backoff`]: the charge, the sleep,
/// the exhaustion break verdict, and the rate-axis observability all
/// travel together — an arm consumes the record wholesale and cannot
/// take the pacing without the observability, or pick which axis to
/// consult (the counter tick itself fires inside the chokepoint).
struct PacingDecision {
    /// Cycle number after this charge (for logs and the client line).
    attempt: u32,
    /// The consecutive-streak budget is spent: the loop MUST break
    /// with its error. Exhaustion is streak-keyed only — the rate
    /// axis paces, never fails (gw.resync.reattach-budget).
    exhausted: bool,
    /// Mandatory sleep before the next cycle: the MAX of the streak
    /// ladder and the rate ladder — both bounds hold on every death
    /// arm. Zero = immediate.
    sleep: std::time::Duration,
    /// The rate axis engaged (token-bucket deficit): the counter was
    /// ticked with this decision's cause label; logs carry the slow-
    /// consumer operator signal.
    rate_paced: bool,
}

/// Bounds consecutive re-attach cycles, resetting only on organic
/// build events or on evidenced recovery.
///
/// "Organic" = an event produced by build progress itself (started,
/// progress, derivation, phase, substitute-progress, inputs-resolved,
/// or a terminal). `Snapshot` and `ResyncRequired` are
/// connection-machinery events: a scheduler that serves every
/// re-attach a snapshot and then immediately signals loss again must
/// CONSUME the budget, not refresh it — pre-fix, the reset on any
/// event made `MAX_RECONNECT` vacuous for exactly that storm
/// (counter: 0 → 1 → snapshot resets → 0 → … forever, at zero
/// backoff).
///
/// "Evidenced recovery" = the dying stream held `Live` longer than
/// [`Self::LIVE_TENURE_RESET`] (bug_068): a long single-derivation
/// compile emits no organic event for hours (progress is
/// state-change-driven; logs ride the store-side `LogTailSet`), so
/// without a tenure reset the per-build budget becomes a lifetime
/// counter and failover #2 of a quiet build exhausts mid-recovery.
/// Storms never accrue tenure — they cycle death→snapshot in
/// seconds — so the storm bound above is preserved.
// r[impl gw.resync.reattach-budget+3]
// (bug_160: the budget is TWO-AXIS — the consecutive streak above,
// and a wall-clock cycle-rate axis organic events cannot reset.
// merged_bug_083: BOTH axes bound EVERY death arm through the single
// next_backoff(cause) chokepoint, and the rate evidence is a token
// bucket whose refill is independent of consumption.)
struct ReattachBudget {
    /// Consecutive re-attach cycles since the last organic event or
    /// evidenced recovery.
    attempts: u32,
    /// When the stream last (re-)entered `Live`. Armed by
    /// [`Self::note_live_entered`], consumed by
    /// [`Self::note_reattach`]: a tenure that outlasted
    /// [`Self::LIVE_TENURE_RESET`] proves the previous outage ended,
    /// so its charges do not bleed into the new one.
    live_since: Option<std::time::Instant>,
    /// Rate-axis token bucket (bug_160 evidence, merged_bug_083
    /// mechanism): capacity [`Self::RATE_MAX`], refilled RATE_MAX per
    /// fully elapsed [`Self::RATE_WINDOW`] (window-quantized), each
    /// cycle consumes one. Negative = the deficit the ladder rung
    /// derives from. The streak axis (`attempts`) resets on organic
    /// progress — correct for "is the scheduler serving us", wrong as
    /// the ONLY bound: a durably-slow consumer of a chatty build
    /// interleaves death→snapshot→organic forever, riding zero
    /// backoff while charging the scheduler one O(DAG) snapshot per
    /// cycle. Refill is TIME-based and independent of consumption —
    /// `note_event` cannot touch it, and (unlike the pre-fix eviction
    /// window) paced cycles cannot evict the proof that pacing is
    /// needed, so the paced fixed point is reachable.
    rate_tokens: i64,
    /// Start of the current refill window. `None` ⇔ the bucket is
    /// full and no window is running; the next charge anchors it.
    rate_window_started: Option<std::time::Instant>,
}

impl Default for ReattachBudget {
    fn default() -> Self {
        Self {
            attempts: 0,
            live_since: None,
            // The bucket starts full: a fresh watch owes nothing.
            rate_tokens: Self::RATE_MAX as i64,
            rate_window_started: None,
        }
    }
}

impl ReattachBudget {
    /// Re-attach cycles that proceed with ZERO backoff after a loss
    /// signal. Past this streak the scheduler is evidently not making
    /// progress on our behalf (every cycle yielded only
    /// snapshot/resync machinery), so the standard reconnect ladder
    /// applies — a lagging-but-healthy scheduler stays invisible to
    /// the client, a resync storm degrades into bounded, paced
    /// retries.
    const ZERO_BACKOFF_STREAK: u32 = 3;

    /// An event arrived on the live stream. Organic events reset the
    /// budget; connection-machinery events (`Snapshot`,
    /// `ResyncRequired`) do not. Exhaustive on purpose: a NEW event
    /// variant fails compilation here and forces the organic/machinery
    /// classification to be made explicitly.
    fn note_event(&mut self, event: &types::build_event::Event) {
        use types::build_event::Event;
        match event {
            Event::Started(_)
            | Event::Progress(_)
            | Event::Derivation(_)
            | Event::Completed(_)
            | Event::Failed(_)
            | Event::Cancelled(_)
            | Event::InputsResolved(_)
            | Event::Phase(_)
            | Event::SubstituteProgress(_) => self.attempts = 0,
            Event::Snapshot(_) | Event::ResyncRequired(_) => {}
        }
    }

    /// Live tenure past which a stream death opens a NEW outage
    /// rather than continuing the previous one. Storms cycle
    /// death→snapshot→death within seconds (the resync path is
    /// zero-backoff inside [`Self::ZERO_BACKOFF_STREAK`] and capped
    /// at 16s by `RECONNECT_BACKOFF` past it), so they never reach
    /// this; a genuine recovery holds `Live` for minutes. ~4× the
    /// backoff cap.
    const LIVE_TENURE_RESET: std::time::Duration = std::time::Duration::from_secs(60);

    /// The watch (re-)entered `Live`: arm the tenure clock that
    /// [`Self::note_reattach`] consults at the stream's death.
    fn note_live_entered(&mut self) {
        self.live_since = Some(std::time::Instant::now());
    }

    /// Test hook: pretend `Live` was entered `by` ago.
    #[cfg(test)]
    fn backdate_live_since_for_test(&mut self, by: std::time::Duration) {
        if let Some(t) = self.live_since.as_mut() {
            *t = t.checked_sub(by).expect("backdated Instant underflow");
        }
    }

    /// Test hook: age the rate bucket's refill window by `by` (drives
    /// refill maturation without real sleeps — the simulated-clock
    /// advance of the deterministic cadence tests).
    #[cfg(test)]
    fn backdate_rate_window_for_test(&mut self, by: std::time::Duration) {
        if let Some(t) = self.rate_window_started.as_mut() {
            *t = t.checked_sub(by).expect("backdated Instant underflow");
        }
    }

    /// Charge one re-attach cycle (stream death, loss signal, or a
    /// failed `WatchBuild`/snapshot read). Returns the cycle number
    /// for logging. Internal to [`Self::next_backoff`] — arms never
    /// charge directly.
    ///
    /// Consumes the armed `Live` tenure first: a dying stream that
    /// stayed `Live` past [`Self::LIVE_TENURE_RESET`] is recovery
    /// evidence — the charges that bought it belong to a previous,
    /// finished outage, so this cycle starts a fresh budget
    /// (bug_068). `take()` so one `Live` entry arms exactly one
    /// potential reset: repeated failures inside `NeedsReattach`
    /// keep charging the same outage.
    ///
    /// The rate bucket refills window-quantized: RATE_MAX tokens per
    /// FULLY elapsed RATE_WINDOW since the window anchor, clamped at
    /// capacity — so "beyond RATE_MAX cycles inside one RATE_WINDOW"
    /// always runs the bucket into deficit, and sustained consumption
    /// above RATE_MAX/RATE_WINDOW keeps it there regardless of how
    /// the pacing spaces the cycles.
    fn note_reattach(&mut self) -> u32 {
        let now = std::time::Instant::now();
        let capacity = Self::RATE_MAX as i64;
        if let Some(start) = self.rate_window_started {
            let windows = (now.saturating_duration_since(start).as_nanos()
                / Self::RATE_WINDOW.as_nanos()) as u32;
            if windows > 0 {
                self.rate_tokens = (self.rate_tokens + i64::from(windows) * capacity).min(capacity);
                self.rate_window_started = if self.rate_tokens == capacity {
                    // Full again — the next charge re-anchors.
                    None
                } else {
                    Some(start + Self::RATE_WINDOW * windows)
                };
            }
        }
        if self.rate_window_started.is_none() {
            self.rate_window_started = Some(now);
        }
        self.rate_tokens -= 1;
        if let Some(since) = self.live_since.take()
            && since.elapsed() >= Self::LIVE_TENURE_RESET
        {
            self.attempts = 0;
        }
        self.attempts += 1;
        self.attempts
    }

    /// The budget is spent: too many consecutive cycles without an
    /// organic event.
    fn exhausted(&self) -> bool {
        self.attempts > MAX_RECONNECT
    }

    /// Wall-clock refill window for the rate axis (bug_160). Sized to
    /// the ladder: RATE_MAX zero-backoff cycles per minute is already
    /// one O(DAG) snapshot every ~10 s sustained — past that the loop
    /// is churning, organic progress or not.
    const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

    /// Token-bucket capacity AND refill amount per
    /// [`Self::RATE_WINDOW`]: cycles beyond `RATE_MAX` inside one
    /// refill window run the bucket into deficit and engage the
    /// ladder regardless of the consecutive streak.
    const RATE_MAX: usize = 6;

    /// THE pacing chokepoint (merged_bug_083): every death arm
    /// charges its cycle and obtains its whole effects record here —
    /// there is no other pacing API for arms to consult, so a
    /// per-arm axis choice cannot be written.
    ///
    /// Both axes bound every cause: the sleep is the MAX of the
    /// streak ladder (zero inside `ZERO_BACKOFF_STREAK` for the
    /// resync signal only — a lagging-but-healthy scheduler stays
    /// invisible; every other death cause pays the ladder from its
    /// first cycle) and the rate ladder (rung = bucket deficit − 1:
    /// 1s, 2s, 4s, … capped at 16s). Refill is time-based, so under
    /// sustained churn the deficit persists and the system settles at
    /// the SUSTAINABLE fixed point — about RATE_MAX cycles per
    /// RATE_WINDOW with the rung hovering at the 8–16s cap
    /// neighborhood — instead of the pre-fix limit cycle, where each
    /// paced cycle aged the eviction window that justified the
    /// pacing and the loop burst back to zero backoff.
    ///
    /// Exhaustion stays STREAK-keyed: the rate axis paces, never
    /// fails (gw.resync.reattach-budget — organic progress proves the
    /// scheduler serves us).
    fn next_backoff(&mut self, cause: BackoffCause) -> PacingDecision {
        let attempt = self.note_reattach();
        let exhausted = self.exhausted();
        // r[impl obs.gateway.stream-end-attributed]
        // Every stream-end charged here is attributed by cause; the
        // rate-paced counter below is the SUBSET that engaged the rate
        // ladder. live_064: the per-cause split is what an incident
        // query reads — pre-fix only the rate-paced subset was
        // countable, so the benign-EOF vs transport vs reattach-refused
        // split was reconstructible only from debug-level logs.
        metrics::counter!(
            "rio_gateway_build_reattach_total",
            "cause" => cause.label()
        )
        .increment(1);
        let streak_sleep = match cause {
            BackoffCause::ResyncSignal => {
                if self.attempts > Self::ZERO_BACKOFF_STREAK {
                    RECONNECT_BACKOFF.duration(self.attempts - 1)
                } else {
                    std::time::Duration::ZERO
                }
            }
            BackoffCause::Transport
            | BackoffCause::EofWithoutTerminal
            | BackoffCause::ReattachCycleFailed => {
                RECONNECT_BACKOFF.duration(self.attempts.saturating_sub(1))
            }
        };
        let deficit = u32::try_from(-self.rate_tokens).unwrap_or(0);
        let rate_paced = deficit > 0;
        let rate_sleep = if rate_paced {
            RECONNECT_BACKOFF.duration(deficit - 1)
        } else {
            std::time::Duration::ZERO
        };
        if rate_paced {
            // The observability tick is part of the decision, not a
            // per-arm courtesy: it cannot be skipped by a new arm.
            metrics::counter!(
                "rio_gateway_build_resync_rate_paced_total",
                "cause" => cause.label()
            )
            .increment(1);
        }
        PacingDecision {
            attempt,
            exhausted,
            sleep: streak_sleep.max(rate_sleep),
            rate_paced,
        }
    }
}

/// Issue the `SubmitBuild` RPC and read its initial response metadata
/// (`x-rio-build-id`, `x-rio-trace-id`). Records `build_id` in
/// `active_build_ids` so a stream error before event 0 is
/// reconnectable. Emits the trace-id diagnostic line to the client.
///
/// On `SubmitBuild` failure, logs a `STDERR_NEXT` diagnostic (NOT
/// `STDERR_ERROR` — see comment) before returning the error so callers
/// that convert `Err` → `BuildResult::failure` produce the correct
/// `STDERR_LAST + BuildResult` sequence.
async fn submit_initial<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    request: tonic::Request<types::SubmitBuildRequest>,
    active_build_ids: &mut HashSet<String>,
) -> anyhow::Result<(String, tonic::codec::Streaming<types::BuildEvent>)> {
    // r[impl sched.grpc.fence-retryable]
    // Bounded pre-build_id retry on UNAVAILABLE only (the fence /
    // not-leader / actor-dead refusal class — sched.grpc.fence-
    // retryable maps every retryable scheduler refusal there). Safe
    // because no build_id metadata has been received: the scheduler
    // sets the header only AFTER MergeDag commits, and a refused/
    // fenced merge rolls back — re-submitting is idempotent. Any
    // other code, a timeout (DEADLINE_EXCEEDED — not provably
    // pre-merge), or an UNAVAILABLE that somehow carries the build-id
    // header propagates unchanged. Budget mirrors the deposed-believer
    // window: 4 retries at 0.5/1/2/4s under the per-attempt timeout.
    const SUBMIT_RETRIES: u32 = 4;
    const SUBMIT_RETRY_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
        base: std::time::Duration::from_millis(500),
        mult: 2.0,
        cap: std::time::Duration::from_secs(4),
        jitter: rio_common::backoff::Jitter::None,
    };
    let (meta, _ext, msg) = request.into_parts();
    let mut attempt: u32 = 0;
    let resp = loop {
        let req = tonic::Request::from_parts(meta.clone(), Default::default(), msg.clone());
        match rio_common::grpc::with_timeout_status(
            "SubmitBuild",
            rio_common::grpc::SUBMIT_BUILD_TIMEOUT,
            // no-jwt: the metadata (incl. the tenant token) was set by
            // the caller via with_jwt at the submit_and_process_build
            // entry point and is replayed verbatim on every attempt.
            scheduler_client.submit_build(req),
        )
        .await
        {
            Ok(r) => break r,
            Err(st)
                if st.code() == tonic::Code::Unavailable
                    && st.metadata().get(rio_proto::BUILD_ID_HEADER).is_none()
                    && attempt < SUBMIT_RETRIES =>
            {
                let delay = SUBMIT_RETRY_BACKOFF.duration(attempt);
                attempt += 1;
                tracing::debug!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %st,
                    "SubmitBuild refused as retryable (UNAVAILABLE, pre-build_id); retrying"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                // STDERR_NEXT diagnostic before the Err propagates. Callers
                // map this to BuildResult::failure (opcodes 36/46) or
                // stderr_err! (opcode 9) — the client SEES the failure
                // either way, but without this line the only context is
                // the tonic Status string with no indication it was the
                // INITIAL submit that failed (vs. a mid-stream event, vs.
                // reconnect exhaustion — all three produce anyhow Errs).
                // NOT STDERR_ERROR: two of three callers (opcodes 36/46)
                // convert Err → BuildResult::failure → STDERR_LAST.
                // Sending STDERR_ERROR here would produce the exact
                // ERROR→LAST desync remediation 07 fixes.
                let _ = stderr.log(&format!("SubmitBuild RPC failed: {e}\n")).await;
                return Err(GatewayError::Scheduler(format!("SubmitBuild failed: {e}")).into());
            }
        }
    };

    // build_id from initial response metadata. Scheduler sets this
    // AFTER MergeDag commits (grpc/mod.rs:~480) — if we have it, the
    // build IS durable and WatchBuild can resume it. Reconnect
    // protection is total: even zero stream events is recoverable.
    let header_build_id = resp
        .metadata()
        .get(rio_proto::BUILD_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // r[impl obs.trace.scheduler-id-in-metadata]
    // x-rio-trace-id: the SCHEDULER handler span's trace_id. Prefer this
    // over our own — the scheduler's #[instrument] span was created
    // before link_parent() ran, so it has its OWN trace_id (LINKED to
    // ours, not parented). That trace extends through worker via the
    // WorkAssignment.traceparent data-carry; ours has only gateway
    // spans. Read BEFORE into_inner() consumes metadata.
    let header_trace_id = resp
        .metadata()
        .get(rio_proto::TRACE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let event_stream = resp.into_inner();

    let build_id = match header_build_id {
        Some(id) => {
            // Track for cancel-on-disconnect + reconnect. The scheduler's
            // snapshot-first WatchBuild attach needs only the build_id —
            // there is no event cursor to maintain.
            active_build_ids.insert(id.clone());
            id
        }
        None => {
            // The scheduler ALWAYS sets this header AFTER MergeDag
            // commits. Absence is a scheduler bug, not a recoverable
            // condition — there is no build_id to WatchBuild against.
            return Err(GatewayError::Scheduler(
                "scheduler did not set x-rio-build-id header".into(),
            )
            .into());
        }
    };
    tracing::Span::current().record("build_id", &build_id);

    // Surface the build_id once per `nix build` via STDERR_NEXT so the
    // user can find their build in the dashboard or `rio-cli builds`.
    // The build_id is no longer load-bearing for `rio-cli logs` — logs
    // are keyed by `(drv_hash, exec_id)`, which the user already has
    // from the failure output and the worker's `rio: exec` log header —
    // but it's still the handle for build tracking, cancellation, and
    // dashboard links. With the header path this fires BEFORE event 0,
    // so the user gets the handle the moment the build is accepted.
    //
    // The trace_id is appended when OTel is wired — gives operators a
    // grep handle for Tempo when debugging a user's build. PRIORITIZE
    // the scheduler's trace_id (x-rio-trace-id header, read above) over
    // our own — the scheduler span is the one that actually spans the
    // full scheduler→worker chain (data-carry per
    // r[sched.trace.assignment-traceparent]). Our own trace only has
    // gateway spans. Fallback to our own for legacy schedulers that
    // don't set the header. Empty trace (no OTel tracer configured —
    // current_trace_id_hex returns "" for TraceId::INVALID, header
    // absent with no OTel on the scheduler side) drops the suffix; the
    // build_id line is still emitted unconditionally.
    let trace_id = header_trace_id.unwrap_or_else(rio_proto::interceptor::current_trace_id_hex);
    let trace_suffix = if trace_id.is_empty() {
        String::new()
    } else {
        format!(" (trace {trace_id})")
    };
    let _ = stderr
        .log(&format!("rio: build {build_id}{trace_suffix}\n"))
        .await;

    Ok((build_id, event_stream))
}

/// Submit a build to the scheduler and process events, returning a BuildResult.
#[instrument(
    skip_all,
    fields(tenant = %request.tenant_name, build_id = tracing::field::Empty)
)]
async fn submit_and_process_build<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    log_client: &rio_proto::LogServiceClient<Channel>,
    request: types::SubmitBuildRequest,
    active_build_ids: &mut HashSet<String>,
    session_jwt: crate::server::session_jwt::SessionTokenSource,
) -> anyhow::Result<BuildResult> {
    use crate::server::session_jwt::RemintCause;
    // Gateway is the trace ROOT (Nix doesn't speak W3C trace context).
    // with_jwt injects the enclosing span's context + tenant JWT — this
    // is THE hop that makes distributed tracing work; without it,
    // scheduler spans are orphaned root traces.
    //
    // r[impl gw.jwt.refresh-on-expiry+2]
    // live_064: the token is read from the session's refreshing SOURCE
    // at every outbound injection in this function (here, the
    // WatchBuild re-attach below, and the log-tail subscriptions) —
    // never snapshotted at entry. The previous Option<&str> parameter
    // froze the submit-time mint for the build's whole lifetime; a
    // 72-min build crossed the 65-min TTL and every re-attach replayed
    // the dead token into UNAUTHENTICATED while the session's own
    // accessor would have re-minted on its next read.
    let request = with_jwt(request, session_jwt.fresh().as_deref())?;

    let (build_id, event_stream) =
        submit_initial(stderr, scheduler_client, request, active_build_ids).await?;

    // Process remaining events, with reconnect on stream error.
    // Scheduler failover/restart drops the stream; we reconnect
    // via WatchBuild(build_id) with backoff (1s/2s/4s/8s/16s cap,
    // max 10). The scheduler's snapshot-first attach describes the
    // build's current state as the new stream's first message, so
    // the gateway needs no cursor into the old stream — reconnect is
    // connect → consume snapshot → continue.
    //
    // Without reconnect: scheduler restart mid-build → client's
    // `nix build` fails with MiscFailure even though the build
    // itself completes fine on a worker. With reconnect: client
    // doesn't notice the scheduler blip.
    //
    // 10 attempts = 1+2+4+8+16 + 5×16 = 111s total. 5 attempts
    // (=31s) was too tight: a force-killed leader's REPLACEMENT pod
    // needs ~20-30s (start + lease acquire on 5s
    // tick). Found by vm-le-build-k3s under the replacement-wins-race
    // path — standby-wins was fast enough to mask it.
    // (MAX_RECONNECT / RECONNECT_BACKOFF live at module scope so the
    // ReattachBudget methods can read them.)
    let mut budget = ReattachBudget::default();
    // Activity-ID state survives reconnects so a WatchBuild resume can
    // stop_activity derivations whose Started arrived on the prior
    // stream and keep attaching log lines / phase to the right aid.
    let mut act = BuildActivityState::default();
    // Live-tail subscriptions to rio-store, one per building
    // derivation. Like `act`, the set survives scheduler-stream
    // reconnects — the subscriptions are independent of the scheduler
    // connection (a scheduler failover must not restart the log tail
    // from line 0).
    // live_062: the tails get a refresh-per-open token SOURCE, not a
    // string snapshot — a watched build's tail outlives the session
    // TTL whenever the build does, and the snapshot was the 65-min
    // read-plane blackout. live_064 closed the same class for the
    // scheduler watch stream above: both faces now share this one
    // session source.
    let (mut tails, mut log_rx) = LogTailSet::new(log_client.clone(), session_jwt.clone());

    // The watch loop, typed by event source. `Live` consumes events;
    // any stream death or loss signal drops the stream (moving to
    // `NeedsReattach`) and only a successful WatchBuild whose first
    // message is the snapshot re-enters `Live`. The re-attach budget
    // resets only on organic events or evidenced recovery (`Live`
    // tenure at the next death) — see `ReattachBudget`.
    budget.note_live_entered();
    let mut source = EventSource::Live(Box::new(event_stream));
    // The most recent stream error. live_064 demoted it from the
    // exhaustion message to an operator-log field: the user-facing
    // exhaustion cause is the LAST RE-ATTACH failure (what actually
    // kept the watch down), while the stream-death classification —
    // benign EOF vs transport vs resync — stays in the warn beside it.
    let mut last_stream_err = StreamProcessError::EofWithoutTerminal;
    // live_064: the most recent re-attach failure, verbatim — the
    // evidence the exhaustion message carries.
    let mut last_reattach_err: Option<String> = None;
    // live_064 + r[gw.jwt.remint-local-expiry-only]: the one-shot
    // LocalExpiry re-mint. Armed (spent) at the first re-attach
    // rejection the gateway can LOCALLY verify as expiry; any other
    // Unauthenticated (NotLocallyHealable) fails fast without spending
    // it. Reset on every successful re-attach so a later genuine
    // expiry gets its own one-shot.
    let mut unauth_remint_spent = false;
    let outcome = loop {
        match source {
            EventSource::Live(ref mut event_stream) => {
                match process_build_events(
                    stderr,
                    event_stream,
                    &mut budget,
                    &mut act,
                    &mut tails,
                    &mut log_rx,
                )
                .await
                {
                    Ok(outcome) => break Ok(outcome),
                    // Wire error: NOT reconnect-worthy. Client disconnected
                    // (SSH closed) — scheduler is fine, there's no one to
                    // send the result to. Surface immediately.
                    Err(e @ StreamProcessError::Wire(_)) => {
                        break Err(e);
                    }
                    // The re-attach verdicts are minted only by the
                    // NeedsReattach arm below — process_build_events
                    // never constructs them. Named here (not a
                    // wildcard) so the alphabet stays total; surfacing
                    // unchanged is correct if a future refactor ever
                    // routes one through this seam.
                    Err(
                        e @ (StreamProcessError::ReattachAuthRejected
                        | StreamProcessError::ReattachExhausted { .. }),
                    ) => {
                        break Err(e);
                    }
                    // Transport OR EofWithoutTerminal: both are failover
                    // signatures. Transport = RST / tonic connection error.
                    // EofWithoutTerminal = scheduler cleanly closed the
                    // stream mid-build — THIS is what k8s pod kill looks
                    // like (SIGTERM → graceful shutdown → TCP FIN →
                    // Ok(None), not Err). vm-le-build-k3s proved the
                    // prior "crash = Transport" assumption wrong.
                    // ResyncRequired = server-signalled watcher lag.
                    //
                    // All three kill the stream HERE: `source` moves to
                    // `NeedsReattach`, so the dead stream can never be
                    // polled again (pre-fix, a failed WatchBuild left it
                    // in place and the next iteration consumed its
                    // buffered post-gap events as if live).
                    Err(
                        e @ (StreamProcessError::Transport(_)
                        | StreamProcessError::EofWithoutTerminal
                        | StreamProcessError::ResyncRequired),
                    ) => {
                        let resync = matches!(e, StreamProcessError::ResyncRequired);
                        // merged_bug_083: the arm names its CAUSE and
                        // consumes the chokepoint's effects record —
                        // it has no axis API to pick from.
                        let cause = if resync {
                            BackoffCause::ResyncSignal
                        } else if matches!(e, StreamProcessError::Transport(_)) {
                            BackoffCause::Transport
                        } else {
                            BackoffCause::EofWithoutTerminal
                        };
                        let decision = budget.next_backoff(cause);
                        if decision.exhausted {
                            break Err(e);
                        }
                        if resync {
                            // r[impl gw.resync.loss-signal+1]
                            // Server-signalled watcher lag: the scheduler is
                            // healthy, so re-attach silently — the snapshot
                            // reconcile is invisible to the nix client —
                            // with zero backoff for the first
                            // ZERO_BACKOFF_STREAK consecutive non-organic
                            // cycles and the standard ladder past that.
                            // The budget (reset only on organic events)
                            // bounds the storm: snapshot + resync cycling
                            // forever now burns through MAX_RECONNECT
                            // instead of refreshing it.
                            if decision.sleep.is_zero() {
                                tracing::debug!(
                                    %build_id,
                                    attempt = decision.attempt,
                                    "scheduler signalled event loss (broadcast lag); resyncing via WatchBuild snapshot"
                                );
                            } else if decision.rate_paced {
                                // bug_160: the interleaved storm —
                                // organic progress kept the streak at
                                // zero while the cycle RATE charged
                                // the scheduler one O(DAG) snapshot
                                // per loop. Operator signal: this is
                                // a slow CONSUMER (or an undersized
                                // broadcast buffer), not a scheduler
                                // outage. (The counter tick fired
                                // inside next_backoff.)
                                tracing::warn!(
                                    %build_id,
                                    attempt = decision.attempt,
                                    backoff_secs = decision.sleep.as_secs(),
                                    "resync cycle rate exceeded the window bound despite organic progress; pacing re-attach"
                                );
                                tokio::time::sleep(decision.sleep).await;
                            } else {
                                tracing::warn!(
                                    %build_id,
                                    attempt = decision.attempt,
                                    backoff_secs = decision.sleep.as_secs(),
                                    "scheduler resync-signal streak with no organic events; pacing re-attach"
                                );
                                tokio::time::sleep(decision.sleep).await;
                            }
                        } else {
                            tracing::warn!(
                                %build_id,
                                error = %e,
                                attempt = decision.attempt,
                                backoff_secs = decision.sleep.as_secs(),
                                rate_paced = decision.rate_paced,
                                "BuildEvent stream error; reconnecting via WatchBuild"
                            );
                            // Also surface to the client via STDERR — they see
                            // "reconnecting..." instead of a hang.
                            let _ = stderr
                                .log(&format!(
                                    "scheduler connection lost (attempt {}/{}); reconnecting...",
                                    decision.attempt, MAX_RECONNECT
                                ))
                                .await;
                            tokio::time::sleep(decision.sleep).await;
                        }
                        last_stream_err = e;
                        source = EventSource::NeedsReattach;
                    }
                }
            }
            EventSource::NeedsReattach => {
                // Reconnect: need a fresh scheduler client. The
                // original was moved into this function; we can't
                // easily get the address here. Clone the existing
                // client — tonic clients ARE cheap to clone (Arc
                // internally), and the underlying channel may have
                // auto-reconnected. If that fails (channel dead),
                // WatchBuild will Err and we retry.
                // r[impl gw.jwt.propagate]
                // r[impl gw.jwt.refresh-on-expiry+2]
                // Reconnect goes through with_jwt like the initial submit
                // — otherwise the resumed stream's scheduler span is an
                // orphan root trace AND carries no x-rio-tenant-token
                // (hard auth failure once scheduler-side WatchBuild authz
                // lands → every failover burns through MAX_RECONNECT).
                // live_064: the token is read from the refreshing source
                // AT THIS INJECTION — a re-attach hours after submit
                // carries a token minted for THIS attempt, never the
                // submit-time snapshot (the 11-identical-UNAUTHENTICATED
                // incident shape).
                let watch_req = with_jwt(
                    types::WatchBuildRequest {
                        build_id: build_id.clone(),
                    },
                    session_jwt.fresh().as_deref(),
                )?;
                // Bounded open (streaming-open-ban): a wedged
                // scheduler must surface as a retryable Err, not park
                // the reconnect loop forever past MAX_RECONNECT.
                let opened = rio_common::grpc::with_timeout_status(
                    "WatchBuild",
                    rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                    scheduler_client.watch_build(watch_req),
                )
                .await;
                // r[impl gw.resync.snapshot-owed]
                // The first message of a re-attached stream MUST be the
                // snapshot (sched.watch.snapshot-first) — `Live` is only
                // constructed after it has been consumed and reconciled,
                // so a half-open stream can never serve organic events
                // ahead of the reconcile. The read is bounded like the
                // open: an accepted-but-silent WatchBuild charges the
                // budget instead of parking forever.
                let reattached = match opened {
                    Ok(resp) => {
                        let mut stream = resp.into_inner();
                        match tokio::time::timeout(
                            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                            stream.message(),
                        )
                        .await
                        {
                            Ok(Ok(Some(types::BuildEvent {
                                event: Some(types::build_event::Event::Snapshot(snap)),
                                ..
                            }))) => {
                                // A snapshot is connection machinery, not an
                                // organic event: the budget is NOT reset here.
                                // It resets on the first organic event the new
                                // stream yields (inside process_build_events)
                                // or, retroactively, by this stream's Live
                                // tenure when it next dies (`note_reattach` —
                                // evidenced recovery, bug_068).
                                match apply_snapshot(stderr, &mut act, &mut tails, snap).await? {
                                    Some(outcome) => break Ok(outcome),
                                    None => {
                                        tracing::info!(%build_id, "reconnected via WatchBuild (snapshot reconciled)");
                                        Some(stream)
                                    }
                                }
                            }
                            Ok(Ok(Some(other))) => {
                                tracing::warn!(
                                    %build_id,
                                    event = ?other.event.as_ref().map(std::mem::discriminant),
                                    "WatchBuild first message was not the snapshot; discarding stream"
                                );
                                last_reattach_err =
                                    Some("WatchBuild first message was not the snapshot".into());
                                None
                            }
                            Ok(Ok(None)) => {
                                tracing::warn!(%build_id, "WatchBuild stream closed before the snapshot");
                                last_reattach_err =
                                    Some("WatchBuild stream closed before the snapshot".into());
                                None
                            }
                            Ok(Err(status)) => {
                                tracing::warn!(%build_id, error = %status, "WatchBuild stream errored before the snapshot");
                                last_reattach_err = Some(format!(
                                    "WatchBuild stream errored before the snapshot ({})",
                                    status.code()
                                ));
                                None
                            }
                            Err(_elapsed) => {
                                tracing::warn!(%build_id, "WatchBuild accepted but served no snapshot in time");
                                last_reattach_err = Some(
                                    "WatchBuild accepted but served no snapshot in time".into(),
                                );
                                None
                            }
                        }
                    }
                    Err(wb_err) => {
                        // live_064 + merged_bug_005 (R34-w(iii)): an
                        // UNAUTHENTICATED open means the CARRIED TOKEN
                        // was rejected. The gateway is the issuer, so
                        // the only cause it can locally VERIFY as
                        // heal-able is its own clock against the
                        // token's own exp — a re-mint then produces a
                        // token the verifier accepts. Any other
                        // Unauthenticated (revoked jti, unknown key,
                        // malformed) is not locally heal-able: a
                        // re-mint would silently override an operator
                        // denial or burn the one-shot on a token signed
                        // with the same key the verifier just refused.
                        // Surface those honestly with the auth
                        // evidence; never re-mint.
                        if wb_err.code() == tonic::Code::Unauthenticated {
                            match session_jwt.note_rejected() {
                                RemintCause::LocalExpiry if !unauth_remint_spent => {
                                    unauth_remint_spent = true;
                                    tracing::warn!(
                                        %build_id, error = %wb_err,
                                        "WatchBuild re-attach rejected UNAUTHENTICATED; the carried token is past local expiry, re-minting and retrying immediately"
                                    );
                                    continue;
                                }
                                RemintCause::LocalExpiry => {
                                    tracing::warn!(
                                        %build_id, error = %wb_err,
                                        "WatchBuild re-attach rejected UNAUTHENTICATED after a local-expiry re-mint; failing the watch with the auth evidence"
                                    );
                                }
                                RemintCause::NotLocallyHealable => {
                                    tracing::warn!(
                                        %build_id, error = %wb_err,
                                        "WatchBuild re-attach rejected UNAUTHENTICATED with the carried token well within its TTL (revoked jti or unknown verify key); not re-minting, failing the watch with the auth evidence"
                                    );
                                }
                            }
                            break Err(StreamProcessError::ReattachAuthRejected);
                        }
                        // Other codes: scheduler still down (transient
                        // — next cycle retries), OR build not found
                        // (recovery didn't reconstruct it — terminal).
                        // Treat both as retryable and let MAX_RECONNECT
                        // cap it; the failure is recorded as the
                        // exhaustion evidence.
                        tracing::warn!(%build_id, error = %wb_err,
                                      "WatchBuild reconnect attempt failed");
                        last_reattach_err =
                            Some(format!("WatchBuild open failed ({})", wb_err.code()));
                        None
                    }
                };
                match reattached {
                    Some(stream) => {
                        budget.note_live_entered();
                        // A successful re-attach closes the auth
                        // episode: a later local-expiry rejection gets
                        // its own one-shot re-mint.
                        unauth_remint_spent = false;
                        source = EventSource::Live(Box::new(stream));
                    }
                    None => {
                        // The cycle failed before any stream existed:
                        // charge the budget and pace the next attempt.
                        // `source` stays NeedsReattach — the dead
                        // stream from before this cycle is long gone
                        // and CANNOT be re-entered (no Live to fall
                        // back to).
                        let decision = budget.next_backoff(BackoffCause::ReattachCycleFailed);
                        if decision.exhausted {
                            // live_064: the surfaced cause is the last
                            // RE-ATTACH failure (the auth evidence in
                            // the incident); the stream-death
                            // classification stays in the operator log
                            // here, not in the user-facing message.
                            tracing::warn!(
                                %build_id,
                                last_stream_error = %last_stream_err,
                                "WatchBuild re-attach budget exhausted"
                            );
                            break Err(StreamProcessError::ReattachExhausted {
                                attempts: decision.attempt,
                                last_reattach: last_reattach_err.take().unwrap_or_else(|| {
                                    "no re-attach attempt completed".to_string()
                                }),
                            });
                        }
                        tracing::debug!(
                            %build_id,
                            attempt = decision.attempt,
                            backoff_secs = decision.sleep.as_secs(),
                            rate_paced = decision.rate_paced,
                            "re-attach cycle failed; backing off"
                        );
                        tokio::time::sleep(decision.sleep).await;
                    }
                }
            }
        }
    };

    // P0331 trace (Design A — bug confirmed, fix in T2):
    //
    // Unconditional removal here defeats CancelBuild-on-disconnect for
    // mid-opcode client drops. The trace:
    //
    //   1. Client disconnects mid-build → response-task's write-half send
    //      fails → outbound pipe reader drops → next stderr.log write
    //      in process_build_events gets BrokenPipe → WireError
    //   2. :372 breaks with outcome = Err(StreamProcessError::Wire(_))
    //   3. THIS LINE removes build_id — map now empty
    //   4. :474 converts Err → Ok(BuildResult::failure), caller at :881
    //      gets Ok, proceeds to :940 stderr.finish() → BrokenPipe again
    //   5. handle_build_paths_with_results returns Err
    //   6. session.rs:147 `handle_opcode(...)?` — the ? exits run_protocol
    //      directly; the :107 EOF-cancel arm is NEVER reached (that arm
    //      only catches opcode-READ errors at :90, not handler-execution
    //      errors). :138 generic-Err is also opcode-READ-only.
    //   7. server.rs channel_eof/channel_close do NOT run CancelBuild —
    //      channel_close → ChannelSession::Drop → proto_task.abort(),
    //      no cancel logic anywhere.
    //
    // Result: build leaks until the orphan-watcher auto-cancel
    // (r[sched.backstop.orphan-watcher], 5 min with no attached
    // watcher). Before that backstop the leak ran for the whole build.
    //
    // Fix is two-part (both needed — step 3 and step 6 compound):
    //   - Guard this remove on !Wire error (keep build_id in map)
    //   - session.rs: run cancel loop on handler-Err too, not just EOF
    //
    // Transport/EofWithoutTerminal errors still remove: scheduler is
    // down, client is alive, cancel would have nowhere to go anyway.
    // r[impl gw.conn.cancel-on-disconnect+3]
    if !matches!(outcome, Err(StreamProcessError::Wire(_))) {
        active_build_ids.remove(&build_id);
    }

    // Build terminus for the live tail (merged_bug_111 ordering):
    // hard-cancel the subscription tasks FIRST, bound-join so every
    // aborted task's PendingGapCell Drop-disclosure (gap marker +
    // withheld lines) lands in the channel, THEN relay everything
    // that reached the gateway. The pre-fix order drained before
    // aborting, so Drop-flushed chunks were stranded in a channel
    // nobody would read again. The join is bounded: an aborted task
    // resolves at its next yield point (typically <1 ms); the bound
    // only caps a pathological scheduler stall, and lines that still
    // miss the window remain durable and readable via `rio-cli logs`.
    // The relay is skipped on a Wire error for the same reason as the
    // activity drain below: the client is gone (the disclosure then
    // dies with the dropped channel — the law's vacuous case).
    let tail_handles = tails.abort_all();
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        futures_util::future::join_all(tail_handles),
    )
    .await;
    if !matches!(outcome, Err(StreamProcessError::Wire(_))) {
        while let Ok(chunk) = log_rx.try_recv() {
            let _ = relay_log_batch(stderr, &act, chunk.into_batch()).await;
        }
    }

    // Best-effort terminal drain: a Wire error means the client is
    // gone (write would BrokenPipe), and the writer may already be
    // poisoned. nom tolerates unstopped activities (closes on EOF),
    // but stop-parity makes the live display correct.
    if !matches!(outcome, Err(StreamProcessError::Wire(_))) {
        let _ = drain_unstopped_activities(stderr, &mut act).await;
    }

    // Close the top-level actBuilds activity. Best-effort: a Wire
    // error means the client is gone (write would BrokenPipe), and
    // the writer may already be poisoned anyway. nom tolerates an
    // unclosed actBuilds (it closes on EOF).
    if let (Some(aid), false) = (
        act.builds_root,
        matches!(outcome, Err(StreamProcessError::Wire(_))),
    ) {
        let _ = stderr.stop_activity(aid).await;
    }

    match outcome {
        Ok(BuildEventOutcome::Completed) => Ok(BuildResult::success()),
        Ok(BuildEventOutcome::Failed {
            status,
            error_message,
        }) => Ok(BuildResult::failure(status.into(), error_message)),
        // THE sole build-scoped cancellation mint (live_051(f1) —
        // KD-scope): "build cancelled" is FINAL vocabulary and exists
        // only at this fold; the attempt/cycle-scoped lane renders
        // through TerminalScope::AttemptCancelled and can never reach
        // this phrase.
        Ok(BuildEventOutcome::Cancelled { reason }) => Ok(BuildResult::failure(
            BuildStatus::TransientFailure,
            format!("build cancelled: {reason}"),
        )),
        Err(e) => Ok(BuildResult::failure(
            BuildStatus::TransientFailure,
            format!("build stream error (reconnect exhausted): {e}"),
        )),
    }
}

// r[impl gw.opcode.build-derivation+2]
// r[impl gw.hook.single-node-dag]
// r[impl gw.hook.ifd-detection+2]
/// wopBuildDerivation (36): Build a derivation via scheduler.
///
/// Receives an inline BasicDerivation (no inputDrvs). Recovers the full
/// Derivation from drv_cache to reconstruct the DAG.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_build_derivation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let negotiated_version = ctx.negotiated_version;
    let SessionContext {
        store_client,
        log_client,
        scheduler_client,
        drv_cache,
        has_seen_build_paths_with_results,
        active_build_ids,
        tenant_name,
        jwt,
        limiter,
        quota_cache,
        ..
    } = ctx;
    let (drv_path_str, drv_path) = match super::read_store_path(reader).await {
        Ok(v) => v,
        Err(e) => stderr_err!(stderr, "wopBuildDerivation: {e}"),
    };
    tracing::Span::current().record("path", drv_path_str.as_str());
    let Ok(basic_drv) = read_basic_derivation(reader).await else {
        stderr_err!(stderr, "wopBuildDerivation: failed to read BasicDerivation");
    };
    read_build_mode_normal_only(reader, stderr, "wopBuildDerivation").await?;

    debug!(
        path = %drv_path_str,
        platform = %basic_drv.platform(),
        builder = %basic_drv.builder(),
        "wopBuildDerivation"
    );

    let is_ifd_hint = !*has_seen_build_paths_with_results;

    // r[impl gw.reject.nochroot]
    // Check __noChroot on the BasicDerivation DIRECTLY. validate_dag
    // (called below) checks drv_cache entries, but if the full drv
    // isn't available (single-node fallback below), the drv is never
    // in the cache → __noChroot check is skipped. The
    // BasicDerivation wire format DOES include env; we have it here.
    //
    // A malicious client could send __noChroot=1 via wopBuildDerivation
    // (which sends an inline BasicDerivation, not a store path) to
    // escape the sandbox. This catches it at the gateway.
    if translate::StructuredEnv::new(basic_drv.env()).bool("__noChroot") == Some(true) {
        warn!(drv_path = %drv_path_str, "rejecting __noChroot via inline BasicDerivation");
        stderr_err!(
            stderr,
            "derivation requests __noChroot (sandbox escape) — not permitted"
        );
    }

    // Recover full Derivation from drv_cache (BasicDerivation has no inputDrvs).
    // The .drv should have been uploaded via wopAddToStoreNar before this call.
    let full_drv = resolve_derivation(&drv_path, store_client, drv_cache).await;

    let (nodes, edges) = match &full_drv {
        Ok(drv) => {
            // wopBuildDerivation carries no output selection on the wire
            // — `None` keeps the root's wanted_output_names empty
            // (= every declared output wanted).
            match translate::reconstruct_dag(&drv_path, drv, None, store_client, drv_cache).await {
                Ok((n, e)) => (n, e),
                Err(dag_err) => {
                    // Degrading to a 1-node DAG here is wrong: an
                    // input-addressed root with no inputs would dispatch
                    // and fail on the builder with "input not found".
                    // The errors here (transitive-input cap exceeded,
                    // child .drv resolve failure mid-BFS) are
                    // user-actionable — surface them.
                    warn!(error = %dag_err, "DAG reconstruction failed");
                    stderr_err!(stderr, "cannot build '{drv_path_str}': {dag_err}");
                }
            }
        }
        Err(e) => {
            debug!(error = %e, "full derivation not available, using single-node DAG");
            // Single-node fallback skips reconstruct_dag (which is
            // where the BFS root gets flagged), so mark the requested
            // target here. Behaviour-neutral for a 1-node submission
            // (it is trivially a structural root) but keeps the
            // "client named it" marker consistent across opcodes.
            let mut node = translate::build_node(&drv_path_str, &basic_drv);
            node.explicitly_requested = true;
            (vec![node], Vec::new())
        }
    };

    let priority_class = if is_ifd_hint { "interactive" } else { "ci" };

    // Validate BEFORE inlining (no point doing FindMissingPaths +
    // inline for a DAG we're about to reject). __noChroot check +
    // early MAX_DAG_NODES.
    if let Err(reason) = translate::validate_dag(&nodes, drv_cache) {
        warn!(reason = %reason, "rejecting build: DAG validation failed");
        // Do NOT send STDERR_ERROR here — it is a terminal frame.
        // The client receives the rejection via BuildResult.errorMsg
        // after STDERR_LAST. See build.rs:160-164 for the inverse
        // invariant (STDERR_ERROR → STDERR_LAST is equally invalid).
        let failure = BuildResult::failure(BuildStatus::InputRejected, reason);
        stderr.finish().await?;
        write_build_result(stderr.inner_mut(), &failure, negotiated_version).await?;
        return Ok(());
    }

    // Inline .drv content for will-dispatch nodes. Mutable because
    // this fills node.drv_content in-place. On store error: skips
    // silently (safe degrade; worker fetches).
    let mut nodes = nodes;
    translate::filter_and_inline_drv(&mut nodes, drv_cache, store_client).await;

    // Rate limit + quota BEFORE SubmitBuild. Checked after wire reads
    // + validation (those are cheap; the expensive part is the
    // scheduler RPC + stream). A rate-limited / over-quota client
    // gets STDERR_ERROR; the connection stays open.
    if rate_limit_check(stderr, limiter, tenant_name.as_ref()).await? {
        return Ok(());
    }
    if quota_check(
        stderr,
        quota_cache,
        store_client,
        tenant_name.as_ref(),
        jwt.token(),
    )
    .await?
    {
        return Ok(());
    }

    let request =
        translate::build_submit_request(nodes, edges, priority_class, tenant_name.as_ref());

    let mut build_result = match submit_and_process_build(
        stderr,
        scheduler_client,
        log_client,
        request,
        active_build_ids,
        jwt.token_source(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "build submission failed");
            BuildResult::failure(
                BuildStatus::TransientFailure,
                format!("scheduler error: {e}"),
            )
        }
    };

    // Verify the declared outputs against the store before reporting
    // success (same machinery as opcodes 9/46): builtOutputs come from
    // result_with_wanted_outputs — declared paths for IA/fixed-CA outputs,
    // registered realisations for floating-CA — and a missing or unrealized
    // output demotes the result to an honest failure instead of shipping an
    // empty outPath the client asserts on (nix-build.cc:722). Needs the full
    // Derivation (with inputDrvs) for hash_derivation_modulo; the inline
    // BasicDerivation lacks inputDrvs so the modular hash would diverge from
    // CppNix for non-leaf drvs. If full_drv resolve failed (single-node
    // fallback above), there is nothing to verify against — leave
    // builtOutputs empty, no worse than before, and IA output paths are
    // still recoverable client-side from the BasicDerivation it sent.
    // Store errors during verification abort via stderr_err! inside
    // check_targets_against_store, before stderr.finish().
    // r[impl gw.opcode.build-results-honest+2]
    if build_result.status.is_success()
        && let Ok(drv) = &full_drv
    {
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        // wopBuildDerivation has no per-target output selection — every
        // declared output is wanted.
        let targets = HashMap::from([(
            0usize,
            TargetDemand {
                drv_path: drv_path_str.clone(),
                drv: drv.clone(),
                spec: OutputSpec::All,
            },
        )]);
        let checks = check_targets_against_store(stderr, ctx, &targets, &mut hash_cache).await?;
        if let Some(check) = checks.get(&0) {
            if check.missing.is_empty() {
                build_result = result_with_wanted_outputs(
                    build_result,
                    &targets[&0],
                    check,
                    &ctx.drv_cache,
                    &mut hash_cache,
                );
            } else {
                // Wrong-success: the scheduler says Built but the store does
                // not hold what the client is owed.
                warn!(
                    drv = %drv_path_str,
                    missing = ?check.missing,
                    "demoting successful wopBuildDerivation result: outputs not in store"
                );
                build_result = BuildResult::failure(
                    BuildStatus::MiscFailure,
                    format!(
                        "build completed but requested outputs are not in the store: {}",
                        check.missing.join("; ")
                    ),
                );
            }
        }
    }

    debug!(
        status = ?build_result.status,
        error_msg = %build_result.error_msg,
        "wopBuildDerivation result"
    );

    stderr.finish().await?;
    write_build_result(stderr.inner_mut(), &build_result, negotiated_version).await?;
    Ok(())
}

/// One requested `DerivedPath::Built` target of a `wopBuildPaths` /
/// `wopBuildPathsWithResults` batch: the resolved `.drv` plus the output
/// selection the client sent for it (`!out,dev` / `!*`). Carried from
/// request parsing to result reporting so the per-target store verification
/// knows exactly which outputs the client is owed.
struct TargetDemand {
    drv_path: String,
    drv: Derivation,
    spec: OutputSpec,
}

/// Store-verified view of one target's wanted outputs, produced by
/// [`check_targets_against_store`].
struct TargetOutputsCheck {
    /// Wanted outputs the store confirms are NOT available: missing store
    /// paths, or floating-CA outputs with no realisation. Human-readable,
    /// used verbatim in the client-facing error message.
    missing: Vec<String>,
    /// Positive evidence that EVERY wanted output resolved to a concrete
    /// store path the store reports as present. Required to report a target
    /// successful when the aggregate outcome was a failure — absence of
    /// evidence (an unverifiable path) is not enough to override it.
    confirmed_present: bool,
    /// Output name → realized store path for floating-CA outputs (from the
    /// Realisations table). Reused to build `builtOutputs` without a second
    /// round of store queries.
    realized: HashMap<String, String>,
    /// The wanted output names this target's spec resolves to
    /// (`Names` as sent, `All` → every declared output name).
    wanted_names: Vec<String>,
    /// `(output name, expected store path)` pairs awaiting the batched
    /// FindMissingPaths answer. Drained when the verdict is finalized.
    checkable: Vec<(String, String)>,
    /// At least one wanted output could not be mapped to a queryable store
    /// path (a name the `.drv` does not declare, or a declared path that is
    /// not a parseable store path — the store rejects the whole
    /// FindMissingPaths batch on such a path, and it can never have been
    /// ingested either). Unverifiable outputs never count as missing (the
    /// target defers to the scheduler outcome, exactly the pre-verification
    /// behavior) but they block `confirmed_present`.
    unverifiable: bool,
}

// r[impl gw.opcode.build-results-honest+2]
/// Resolve every target's wanted outputs to concrete store paths and ask the
/// store — ONE batched `FindMissingPaths` over the union, tenant-scoped via
/// the session JWT like `wopQueryValidPaths` — which of them actually exist.
///
/// Expected paths: the declared `.drv` path for input-addressed / fixed-CA
/// outputs, the Realisations row for floating-CA outputs. A wanted
/// floating-CA output with NO realisation is recorded as missing
/// immediately — the alternative (today's behavior before this check) is a
/// "successful" entry whose `builtOutputs` carry an empty `outPath`, which
/// stock clients reject (`Realisation JSON has empty 'outPath'` /
/// nix-build.cc:722 assert).
///
/// Store errors (realisation lookup or FindMissingPaths failure/timeout)
/// abort the whole opcode via `stderr_err!` — the gateway never falls back
/// to trusting the scheduler's word about what is in the store. Callers run
/// this BEFORE `stderr.finish()` (r[gw.stderr.error-before-return+2]).
async fn check_targets_against_store<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
    targets: &HashMap<usize, TargetDemand>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> anyhow::Result<HashMap<usize, TargetOutputsCheck>> {
    let SessionContext {
        store_client,
        drv_cache,
        jwt,
        ..
    } = ctx;

    let mut checks: HashMap<usize, TargetOutputsCheck> = HashMap::new();
    // Deduplicated union of every target's checkable paths — one store
    // round-trip for the whole batch. BTreeSet for a deterministic request.
    let mut to_query: BTreeSet<String> = BTreeSet::new();

    // Deterministic per-target order so log/error output is stable.
    let mut indices: Vec<usize> = targets.keys().copied().collect();
    indices.sort_unstable();

    for idx in indices {
        let demand = &targets[&idx];
        // Floating-CA outputs: declared path is "" and the real path lives
        // in the Realisations table (the scheduler wrote it before emitting
        // Completed). Non-NotFound store errors abort the opcode.
        let (_, realized) = match resolve_floating_outputs(
            &demand.drv,
            &demand.drv_path,
            store_client,
            jwt.token(),
            drv_cache,
            hash_cache,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => stderr_err!(
                stderr,
                "store error querying realisation for {}: {e}",
                demand.drv_path
            ),
        };

        let wanted_names: Vec<String> = match &demand.spec {
            OutputSpec::All => demand
                .drv
                .outputs()
                .iter()
                .map(|o| o.name().to_string())
                .collect(),
            OutputSpec::Names(names) => names.clone(),
        };

        let mut missing = Vec::new();
        let mut checkable = Vec::new();
        let mut unverifiable = false;
        for name in &wanted_names {
            let Some(output) = demand
                .drv
                .outputs()
                .iter()
                .find(|o| o.name() == name.as_str())
            else {
                // Requested output name the .drv does not declare. Nothing
                // to ask the store about — defer to the scheduler outcome.
                unverifiable = true;
                continue;
            };
            let expected = if output.path().is_empty() {
                match realized.get(name) {
                    Some(p) => p.clone(),
                    None => {
                        missing.push(format!(
                            "floating-CA output '{name}' of {} has no realisation",
                            demand.drv_path
                        ));
                        continue;
                    }
                }
            } else {
                output.path().to_string()
            };
            if StorePath::parse(&expected).is_err() {
                unverifiable = true;
                continue;
            }
            to_query.insert(expected.clone());
            checkable.push((name.clone(), expected));
        }

        checks.insert(
            idx,
            TargetOutputsCheck {
                missing,
                confirmed_present: false,
                realized,
                wanted_names,
                checkable,
                unverifiable,
            },
        );
    }

    let missing_set: HashSet<String> = if to_query.is_empty() {
        HashSet::new()
    } else {
        // Tenant-scoped like wopQueryValidPaths: dropping the JWT would
        // bypass the cross-tenant visibility gate and could "verify" a
        // path the requesting tenant cannot actually fetch.
        let req = with_jwt(
            types::FindMissingPathsRequest {
                store_paths: to_query.into_iter().collect(),
            },
            jwt.token(),
        )?;
        match rio_common::grpc::with_timeout(
            "FindMissingPaths",
            super::opcodes_read::GATEWAY_FMP_TIMEOUT,
            store_client.find_missing_paths(req),
        )
        .await
        {
            Ok(r) => r.into_inner().missing_paths.into_iter().collect(),
            Err(e) => stderr_err!(stderr, "store error verifying build outputs: {e}"),
        }
    };

    for check in checks.values_mut() {
        for (name, path) in std::mem::take(&mut check.checkable) {
            if missing_set.contains(&path) {
                check.missing.push(format!(
                    "output '{name}' ({path}) is not valid in the store"
                ));
            }
        }
        // Promotion over a failed aggregate needs positive evidence for
        // every wanted output; an empty wanted set or any unverifiable
        // output leaves the scheduler outcome authoritative.
        check.confirmed_present =
            check.missing.is_empty() && !check.unverifiable && !check.wanted_names.is_empty();
    }

    Ok(checks)
}

// r[impl gw.opcode.build-results-honest+2]
/// Build the success-side `BuildResult` for ONE verified target: status and
/// timing from `base`, `builtOutputs` covering exactly the wanted outputs
/// (drvHashModulo ids; floating-CA paths from the realisations resolved
/// during verification — no further store I/O). If the modular hash is
/// uncomputable (already `warn!`-logged) the result is returned without
/// builtOutputs — the same degrade the pre-verification enrichment had.
fn result_with_wanted_outputs(
    base: BuildResult,
    demand: &TargetDemand,
    check: &TargetOutputsCheck,
    drv_cache: &HashMap<StorePath, Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> BuildResult {
    let Some(hash) = translate::compute_modular_hash_cached(
        &demand.drv,
        &demand.drv_path,
        drv_cache,
        hash_cache,
    ) else {
        return base;
    };
    let hash_hex = hex::encode(hash);
    let mut result = base.with_outputs_from_drv(&demand.drv, &hash_hex, &check.realized);
    // builtOutputs honesty: cover exactly the outputs the client asked for.
    // Membership is decided on the DrvOutput ids ("sha256:<hash>!<name>")
    // rather than re-parsing names back out of them.
    let wanted_ids: HashSet<String> = check
        .wanted_names
        .iter()
        .map(|n| format!("sha256:{hash_hex}!{n}"))
        .collect();
    result
        .built_outputs
        .retain(|o| wanted_ids.contains(&o.drv_output_id));
    result
}

/// Dedup DAG nodes by `drv_path` and edges by `(parent, child)`.
///
/// Multi-root builds with shared deps walk the shared subgraph once per
/// root, producing duplicate nodes/edges. The scheduler tolerates dups
/// (MergeDag is idempotent; `derivation_edges` PK is `(parent,child)` so
/// dups are `ON CONFLICT DO NOTHING`) but they waste bytes + PG RTTs.
///
/// Duplicates are identical EXCEPT for the demand fields —
/// `wanted_output_names` and `explicitly_requested` are computed from
/// the *request target / consumers reachable in one root's BFS* rather
/// than from the `.drv` itself, so each root's copy carries only that
/// root's demand. Retain-first must therefore UNION the dropped
/// duplicate's wanted set into the retained node: dropping it un-wants
/// whatever the other root asked for (root A wants `child^out`,
/// root B wants `child^dev` — keeping only A's copy would let the
/// scheduler treat a missing `dev` output as ignorable). The union is
/// [`rio_common::wanted_outputs::union_wanted_saturating`] — the same
/// saturating union (empty = "all declared outputs wanted", all ∪ X =
/// all) that `DerivationState::union_wanted` and the PG upsert's
/// union-on-conflict apply when the duplicates arrive as separate
/// submissions instead. `explicitly_requested` is ORed for the same
/// reason: a target requested in ANY copy stays a requested target
/// (this is exactly the copy that records "the client also named this
/// dependency as a target of its own").
fn dedup_dag(nodes: &mut Vec<types::DerivationNode>, edges: &mut Vec<types::DerivationEdge>) {
    let mut keep: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<types::DerivationNode> = Vec::with_capacity(nodes.len());
    for node in nodes.drain(..) {
        match keep.entry(node.drv_path.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(deduped.len());
                deduped.push(node);
            }
            std::collections::hash_map::Entry::Occupied(e) => {
                let kept = &mut deduped[*e.get()];
                rio_common::wanted_outputs::union_wanted_saturating(
                    &mut kept.wanted_output_names,
                    &node.wanted_output_names,
                );
                kept.explicitly_requested |= node.explicitly_requested;
            }
        }
    }
    *nodes = deduped;
    let mut seen_edges = HashSet::new();
    edges.retain(|e| seen_edges.insert((e.parent_drv_path.clone(), e.child_drv_path.clone())));
}

/// Outcome of [`submit_dag`] — the shared DAG-submit pipeline for
/// `wopBuildPaths` and `wopBuildPathsWithResults`.
enum DagSubmitOutcome {
    /// Rate-limited or quota-exceeded. `STDERR_ERROR` already sent by
    /// the respective check; caller should `return Ok(())`.
    Gated,
    /// `validate_dag` rejected the DAG before submission. No
    /// `STDERR_ERROR` sent — caller decides whether to surface as
    /// `stderr_err!` (wopBuildPaths) or as a per-path
    /// `BuildResult::failure(InputRejected, …)` (wopBuildPathsWithResults).
    Rejected(String),
    /// Build was submitted and the scheduler returned a result
    /// (success OR failure — caller inspects `.status`).
    Built(BuildResult),
}

/// Shared DAG-submit pipeline:
/// `dedup → validate → rate-limit → quota → inline-drv → SubmitBuild`.
///
/// Runs every gate between DAG reconstruction and the scheduler RPC.
/// Gate ORDER is fixed here so the two build-paths opcodes cannot drift:
/// validate first (cheap, no I/O), then rate/quota (may send
/// `STDERR_ERROR`), then inline (store I/O), then submit. Prior to this
/// extraction the two handlers ran inline at different points relative
/// to rate/quota — harmless but inconsistent.
///
/// Returns `Err` only when `submit_and_process_build` itself errors
/// (scheduler transport/timeout); caller decides whether that is
/// session-terminal (`stderr_err!`) or a per-path `TransientFailure`.
async fn submit_dag<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
    mut nodes: Vec<types::DerivationNode>,
    mut edges: Vec<types::DerivationEdge>,
) -> anyhow::Result<DagSubmitOutcome> {
    let SessionContext {
        store_client,
        log_client,
        scheduler_client,
        drv_cache,
        active_build_ids,
        tenant_name,
        jwt,
        limiter,
        quota_cache,
        ..
    } = ctx;

    dedup_dag(&mut nodes, &mut edges);

    if let Err(reason) = translate::validate_dag(&nodes, drv_cache) {
        warn!(reason = %reason, "rejecting build: DAG validation failed");
        return Ok(DagSubmitOutcome::Rejected(reason));
    }

    if rate_limit_check(stderr, limiter, tenant_name.as_ref()).await? {
        return Ok(DagSubmitOutcome::Gated);
    }
    if quota_check(
        stderr,
        quota_cache,
        store_client,
        tenant_name.as_ref(),
        jwt.token(),
    )
    .await?
    {
        return Ok(DagSubmitOutcome::Gated);
    }

    translate::filter_and_inline_drv(&mut nodes, drv_cache, store_client).await;

    // sh-036 §3: between `wopAddMultipleToStore` finishing and the
    // first `actSubstitute` start the client sees ~13 s of dead air
    // (filter FMP above + the scheduler's MergeDag turn). Emit one
    // `STDERR_NEXT` line naming the post-dedup/post-filter node count
    // — the same N the scheduler reports in `BuildProgress.total` — so
    // `nix build` shows what is being waited on. AFTER `filter` so the
    // count is the one the scheduler sees; the filter's anon FMP is
    // <1 s so the half-second of earlier landing a pre-filter emit
    // would buy is not worth the count drift.
    stderr
        .log(&format!("rio: planning {n} derivations\n", n = nodes.len()))
        .await?;

    let request = translate::build_submit_request(nodes, edges, "ci", tenant_name.as_ref());
    let result = submit_and_process_build(
        stderr,
        scheduler_client,
        log_client,
        request,
        active_build_ids,
        jwt.token_source(),
    )
    .await?;
    Ok(DagSubmitOutcome::Built(result))
}

/// Resolve a `.drv` and reconstruct its full transitive DAG. Shared
/// `DerivedPath::Built` arm of the two `wopBuildPaths*` handlers — both
/// do `resolve_derivation` → `reconstruct_dag` → extend nodes/edges,
/// differing only in error sink (`stderr_err!` abort vs. per-path
/// `BuildResult::failure`).
///
/// `outputs` is the request's `^out,dev` / `^*` selection for THIS
/// root — it seeds the root node's `wanted_output_names` (`^*` keeps
/// the empty all-wanted sentinel).
async fn resolve_built_dag(
    drv: &StorePath,
    outputs: &OutputSpec,
    ctx: &mut SessionContext,
) -> anyhow::Result<(
    Vec<types::DerivationNode>,
    Vec<types::DerivationEdge>,
    Derivation,
)> {
    let drv_obj = resolve_derivation(drv, &mut ctx.store_client, &mut ctx.drv_cache).await?;
    let (nodes, edges) = translate::reconstruct_dag(
        drv,
        &drv_obj,
        Some(outputs),
        &mut ctx.store_client,
        &mut ctx.drv_cache,
    )
    .await?;
    Ok((nodes, edges, drv_obj))
}

// r[impl gw.opcode.build-paths]
/// wopBuildPaths (9): Build a set of derivations.
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_build_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    read_build_mode_normal_only(reader, stderr, "wopBuildPaths").await?;

    tracing::Span::current().record("count", raw_paths.len());
    debug!(count = raw_paths.len(), "wopBuildPaths");

    // Collect all derivation paths and reconstruct a combined DAG
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    // Per-target demand for the post-build store verification — opcode 9
    // returns no per-path results, but its bare success word makes the same
    // promise that every requested output now exists.
    let mut targets: HashMap<usize, TargetDemand> = HashMap::new();

    for (idx, raw) in raw_paths.iter().enumerate() {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => stderr_err!(stderr, "invalid DerivedPath '{raw}': {e}"),
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                match grpc_is_valid_path(&mut ctx.store_client, ctx.jwt.token(), path).await {
                    Ok(true) => { /* exists, fine */ }
                    Ok(false) => {
                        stderr_err!(stderr, "path '{path}' is not valid and cannot be built");
                    }
                    Err(e) => stderr_err!(stderr, "store error: {e}"),
                }
            }
            DerivedPath::Built { drv, outputs } => {
                match resolve_built_dag(drv, outputs, ctx).await {
                    Ok((nodes, edges, drv_obj)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        targets.insert(
                            idx,
                            TargetDemand {
                                drv_path: drv.to_string(),
                                drv: drv_obj,
                                spec: outputs.clone(),
                            },
                        );
                    }
                    Err(e) => stderr_err!(stderr, "DAG reconstruction failed for '{drv}': {e}"),
                }
            }
        }
    }

    if !all_nodes.is_empty() {
        match submit_dag(stderr, ctx, all_nodes, all_edges).await {
            Ok(DagSubmitOutcome::Gated) => return Ok(()),
            Ok(DagSubmitOutcome::Rejected(reason)) => {
                stderr_err!(stderr, "build rejected: {reason}")
            }
            Ok(DagSubmitOutcome::Built(r)) if !r.status.is_success() => {
                stderr_err!(stderr, "build failed: {}", r.error_msg)
            }
            Ok(DagSubmitOutcome::Built(_)) => {
                // r[impl gw.opcode.build-results-honest+2]
                // The scheduler says the DAG completed; gate the success
                // word on the store actually holding every requested
                // output (same verification as wopBuildPathsWithResults).
                let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
                let checks =
                    check_targets_against_store(stderr, ctx, &targets, &mut hash_cache).await?;
                let mut indices: Vec<usize> = checks.keys().copied().collect();
                indices.sort_unstable();
                let missing: Vec<String> = indices
                    .iter()
                    .filter_map(|i| checks.get(i))
                    .flat_map(|c| c.missing.iter().cloned())
                    .collect();
                if !missing.is_empty() {
                    stderr_err!(
                        stderr,
                        "build completed but requested outputs are not in the store: {}",
                        missing.join("; ")
                    );
                }
            }
            Err(e) => stderr_err!(stderr, "build failed: {e}"),
        }
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

// r[impl gw.opcode.build-paths-with-results]
// r[impl gw.stderr.error-before-return+2]
/// wopBuildPathsWithResults (46): Build paths and return per-path BuildResult.
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_build_paths_with_results<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    read_build_mode_normal_only(reader, stderr, "wopBuildPathsWithResults").await?;

    tracing::Span::current().record("count", raw_paths.len());
    debug!(count = raw_paths.len(), "wopBuildPathsWithResults");

    let mut results = Vec::new();

    // Collect all derivation paths to build together
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut opaque_results: HashMap<usize, BuildResult> = HashMap::new();
    // Track idx → TargetDemand (drvPath, Derivation, OutputSpec) for Built
    // paths so the per-target store verification and builtOutputs
    // population after the build know exactly what each entry asked for.
    // Without builtOutputs, the client can't map the derivation to its outputs
    // and falls back to some NAR-based verification → "error: no sink".
    let mut drv_for_idx: HashMap<usize, TargetDemand> = HashMap::new();

    for (idx, raw) in raw_paths.iter().enumerate() {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => {
                opaque_results.insert(
                    idx,
                    BuildResult::failure(
                        BuildStatus::InputRejected,
                        format!("invalid path '{raw}': {e}"),
                    ),
                );
                continue;
            }
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                let result =
                    match grpc_is_valid_path(&mut ctx.store_client, ctx.jwt.token(), path).await {
                        Ok(true) => BuildResult {
                            status: BuildStatus::AlreadyValid,
                            ..Default::default()
                        },
                        Ok(false) => BuildResult::failure(
                            BuildStatus::NoSubstituters,
                            format!("path '{}' not valid", path),
                        ),
                        Err(e) => BuildResult::failure(
                            BuildStatus::TransientFailure,
                            format!("store error: {e}"),
                        ),
                    };
                opaque_results.insert(idx, result);
            }
            DerivedPath::Built { drv, outputs } => {
                match resolve_built_dag(drv, outputs, ctx).await {
                    Ok((nodes, edges, drv_obj)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        drv_for_idx.insert(
                            idx,
                            TargetDemand {
                                drv_path: drv.to_string(),
                                drv: drv_obj,
                                spec: outputs.clone(),
                            },
                        );
                    }
                    Err(e) => {
                        opaque_results.insert(
                            idx,
                            BuildResult::failure(BuildStatus::MiscFailure, e.to_string()),
                        );
                    }
                }
            }
        }
    }

    if !all_nodes.is_empty() {
        let build_result = match submit_dag(stderr, ctx, all_nodes, all_edges).await {
            Ok(DagSubmitOutcome::Gated) => return Ok(()),
            Ok(DagSubmitOutcome::Rejected(reason)) => {
                BuildResult::failure(BuildStatus::InputRejected, reason)
            }
            Ok(DagSubmitOutcome::Built(r)) => r,
            Err(e) => {
                warn!(error = %e, "wopBuildPathsWithResults: build submission failed");
                metrics::counter!("rio_gateway_errors_total", "type" => "scheduler_submit")
                    .increment(1);
                BuildResult::failure(
                    BuildStatus::TransientFailure,
                    format!("scheduler error: {e}"),
                )
            }
        };

        // r[impl gw.opcode.build-results-honest+2]
        // Honest per-target results: a target is reported successful only
        // when the store confirms the outputs the client asked for actually
        // exist (defense in depth on top of the scheduler-side guarantees),
        // and a target whose outputs ARE all present is not blanket-failed
        // by an unrelated failure elsewhere in the batch. One batched
        // FindMissingPaths; store errors abort the opcode (stderr_err!
        // inside, before stderr.finish()).
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        let mut checks =
            check_targets_against_store(stderr, ctx, &drv_for_idx, &mut hash_cache).await?;

        for (idx, _raw) in raw_paths.iter().enumerate() {
            if let Some(opaque) = opaque_results.remove(&idx) {
                results.push(opaque);
                continue;
            }
            let (Some(demand), Some(check)) = (drv_for_idx.get(&idx), checks.remove(&idx)) else {
                results.push(build_result.clone());
                continue;
            };
            if build_result.status.is_success() {
                if check.missing.is_empty() {
                    // Verified (or unverifiable — defer to the scheduler):
                    // success, with builtOutputs covering exactly the
                    // wanted outputs.
                    results.push(result_with_wanted_outputs(
                        build_result.clone(),
                        demand,
                        &check,
                        &ctx.drv_cache,
                        &mut hash_cache,
                    ));
                } else {
                    // Wrong-success: the aggregate says Built but this
                    // target's requested outputs are not in the store.
                    warn!(
                        drv = %demand.drv_path,
                        missing = ?check.missing,
                        "demoting successful wopBuildPathsWithResults entry: outputs not in store"
                    );
                    results.push(BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!(
                            "build completed but requested outputs are not in the store: {}",
                            check.missing.join("; ")
                        ),
                    ));
                }
            } else if check.confirmed_present {
                // Partial outcome: the aggregate failed, but every output
                // THIS target asked for is present in the store — report
                // the target honestly as built.
                results.push(result_with_wanted_outputs(
                    BuildResult::success(),
                    demand,
                    &check,
                    &ctx.drv_cache,
                    &mut hash_cache,
                ));
            } else {
                // Unverified target under a failed aggregate: keep the
                // mapped failure status + error message.
                results.push(build_result.clone());
            }
        }
    } else {
        for idx in 0..raw_paths.len() {
            results.push(opaque_results.remove(&idx).unwrap_or_else(|| {
                BuildResult::failure(BuildStatus::MiscFailure, "unknown path".to_string())
            }));
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_u64(w, results.len() as u64).await?;
    for (raw, result) in raw_paths.iter().zip(results.iter()) {
        wire::write_string(w, raw).await?;
        write_build_result(w, result, ctx.negotiated_version).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::protocol::stderr::{STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY};

    fn ev(kind: types::DerivationEventKind, drv: &str, outs: &[&str]) -> types::DerivationEvent {
        types::DerivationEvent {
            derivation_path: drv.into(),
            kind: kind as i32,
            output_paths: outs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn subst_aids(act: &BuildActivityState, drv: &str) -> SubstAids {
        match act.display.get(drv) {
            Some(DrvDisplay::Subst(aids)) => aids.clone(),
            other => panic!("expected subst display for {drv}, got {other:?}"),
        }
    }

    fn build_aid(act: &BuildActivityState, drv: &str) -> u64 {
        match act.display.get(drv) {
            Some(DrvDisplay::Build(aid)) => *aid,
            other => panic!("expected build display for {drv}, got {other:?}"),
        }
    }

    fn subst_count(act: &BuildActivityState) -> usize {
        act.display
            .values()
            .filter(|d| matches!(d, DrvDisplay::Subst(_)))
            .count()
    }

    fn build_count(act: &BuildActivityState) -> usize {
        act.display
            .values()
            .filter(|d| matches!(d, DrvDisplay::Build(_)))
            .count()
    }

    // r[verify sched.merge.wanted-outputs+3]
    /// Multi-root submissions concatenate per-root node lists, so a drv
    /// reachable from two roots appears twice — each copy carrying the
    /// wanted set computed from THAT root's consumers/spec only.
    /// Retain-first dedup must UNION the dropped duplicate's
    /// `wanted_output_names` into the retained node: dropping it
    /// un-wants whatever the other root demanded (root A wants
    /// `child^out`, root B wants `child^dev` — keeping only A's copy
    /// would let the scheduler treat a missing `dev` as ignorable).
    /// Empty saturates the union (empty = "all declared outputs
    /// wanted", and all ∪ X = all).
    #[test]
    fn dedup_dag_unions_wanted_output_names() {
        let mk = |wanted: &[&str]| types::DerivationNode {
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            drv_hash: "x".into(),
            wanted_output_names: wanted.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };

        // {out} ∪ {dev} = {dev, out} (sorted union).
        let mut nodes = vec![mk(&["out"]), mk(&["dev"])];
        let mut edges = Vec::new();
        dedup_dag(&mut nodes, &mut edges);
        assert_eq!(nodes.len(), 1, "duplicates by drv_path collapse to one");
        assert_eq!(
            nodes[0].wanted_output_names,
            vec!["dev", "out"],
            "retained node must carry the UNION of all duplicates' wanted sets"
        );

        // {out} ∪ {} = {} — the empty (= all-wanted) sentinel saturates.
        let mut nodes = vec![mk(&["out"]), mk(&[])];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "all ∪ {{out}} = all (empty saturates)"
        );

        // {} ∪ {out} = {} — saturation is order-independent.
        let mut nodes = vec![mk(&[]), mk(&["out"])];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "{{out}} ∪ all = all (empty saturates regardless of which copy is retained)"
        );

        // Distinct drv_paths are untouched (no spurious cross-node union).
        let mut nodes = vec![
            types::DerivationNode {
                drv_path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y.drv".into(),
                wanted_output_names: vec!["out".into()],
                ..Default::default()
            },
            mk(&["dev"]),
        ];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].wanted_output_names, vec!["out"]);
        assert_eq!(nodes[1].wanted_output_names, vec!["dev"]);
    }

    /// Retain-first dedup must OR `explicitly_requested` across
    /// duplicates: a node the client named as a build target in ANY
    /// per-target walk stays flagged, no matter which copy is retained.
    /// Dropping the flag would re-expose the requested target to the
    /// scheduler's roots-only prune (it is a non-root of the combined
    /// submission, so only the flag keeps it in the demand set).
    #[test]
    fn dedup_dag_ors_explicitly_requested() {
        let mk = |requested: bool| types::DerivationNode {
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            drv_hash: "x".into(),
            explicitly_requested: requested,
            ..Default::default()
        };

        // Flagged copy is the DROPPED duplicate — flag must survive.
        let mut nodes = vec![mk(false), mk(true)];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(nodes.len(), 1);
        assert!(
            nodes[0].explicitly_requested,
            "flag set on a dropped duplicate must be ORed into the retained node"
        );

        // Flagged copy is the RETAINED one — stays flagged.
        let mut nodes = vec![mk(true), mk(false)];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert!(nodes[0].explicitly_requested);

        // No copy flagged — stays unflagged (no spurious promotion).
        let mut nodes = vec![mk(false), mk(false)];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert!(!nodes[0].explicitly_requested);
    }

    /// Multi-target request where one requested target lies INSIDE the
    /// other's inputDrv closure (`nix build .#app .#lib^dev` with
    /// app → lib): the combined, deduped submission must carry exactly
    /// one `lib` node that (a) is marked `explicitly_requested`, (b)
    /// wants the union of the client's selector and the consumer-derived
    /// names, and (c) keeps its incoming edge — i.e. the request's
    /// "build lib too" intent survives even though lib is not a
    /// structural root of the combined DAG.
    #[tokio::test]
    async fn multi_target_dedup_keeps_inner_target_flagged_and_wanted() {
        let app_path = StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app.drv")
            .expect("valid test store path");
        let lib_path = StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib.drv")
            .expect("valid test store path");

        // lib declares out+dev; app's inputDrvs entry names only {out}.
        let lib_drv = Derivation::parse(
            r#"Derive([("dev","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib-dev","",""),("out","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
        )
        .expect("test ATerm parses");
        let app_drv = Derivation::parse(
            r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app-out","","")],[("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[])"#,
        )
        .expect("test ATerm parses");

        // Both targets resolved into the per-session cache (production
        // does this via resolve_derivation before reconstruct_dag).
        let mut store = StoreServiceClient::new(rio_test_support::grpc::dead_channel());
        let mut cache = HashMap::new();
        cache.insert(app_path.clone(), app_drv.clone());
        cache.insert(lib_path.clone(), lib_drv.clone());

        // Per-target loop of handle_build_paths{,_with_results}: one
        // resolved sub-DAG per Built target, concatenated…
        let (mut nodes, mut edges) = translate::reconstruct_dag(
            &app_path,
            &app_drv,
            Some(&OutputSpec::All),
            &mut store,
            &mut cache,
        )
        .await
        .expect("app DAG reconstructs");
        let (lib_nodes, lib_edges) = translate::reconstruct_dag(
            &lib_path,
            &lib_drv,
            Some(&OutputSpec::Names(vec!["dev".into()])),
            &mut store,
            &mut cache,
        )
        .await
        .expect("lib DAG reconstructs");
        nodes.extend(lib_nodes);
        edges.extend(lib_edges);

        // …then deduped into ONE submission (submit_dag's first step).
        dedup_dag(&mut nodes, &mut edges);

        assert_eq!(nodes.len(), 2, "app + lib, lib's two copies collapsed");
        let lib = nodes
            .iter()
            .find(|n| n.drv_path == lib_path.as_str())
            .expect("lib node present");
        assert!(
            lib.explicitly_requested,
            "the client named lib as a build target — the combined \
             submission must keep it marked even though it is not a \
             structural root"
        );
        assert_eq!(
            lib.wanted_output_names,
            vec!["dev", "out"],
            "lib's wanted set = client selector (^dev) ∪ consumer-derived (out)"
        );
        let app = nodes
            .iter()
            .find(|n| n.drv_path == app_path.as_str())
            .expect("app node present");
        assert!(app.explicitly_requested, "app is a requested target too");
        assert!(
            edges
                .iter()
                .any(|e| e.parent_drv_path == app_path.as_str()
                    && e.child_drv_path == lib_path.as_str()),
            "lib keeps its incoming edge from app in the combined submission"
        );
    }

    /// Build a [`SessionContext`] whose every gRPC client is a
    /// [`dead_channel`](rio_test_support::grpc::dead_channel) and whose
    /// tenant/jwt/limiter/quota knobs are all in the no-op single-tenant
    /// position. With `tenant_name = None` the quota check short-circuits
    /// to `Unlimited` (no store RPC); with no `expected_output_paths` on
    /// the submitted nodes `filter_and_inline_drv` skips its
    /// `FindMissingPaths` probe — so `submit_dag` runs straight to the
    /// scheduler call without ever touching the dead store/log channels.
    fn dead_session_ctx() -> SessionContext {
        let dead = rio_test_support::grpc::dead_channel();
        SessionContext::new(
            StoreServiceClient::new(dead.clone()),
            rio_proto::LogServiceClient::new(dead.clone()),
            SchedulerServiceClient::new(dead),
            None,
            crate::handler::SessionJwt::none(),
            None,
            crate::ratelimit::TenantLimiter::disabled(),
            crate::quota::QuotaCache::new(),
        )
    }

    /// sh-036 §3: between `wopAddMultipleToStore` finishing and the
    /// first `actSubstitute` start, the client sees four wire frames
    /// covering ~13 s of dead air (filter FMP + scheduler MergeDag turn)
    /// — `nix build` sits on a blank progress bar with nothing to say
    /// what is happening. `submit_dag` is the last gateway-side
    /// chokepoint before that block, and at this point `nodes.len()` is
    /// final (post-dedup, post-filter — the same N the scheduler will
    /// report in `BuildProgress.total`), so it MUST emit one
    /// `STDERR_NEXT "rio: planning N derivations"` line BEFORE handing
    /// off to `submit_and_process_build`. The line is the FIRST
    /// `STDERR_NEXT` the build path writes — it precedes both the
    /// `rio: build <id>` line (post-MergeDag) and the
    /// `SubmitBuild RPC failed` diagnostic (the failure path this test
    /// drives via a dead scheduler channel).
    ///
    /// `start_paused`: the dead-channel `SubmitBuild` fails with
    /// `Unavailable`, which `submit_initial`'s pre-build_id retry loop
    /// backs off over 0.5/1/2/4 s; the paused clock auto-advances those
    /// sleeps so the test does not burn ~7.5 s of wall clock.
    #[tokio::test(start_paused = true)]
    async fn submit_dag_emits_planning_stderr_next() {
        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut ctx = dead_session_ctx();

        // Three distinct drvs, no `expected_output_paths` (skips the
        // filter's store probe). `dedup_dag` keeps all three.
        let nodes: Vec<types::DerivationNode> = (0..3)
            .map(|i| types::DerivationNode {
                drv_path: format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{i}-n{i}.drv"),
                drv_hash: format!("n{i}"),
                ..Default::default()
            })
            .collect();

        let outcome = submit_dag(&mut stderr, &mut ctx, nodes, Vec::new()).await;
        assert!(
            outcome.is_err(),
            "dead scheduler channel: SubmitBuild fails after retry budget"
        );

        let first_log = read_frames(buf)
            .await
            .into_iter()
            .find_map(|f| match f {
                Frame::Log(s) => Some(s),
                _ => None,
            })
            .expect("at least one STDERR_NEXT frame is written");
        assert_eq!(
            first_log, "rio: planning 3 derivations\n",
            "the planning line is the FIRST STDERR_NEXT submit_dag emits \
             (before the scheduler block — sh-036 §3)"
        );
    }

    /// Wire helper: read one full `STDERR_START_ACTIVITY` frame and
    /// return `(aid, ActivityType)`. Consumes text + N fields + parent.
    /// Panics if the next frame is not START_ACTIVITY.
    async fn read_start_activity<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> (u64, u64) {
        assert_eq!(wire::read_u64(r).await.unwrap(), STDERR_START_ACTIVITY);
        let aid = wire::read_u64(r).await.unwrap();
        let _level = wire::read_u64(r).await.unwrap();
        let act_type = wire::read_u64(r).await.unwrap();
        let _text = wire::read_string(r).await.unwrap();
        let nfields = wire::read_u64(r).await.unwrap();
        for _ in 0..nfields {
            match wire::read_u64(r).await.unwrap() {
                0 => {
                    let _ = wire::read_u64(r).await.unwrap();
                }
                1 => {
                    let _ = wire::read_string(r).await.unwrap();
                }
                t => panic!("unknown field type {t}"),
            }
        }
        let _parent = wire::read_u64(r).await.unwrap();
        (aid, act_type)
    }

    /// Wire helper: read one `STDERR_STOP_ACTIVITY` frame and return its aid.
    async fn read_stop_activity<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> u64 {
        assert_eq!(wire::read_u64(r).await.unwrap(), STDERR_STOP_ACTIVITY);
        wire::read_u64(r).await.unwrap()
    }

    /// Field value captured by [`read_frames`] — the C7 helper:
    /// `read_start_activity` discards string fields, but the
    /// live_043 assertions need the copy START frame's `from` field.
    #[derive(Debug, Clone, PartialEq)]
    enum FVal {
        Int(u64),
        Str(String),
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Frame {
        Start {
            aid: u64,
            act_type: u64,
            fields: Vec<FVal>,
        },
        Stop(u64),
        Result {
            aid: u64,
            rtype: u64,
            fields: Vec<FVal>,
        },
        Log(String),
    }

    /// Parse EVERY stderr frame in `buf` (start/stop/result/log) into
    /// a structural list — order-robust assertions for the deferred
    /// copy-start lifecycle.
    async fn read_frames(buf: Vec<u8>) -> Vec<Frame> {
        use rio_nix::protocol::stderr::STDERR_NEXT;
        let mut r = std::io::Cursor::new(buf);
        let mut frames = Vec::new();
        loop {
            let Ok(op) = wire::read_u64(&mut r).await else {
                break;
            };
            match op {
                x if x == STDERR_START_ACTIVITY => {
                    let aid = wire::read_u64(&mut r).await.unwrap();
                    let _level = wire::read_u64(&mut r).await.unwrap();
                    let act_type = wire::read_u64(&mut r).await.unwrap();
                    let _text = wire::read_string(&mut r).await.unwrap();
                    let n = wire::read_u64(&mut r).await.unwrap();
                    let mut fields = Vec::new();
                    for _ in 0..n {
                        match wire::read_u64(&mut r).await.unwrap() {
                            0 => fields.push(FVal::Int(wire::read_u64(&mut r).await.unwrap())),
                            1 => fields.push(FVal::Str(wire::read_string(&mut r).await.unwrap())),
                            t => panic!("unknown field type {t}"),
                        }
                    }
                    let _parent = wire::read_u64(&mut r).await.unwrap();
                    frames.push(Frame::Start {
                        aid,
                        act_type,
                        fields,
                    });
                }
                x if x == STDERR_STOP_ACTIVITY => {
                    frames.push(Frame::Stop(wire::read_u64(&mut r).await.unwrap()));
                }
                x if x == STDERR_RESULT => {
                    let aid = wire::read_u64(&mut r).await.unwrap();
                    let rtype = wire::read_u64(&mut r).await.unwrap();
                    let n = wire::read_u64(&mut r).await.unwrap();
                    let mut fields = Vec::new();
                    for _ in 0..n {
                        match wire::read_u64(&mut r).await.unwrap() {
                            0 => fields.push(FVal::Int(wire::read_u64(&mut r).await.unwrap())),
                            1 => fields.push(FVal::Str(wire::read_string(&mut r).await.unwrap())),
                            t => panic!("unknown field type {t}"),
                        }
                    }
                    frames.push(Frame::Result { aid, rtype, fields });
                }
                x if x == STDERR_NEXT => {
                    frames.push(Frame::Log(wire::read_string(&mut r).await.unwrap()));
                }
                op => panic!("unexpected opcode {op}"),
            }
        }
        frames
    }

    /// The copy child's aid, from its START frame (the pair may be
    /// closed and gone from the display map by assertion time).
    fn subst_copy_aid(frames: &[Frame]) -> u64 {
        frames
            .iter()
            .find_map(|f| match f {
                Frame::Start { aid, act_type, .. }
                    if *act_type == ActivityType::CopyPath as u64 =>
                {
                    Some(*aid)
                }
                _ => None,
            })
            .expect("a copy START frame exists")
    }

    /// Count resProgress results on `aid`.
    fn progress_results(frames: &[Frame], aid: u64) -> Vec<&Vec<FVal>> {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Result {
                    aid: a,
                    rtype,
                    fields,
                } if *a == aid && *rtype == ResultType::Progress as u64 => Some(fields),
                _ => None,
            })
            .collect()
    }

    /// SetExpected results on `aid` — the denominator-conservation
    /// reads ride this beside [`progress_results`].
    fn set_expected_results(frames: &[Frame], aid: u64) -> Vec<&Vec<FVal>> {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Result {
                    aid: a,
                    rtype,
                    fields,
                } if *a == aid && *rtype == ResultType::SetExpected as u64 => Some(fields),
                _ => None,
            })
            .collect()
    }

    /// The root's `SetExpected{actCopyPath}` VALUE sequence — the
    /// "X/Y copied" denominator nom renders, in emission order.
    fn copy_path_expected_seq(frames: &[Frame], root: u64) -> Vec<u64> {
        set_expected_results(frames, root)
            .iter()
            .filter(|f| f.first() == Some(&FVal::Int(ActivityType::CopyPath as u64)))
            .map(|f| match f.get(1) {
                Some(FVal::Int(v)) => *v,
                other => panic!("malformed SetExpected fields: {other:?}"),
            })
            .collect()
    }

    // r[verify gw.activity.subst-progress+4]
    /// live_045 red: substitution progress rides the `actCopyPath`
    /// child ONLY — the `actSubstitute` parent is structural (stock
    /// convention: `substitution-goal.cc` never emits `resProgress` on
    /// the substitute activity; the bytes belong to the
    /// `copyStorePath` child). The both-aids emission (42ebd60a9) fed
    /// direction-aware consumers — which dedup nested copies by
    /// `(path, host)` — two rows per path: the parent's empty
    /// substituter URI parses as Localhost and never matches the
    /// child's sourced URI, so every displayed byte doubled and
    /// locally-present paths' commit ticks were booked as downloads.
    #[tokio::test]
    async fn substitute_progress_lands_on_copy_child_only() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 7,
                bytes_expected: 100,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();

        let frames = read_frames(buf).await;
        let on_subst = progress_results(&frames, aids.subst).len();
        assert_eq!(
            on_subst, 0,
            "the actSubstitute parent is structural and must receive ZERO resProgress frames (a parent emission renders a second, never-deduped row in direction-aware consumers)"
        );
        let copy = subst_copy_aid(&frames);
        let on_copy = progress_results(&frames, copy).len();
        assert_eq!(on_copy, 1, "the sourced tick's bytes ride the copy child");
    }

    // r[verify gw.activity.subst-progress+4]
    /// live_043 red #2: the copy activity's START frame must carry the
    /// upstream source in its `from` field (stock copyStorePath
    /// semantics: [storePath, from, to]) — the relay discarded
    /// SubstituteProgress.upstream_uri and `from` stayed empty forever,
    /// so direction-aware consumers rendered a download as a
    /// local-to-local copy with no source.
    #[tokio::test]
    async fn copy_frame_carries_the_upstream_source() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-foo.drv";
        let uri = "https://cache.example";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();

        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 1,
                bytes_expected: 2,
                upstream_uri: uri.into(),
            },
        )
        .await
        .unwrap();

        let frames = read_frames(buf).await;
        let copy_start = frames
            .iter()
            .find_map(|f| match f {
                Frame::Start {
                    act_type, fields, ..
                } if *act_type == ActivityType::CopyPath as u64 => Some(fields.clone()),
                _ => None,
            })
            .expect("a copy START frame exists");
        assert_eq!(
            copy_start.get(1),
            Some(&FVal::Str(uri.to_string())),
            "the copy START frame's `from` field must carry the first sourced tick's upstream URI (got {copy_start:?})"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// sh-034 red: the actSubstitute parent's substituterUri starts
    /// empty (the upstream is unknown at Substituting-time) and the
    /// wire protocol has no field-update frame, so direction-aware
    /// consumers parse the empty fields[1] as Localhost forever and
    /// their substitution-host dedup never matches the child copy's
    /// real source — every fetch double-counts. The first sourced
    /// tick MUST backfill the URI on the parent via SetPhase ("from
    /// <uri>") so nom shows it; the nxb double-count fix is
    /// parent-aid linkage out-of-tree.
    #[tokio::test]
    async fn first_sourced_tick_backfills_subst_uri_phase() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu-foo.drv";
        let uri = "https://cache.example";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 1,
                bytes_expected: 2,
                upstream_uri: uri.into(),
            },
        )
        .await
        .unwrap();

        let frames = read_frames(buf).await;
        let set_phase = frames.iter().find_map(|f| match f {
            Frame::Result { aid, rtype, fields }
                if *aid == aids.subst && *rtype == ResultType::SetPhase as u64 =>
            {
                Some(fields.clone())
            }
            _ => None,
        });
        assert_eq!(
            set_phase,
            Some(vec![FVal::Str(format!("from {uri}"))]),
            "the first sourced tick must backfill the actSubstitute parent's URI \
             via a SetPhase result on aids.subst (frames: {frames:?})"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// live_043 companion green: a progress tick arriving AFTER the
    /// pair closed (the post-outcome straggler — delivery is droppable
    /// and reorderable end-to-end) is TOLERATED: no frames, the typed
    /// dropped lane, one debug line. Never reordered, never resurrects
    /// the pair.
    #[tokio::test]
    async fn straggler_after_close_is_a_counted_noop() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/tttttttttttttttttttttttttttttttt-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 5,
                bytes_expected: 10,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();
        // The straggler: the final tick lost the race to the outcome.
        // A fresh buffer isolates its (absence of) frames.
        let mut buf2 = Vec::new();
        let mut w2 = &mut buf2;
        let mut stderr2 = StderrWriter::new(&mut w2);
        let outcome = relay_substitute_progress(
            &mut stderr2,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 10,
                bytes_expected: 10,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, SubstRelay::DroppedUntracked, "tolerated, typed");
        assert!(
            buf2.is_empty(),
            "a post-close straggler emits NO frames (the pair is closed; \
             the close synthesis already completed the bar)"
        );
        assert!(!act.display.contains_key(drv), "never resurrected");
    }

    // r[verify gw.activity.subst-progress+4]
    /// live_043 red #3 (the FS-1 relocated binding red; lane corrected
    /// in live_045): a terminal outcome closing a pair whose
    /// last-relayed progress is partial must synthesize the completing
    /// resProgress on the COPY aid — the only progress lane — before
    /// the stops; the terminal outcome IS the completion proof. The
    /// actSubstitute parent stays structural through the close: ZERO
    /// resProgress frames over the pair's whole lifetime.
    #[tokio::test]
    async fn outcome_with_open_pair_renders_complete_progress() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/ssssssssssssssssssssssssssssssss-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 10,
                bytes_expected: 100,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();

        // The closing outcome arrives with NO further tick (the final
        // tick lost the race end-to-end).
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        let copy = subst_copy_aid(&frames);
        let completing = progress_results(&frames, copy)
            .iter()
            .any(|fields| fields.first() == Some(&FVal::Int(100)));
        assert!(
            completing,
            "pair closed with `done < expected` and no completing resProgress on the copy aid — the terminal outcome is the completion proof and must fill the bar (the live partial/empty-bar shape)"
        );
        assert_eq!(
            progress_results(&frames, aids.subst).len(),
            0,
            "the close synthesis must not leak resProgress onto the structural actSubstitute parent"
        );
    }

    // r[verify gw.display.family-flip]
    // r[verify gw.activity.subst-progress+4]
    /// merged_bug_003 red R1: a `Started` close fires on fetch FAILURE
    /// (the drv fell through to a build) — it DISPROVES transfer
    /// completion, so the close must FREEZE the last truthful relayed
    /// bar instead of synthesizing `[expected, expected, 0, 0]` (which
    /// rendered a failed partial fetch as a completed download).
    /// Certifies: the disproving-close wire sequence, frame-exact.
    #[tokio::test]
    async fn fell_through_close_freezes_last_truthful_bar() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r1r1r1r1r1r1r1r1r1r1r1r1r1r1r1r1-foo.drv";
        let uri = "https://cache.example";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();
        let machine = act.machine_name.clone();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: uri.into(),
            },
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        let copy = aids.copy.expect("sourced tick starts the copy child");

        // Fetch failed → the drv reverted to Ready → dispatched as a
        // build: `Started` closes the dangling pair.
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        let build = build_aid(&act, drv);

        let frames = read_frames(buf).await;
        assert_eq!(
            frames,
            vec![
                Frame::Start {
                    aid: aids.subst,
                    act_type: ActivityType::Substitute as u64,
                    fields: vec![FVal::Str(drv.into()), FVal::Str(String::new())],
                },
                Frame::Start {
                    aid: copy,
                    act_type: ActivityType::CopyPath as u64,
                    fields: vec![
                        FVal::Str(drv.into()),
                        FVal::Str(uri.into()),
                        FVal::Str(machine.clone()),
                    ],
                },
                Frame::Result {
                    aid: aids.subst,
                    rtype: ResultType::SetPhase as u64,
                    fields: vec![FVal::Str(format!("from {uri}"))],
                },
                Frame::Result {
                    aid: copy,
                    rtype: ResultType::Progress as u64,
                    fields: vec![FVal::Int(3), FVal::Int(9), FVal::Int(0), FVal::Int(0)],
                },
                Frame::Stop(copy),
                Frame::Stop(aids.subst),
                Frame::Start {
                    aid: build,
                    act_type: ActivityType::Build as u64,
                    fields: vec![
                        FVal::Str(drv.into()),
                        FVal::Str(machine),
                        FVal::Int(1),
                        FVal::Int(1),
                    ],
                },
            ],
            "a fell-through (fetch FAILED) close must freeze the last truthful bar: \
             no completing resProgress between the relayed tick and the stops"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// merged_bug_003 red R2: a `Failed` cascade close (the drv never
    /// fetched — DependencyFailed without Started/Cached) DISPROVES
    /// transfer completion: exactly the one relayed partial frame on
    /// the copy aid, stops child-first, then the failure line.
    /// Certifies: the cascade-close frame law.
    #[tokio::test]
    async fn failed_cascade_close_freezes_last_truthful_bar() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r2r2r2r2r2r2r2r2r2r2r2r2r2r2r2r2-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        let copy = aids.copy.expect("sourced tick starts the copy child");

        let mut fail_ev = ev(Failed, drv, &[]);
        fail_ev.failure_status = types::BuildResultStatus::DependencyFailed as i32;
        fail_ev.error_message = "dependency failed".into();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, fail_ev)
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        assert_eq!(
            progress_results(&frames, copy),
            vec![&vec![
                FVal::Int(3),
                FVal::Int(9),
                FVal::Int(0),
                FVal::Int(0)
            ]],
            "a cascade close must leave EXACTLY the relayed partial bar on the \
             copy aid — a second (completing) entry is the synthesized lie"
        );
        let stop_copy = frames
            .iter()
            .position(|f| *f == Frame::Stop(copy))
            .expect("copy stop present");
        let stop_subst = frames
            .iter()
            .position(|f| *f == Frame::Stop(aids.subst))
            .expect("subst stop present");
        let fail_line = frames
            .iter()
            .position(|f| matches!(f, Frame::Log(msg) if msg.contains("failed")))
            .expect("failure line present");
        assert!(
            stop_copy < stop_subst && stop_subst < fail_line,
            "stops child-first, then the failure line \
             (copy@{stop_copy}, subst@{stop_subst}, log@{fail_line})"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// merged_bug_003 red R3: a snapshot kind-flip close ("substitute
    /// fell through to a build" while the gateway was detached)
    /// DISPROVES transfer completion — the detached fell-through close
    /// must freeze the last truthful bar.
    #[tokio::test]
    async fn snapshot_kind_flip_close_freezes_last_truthful_bar() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r3r3r3r3r3r3r3r3r3r3r3r3r3r3r3r3-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        let copy = aids.copy.expect("sourced tick starts the copy child");

        // The drv is running as a BUILD in the snapshot: the substitute
        // fell through while we were detached.
        let snap = snap_running(&[(
            drv,
            "01900000-0000-7000-8000-0000000000d1",
            types::AttemptKind::Build,
        )]);
        apply_snapshot(&mut stderr, &mut act, &mut tails, snap)
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        assert_eq!(
            progress_results(&frames, copy),
            vec![&vec![
                FVal::Int(3),
                FVal::Int(9),
                FVal::Int(0),
                FVal::Int(0)
            ]],
            "a kind-flip close must leave EXACTLY the relayed partial bar on \
             the copy aid — no completing synthesis for a fell-through"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// merged_bug_003 red R4: a snapshot gone-reconcile close (the drv
    /// left the running set while we were detached — outcome
    /// UNOBSERVED) does not prove completion; the close must freeze
    /// the last truthful bar.
    #[tokio::test]
    async fn snapshot_gone_close_freezes_last_truthful_bar() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r4r4r4r4r4r4r4r4r4r4r4r4r4r4r4r4-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        let copy = aids.copy.expect("sourced tick starts the copy child");

        apply_snapshot(&mut stderr, &mut act, &mut tails, snap_running(&[]))
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        assert_eq!(
            progress_results(&frames, copy),
            vec![&vec![
                FVal::Int(3),
                FVal::Int(9),
                FVal::Int(0),
                FVal::Int(0)
            ]],
            "an unknown-outcome (gone) close must leave EXACTLY the relayed \
             partial bar on the copy aid — completion was never observed"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// merged_bug_003 red R5: the terminal drain closes pairs whose
    /// terminal event was LOST — the outcome is unknown, so the close
    /// must freeze the last truthful bar.
    #[tokio::test]
    async fn terminal_drain_close_freezes_last_truthful_bar() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r5r5r5r5r5r5r5r5r5r5r5r5r5r5r5r5-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        let copy = aids.copy.expect("sourced tick starts the copy child");

        drain_unstopped_activities(&mut stderr, &mut act)
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        assert_eq!(
            progress_results(&frames, copy),
            vec![&vec![
                FVal::Int(3),
                FVal::Int(9),
                FVal::Int(0),
                FVal::Int(0)
            ]],
            "an event-loss (drain) close must leave EXACTLY the relayed \
             partial bar on the copy aid — completion was never observed"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// bug_123 red R6: a zero-fetch pair (empty-URI commit tick only,
    /// no copy child ever) must RETIRE its `SetExpected{actCopyPath}`
    /// mint at close — the exact "X/Y copied completes at X < Y"
    /// residual the live_045 hotfix documented, now a defect.
    /// Certifies: the conservation law on the wire — the final
    /// denominator equals the count of copy starts (here zero).
    #[tokio::test]
    async fn zero_fetch_close_retires_copy_denominator() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r6r6r6r6r6r6r6r6r6r6r6r6r6r6r6r6-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            }),
        )
        .await
        .unwrap();
        let root = act.builds_root.expect("BuildStarted opens the root");

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        // The zero-fetch commit tick: empty URI, partial bar —
        // absorbed frameless, recorded as close input.
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: String::new(),
            },
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        assert_eq!(aids.copy, None, "zero-fetch: no copy child");

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        assert_eq!(
            copy_path_expected_seq(&frames, root),
            vec![1, 0],
            "a copy-less close must retire its mint: the root's \
             SetExpected{{actCopyPath}} sequence converges to the count of \
             copy starts (zero here)"
        );
        // The retire frame lands AFTER the pair's stop (deterministic
        // frame order for exact-wire siblings).
        let stop_subst = frames
            .iter()
            .position(|f| *f == Frame::Stop(aids.subst))
            .expect("subst stop present");
        let retire = frames
            .iter()
            .position(|f| {
                *f == Frame::Result {
                    aid: root,
                    rtype: ResultType::SetExpected as u64,
                    fields: vec![FVal::Int(ActivityType::CopyPath as u64), FVal::Int(0)],
                }
            })
            .expect("retire frame present");
        assert!(
            stop_subst < retire,
            "retire after the stops (stop@{stop_subst}, retire@{retire})"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// bug_123 red R7: a fell-through close (fetch failed, no tick at
    /// all, drv re-dispatched as a build) retires the mint — the drv
    /// re-counts under "X/Y built", not "copied". Wire tail pinned:
    /// `Stop(subst)` then the retire frame then `Start(build)`.
    #[tokio::test]
    async fn fell_through_close_retires_copy_denominator() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r7r7r7r7r7r7r7r7r7r7r7r7r7r7r7r7-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            }),
        )
        .await
        .unwrap();
        let root = act.builds_root.expect("BuildStarted opens the root");

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        let n = frames.len();
        assert_eq!(
            frames[n - 3],
            Frame::Stop(aids.subst),
            "tail: the pair's stop precedes the retire frame"
        );
        assert_eq!(
            frames[n - 2],
            Frame::Result {
                aid: root,
                rtype: ResultType::SetExpected as u64,
                fields: vec![FVal::Int(ActivityType::CopyPath as u64), FVal::Int(0)],
            },
            "tail: the copy-less close re-emits the decremented denominator"
        );
        assert!(
            matches!(&frames[n - 1], Frame::Start { act_type, .. }
                if *act_type == ActivityType::Build as u64),
            "tail: the build display opens after the retire (got {:?})",
            frames[n - 1]
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// bug_123 companion green R8: a SOURCED pair (copy child started)
    /// does NOT retire — the realize edge is not double-booked; the
    /// final denominator equals the one copy start.
    #[tokio::test]
    async fn sourced_close_does_not_retire() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/r8r8r8r8r8r8r8r8r8r8r8r8r8r8r8r8-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            }),
        )
        .await
        .unwrap();
        let root = act.builds_root.expect("BuildStarted opens the root");

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        assert_eq!(
            copy_path_expected_seq(&frames, root),
            vec![1],
            "a sourced close keeps its mint — the expectation realized"
        );
        let copy_starts = frames
            .iter()
            .filter(|f| {
                matches!(f, Frame::Start { act_type, .. }
                    if *act_type == ActivityType::CopyPath as u64)
            })
            .count();
        assert_eq!(copy_starts, 1);
        assert_eq!(
            copy_path_expected_seq(&frames, root).last().copied(),
            Some(copy_starts as u64),
            "final denominator == count of copy starts"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// bug_123 red R9: multi-pair conservation. Three drvs — A sourced
    /// then Cached, B empty-tick then Cached, C no-tick then Started —
    /// mint 1,2,3; B and C retire (2, then 1); the final denominator
    /// equals the ONE copy start. The only cross-pair coupling is the
    /// scalar — this is the composition witness.
    #[tokio::test]
    async fn denominator_converges_to_copy_starts_mixed_closure() {
        use types::DerivationEventKind::*;
        let drv_a = "/nix/store/r9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9-a.drv";
        let drv_b = "/nix/store/r9b9b9b9b9b9b9b9b9b9b9b9b9b9b9b9-b.drv";
        let drv_c = "/nix/store/r9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9-c.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Started(types::BuildStarted {
                total_derivations: 3,
                cached_derivations: 0,
            }),
        )
        .await
        .unwrap();
        let root = act.builds_root.expect("BuildStarted opens the root");

        for drv in [drv_a, drv_b, drv_c] {
            relay_derivation_status(
                &mut stderr,
                &mut act,
                &mut tails,
                ev(Substituting, drv, &[]),
            )
            .await
            .unwrap();
        }
        // A: sourced tick → copy starts → Cached (keeps its mint).
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv_a.into(),
                bytes_done: 4,
                bytes_expected: 8,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv_a, &[]))
            .await
            .unwrap();
        // B: empty commit tick only → Cached (zero-fetch; retires).
        relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv_b.into(),
                bytes_done: 2,
                bytes_expected: 5,
                upstream_uri: String::new(),
            },
        )
        .await
        .unwrap();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv_b, &[]))
            .await
            .unwrap();
        // C: no tick at all → Started (fell through; retires).
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv_c, &[]))
            .await
            .unwrap();

        let frames = read_frames(buf).await;
        let copy_starts = frames
            .iter()
            .filter(|f| {
                matches!(f, Frame::Start { act_type, .. }
                    if *act_type == ActivityType::CopyPath as u64)
            })
            .count();
        assert_eq!(copy_starts, 1, "only A sourced a copy");
        assert_eq!(
            copy_path_expected_seq(&frames, root),
            vec![1, 2, 3, 2, 1],
            "mints 1,2,3; B's close retires to 2; C's close retires to 1"
        );
        assert_eq!(
            copy_path_expected_seq(&frames, root).last().copied(),
            Some(copy_starts as u64),
            "final denominator == count of copy starts (conservation)"
        );
    }

    /// Closure-set census: `SubstCloseCause::ALL` carries every
    /// variant exactly once. The index fn is a TOTAL match — adding a
    /// variant without extending it is a compile error; the `seen`
    /// cover then fails until `ALL` carries the variant at its index.
    #[test]
    fn subst_close_cause_all_is_complete() {
        fn index(c: SubstCloseCause) -> usize {
            match c {
                SubstCloseCause::Cached => 0,
                SubstCloseCause::Completed => 1,
                SubstCloseCause::FellThroughToBuild => 2,
                SubstCloseCause::Failed => 3,
                SubstCloseCause::SnapshotKindFlip => 4,
                SubstCloseCause::SnapshotGone => 5,
                SubstCloseCause::TerminalDrain => 6,
            }
        }
        let mut seen = [0u8; SubstCloseCause::ALL.len()];
        for c in SubstCloseCause::ALL {
            seen[index(c)] += 1;
        }
        assert_eq!(
            seen,
            [1; SubstCloseCause::ALL.len()],
            "ALL is the close-cause alphabet, each variant exactly once"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// merged_bug_003 + bug_123 policy census: the close law is TOTAL
    /// over `SubstCloseCause::ALL` × {copy-open-partial, copy-less} —
    /// cells generated from the alphabet, never hand-enumerated.
    /// Certifies: synthesis ⇔ (cause proves completion ∧ copy started
    /// ∧ bar partial), with the expected proof status restated per
    /// variant through an independent exhaustive match (a production
    /// policy flip on ANY variant reds the matching cell); AND the
    /// retire law per cell — every copy-less close decrements the
    /// denominator and re-emits it on the root after the stops, for
    /// ALL 7 causes (retire is cause-independent); every copy-open
    /// close retires NOTHING.
    /// Pre-fix this test is a COMPILE-level red — the cause type did
    /// not exist (disclosed strawman; the behavioral pre-fix pins are
    /// R1–R5 for the synthesis license and R6/R7/R9 for the retire).
    #[tokio::test]
    async fn close_synthesis_policy_is_total_over_causes() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/g2g2g2g2g2g2g2g2g2g2g2g2g2g2g2g2-foo.drv";

        for cause in SubstCloseCause::ALL {
            // The proof table, restated independently of
            // `completion_proven` (total match: a new variant must
            // take a row here before this compiles). Cached is the
            // ONLY completion proof; `Completed` is pinned false per
            // the D2 divergence record — not a normal FSM transition
            // for a substituting drv, and under the lost-`Started`
            // path the completion claim is exactly false. A future
            // FSM change that legalizes Substituting→Completed
            // re-litigates that row HERE, visibly.
            let proves = match cause {
                SubstCloseCause::Cached => true,
                SubstCloseCause::Completed
                | SubstCloseCause::FellThroughToBuild
                | SubstCloseCause::Failed
                | SubstCloseCause::SnapshotKindFlip
                | SubstCloseCause::SnapshotGone
                | SubstCloseCause::TerminalDrain => false,
            };
            for copy_open in [true, false] {
                // Mint the pair through the PRODUCTION path: root via
                // BuildStarted (so retire frames are observable),
                // relay arm + progress relay (sourced tick opens the
                // copy child; empty-URI tick is absorbed frameless).
                let mut mint_buf = Vec::new();
                let mut mint_w = &mut mint_buf;
                let mut mint_stderr = StderrWriter::new(&mut mint_w);
                let mut act = BuildActivityState::default();
                let (mut tails, _rx) = lazy_tails();
                relay_build_progress(
                    &mut mint_stderr,
                    &mut act,
                    &types::build_event::Event::Started(types::BuildStarted {
                        total_derivations: 1,
                        cached_derivations: 0,
                    }),
                )
                .await
                .unwrap();
                let root = act.builds_root.expect("BuildStarted opens the root");
                relay_derivation_status(
                    &mut mint_stderr,
                    &mut act,
                    &mut tails,
                    ev(Substituting, drv, &[]),
                )
                .await
                .unwrap();
                relay_substitute_progress(
                    &mut mint_stderr,
                    &mut act,
                    types::SubstituteProgress {
                        derivation_path: drv.into(),
                        bytes_done: 3,
                        bytes_expected: 9,
                        upstream_uri: if copy_open {
                            "https://cache.example".into()
                        } else {
                            String::new()
                        },
                    },
                )
                .await
                .unwrap();
                let aids = subst_aids(&act, drv);
                assert_eq!(aids.copy.is_some(), copy_open, "mint shape");
                assert_eq!(act.subst_expected, 1, "one mint");

                // Drive THE chokepoint directly, mirroring production
                // callers (entry removed first); a fresh buffer
                // isolates the close frames.
                act.display.remove(drv);
                let mut buf = Vec::new();
                let mut w = &mut buf;
                let mut stderr = StderrWriter::new(&mut w);
                stop_subst_pair(&mut stderr, &mut act, aids.clone(), cause)
                    .await
                    .unwrap();
                let frames = read_frames(buf).await;

                let synthesized: usize = frames
                    .iter()
                    .filter(|f| {
                        matches!(f, Frame::Result { rtype, .. }
                            if *rtype == ResultType::Progress as u64)
                    })
                    .count();
                let want_synthesis = proves && copy_open; // bar is partial (3 < 9) by construction
                assert_eq!(
                    synthesized,
                    usize::from(want_synthesis),
                    "cell ({cause:?}, copy_open={copy_open}): synthesis ⇔ \
                     (proven ∧ copy ∧ partial)"
                );
                // Stops: child first iff started, parent always; the
                // retire edge rides the copy discriminant, not the
                // cause.
                if copy_open {
                    let copy = aids.copy.unwrap();
                    let stop_copy = frames
                        .iter()
                        .position(|f| *f == Frame::Stop(copy))
                        .expect("copy stop present");
                    let stop_subst = frames
                        .iter()
                        .position(|f| *f == Frame::Stop(aids.subst))
                        .expect("subst stop present");
                    assert!(stop_copy < stop_subst, "child stops first");
                    assert_eq!(
                        act.subst_expected, 1,
                        "cell ({cause:?}, copy_open): a realized expectation \
                         is NOT retired"
                    );
                    assert!(
                        copy_path_expected_seq(&frames, root).is_empty(),
                        "cell ({cause:?}, copy_open): no denominator \
                         re-emission at a copy-open close"
                    );
                } else {
                    assert_eq!(
                        frames,
                        vec![
                            Frame::Stop(aids.subst),
                            Frame::Result {
                                aid: root,
                                rtype: ResultType::SetExpected as u64,
                                fields: vec![
                                    FVal::Int(ActivityType::CopyPath as u64),
                                    FVal::Int(0),
                                ],
                            },
                        ],
                        "cell ({cause:?}, copy-less): the subst stop, then \
                         the retire re-emission — for EVERY cause"
                    );
                    assert_eq!(
                        act.subst_expected, 0,
                        "cell ({cause:?}, copy-less): the mint is retired"
                    );
                }
            }
        }
    }

    // r[verify gw.activity.subst-progress+4]
    /// live_045 companion: a pair whose only tick carried an EMPTY
    /// upstream_uri (job-level commit walk of a locally-present
    /// closure) never starts a copy child, so the close emits NO
    /// synthesis frame at all — the pair closes subst-only, the
    /// ratified truthful display (nothing was downloaded; booking the
    /// commit tick as a download is the regression shape).
    #[tokio::test]
    async fn close_without_copy_child_synthesizes_nothing() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        // The commit tick: empty URI, partial bar — recorded as
        // close-synthesis input but MUST NOT start a copy or emit.
        let relayed = relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 3,
                bytes_expected: 9,
                upstream_uri: String::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            relayed,
            SubstRelay::Relayed,
            "tracked: absorbed, not dropped"
        );

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();

        // Wire: START(subst) → STOP(subst). No copy start, no
        // resProgress on ANY aid — subst-only close.
        let frames = read_frames(buf).await;
        assert_eq!(
            frames,
            vec![
                Frame::Start {
                    aid: aids.subst,
                    act_type: ActivityType::Substitute as u64,
                    fields: vec![FVal::Str(drv.into()), FVal::Str(String::new())],
                },
                Frame::Stop(aids.subst),
            ],
            "a no-copy-child pair closes subst-only: no synthesis frame, no parent progress"
        );
    }

    // r[verify gw.stderr.activity+2]
    /// Substituting → start_activity(actSubstitute, [out, ""]); Cached →
    /// stop_activity(same aid). A merge-time Cached (no preceding
    /// Substituting) writes nothing.
    #[tokio::test]
    async fn relay_substituting_then_cached_renders_act_substitute() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo.drv";
        let out = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-foo";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[out]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);
        assert_eq!(act.subst_expected, 1);

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[out]))
            .await
            .unwrap();
        assert_eq!(subst_count(&act), 0, "Cached must remove subst aids");

        // r[verify gw.activity.subst-progress+4]
        // Wire (live_043 deferred-start): START(Substitute) only — no
        // sourced tick arrived, so no copy child ever started (a
        // zero-fetch/no-tick substitution closes subst-only, the
        // truthful display) — then STOP(subst). No SetExpected
        // (builds_root None).
        assert_eq!(aids.copy, None, "no sourced tick => no copy child");
        let mut r = std::io::Cursor::new(buf);
        let (sa, st) = read_start_activity(&mut r).await;
        assert_eq!(sa, aids.subst);
        assert_eq!(st, ActivityType::Substitute as u64);
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);

        // Merge-time Cached (no preceding Substituting) writes nothing.
        let mut buf2 = Vec::new();
        let mut w2 = &mut buf2;
        let mut stderr2 = StderrWriter::new(&mut w2);
        let mut act2 = BuildActivityState::default();
        relay_derivation_status(&mut stderr2, &mut act2, &mut tails, ev(Cached, drv, &[out]))
            .await
            .unwrap();
        assert!(
            buf2.is_empty(),
            "Cached without prior Substituting is silent"
        );
    }

    // r[verify gw.activity.subst-progress+4]
    /// Substituting → SubstituteProgress → Cached: progress emits a
    /// `STDERR_RESULT{copy_aid, resProgress, [done, expected, 0, 0]}`
    /// frame between START(CopyPath) and STOP(copy). This is what nom
    /// renders as the download bar.
    #[tokio::test]
    async fn relay_substitute_progress_emits_res_progress_on_copy() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/pppppppppppppppppppppppppppppppc-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();

        let relayed = relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 12_345_678,
                bytes_expected: 99_999_999,
                upstream_uri: "https://cache.example.test".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(relayed, SubstRelay::Relayed);
        // The sourced tick started the deferred copy child.
        let aids = subst_aids(&act, drv);
        let copy = aids.copy.expect("sourced tick starts the copy child");

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();

        // Wire (live_043 deferred start, live_045 copy-only lane,
        // sh-034 URI backfill): START(subst) → [sourced tick]
        // START(copy, from=uri) → resSetPhase(subst, "from <uri>") →
        // resProgress(copy) → [final tick already complete? no:
        // done < expected, so the close synthesizes completion on the
        // copy aid] → STOP(copy) → STOP(subst). The structural
        // actSubstitute parent carries NO resProgress anywhere.
        let frames = read_frames(buf).await;
        let mut it = frames.iter();
        assert!(matches!(
            it.next(),
            Some(Frame::Start { act_type, .. }) if *act_type == ActivityType::Substitute as u64
        ));
        match it.next() {
            Some(Frame::Start {
                aid,
                act_type,
                fields,
            }) => {
                assert_eq!(*aid, copy);
                assert_eq!(*act_type, ActivityType::CopyPath as u64);
                assert_eq!(
                    fields.get(1),
                    Some(&FVal::Str("https://cache.example.test".into())),
                    "from = the first sourced tick's URI"
                );
            }
            other => panic!("expected copy START, got {other:?}"),
        }
        // The first sourced tick backfills the parent's URI via
        // SetPhase (sh-034) — NOT a resProgress.
        match it.next() {
            Some(Frame::Result {
                aid: a,
                rtype,
                fields,
            }) => {
                assert_eq!(*a, aids.subst);
                assert_eq!(*rtype, ResultType::SetPhase as u64);
                assert_eq!(
                    fields,
                    &vec![FVal::Str("from https://cache.example.test".into())]
                );
            }
            other => panic!("expected SetPhase backfill on the subst aid, got {other:?}"),
        }
        // The tick's progress: the copy child only.
        match it.next() {
            Some(Frame::Result {
                aid: a,
                rtype,
                fields,
            }) => {
                assert_eq!(*a, copy);
                assert_eq!(*rtype, ResultType::Progress as u64);
                assert_eq!(fields[0], FVal::Int(12_345_678));
                assert_eq!(fields[1], FVal::Int(99_999_999));
            }
            other => panic!("expected progress on the copy aid, got {other:?}"),
        }
        // Close synthesis (done < expected at close): completing
        // progress on the copy aid only, then the stops, child first.
        match it.next() {
            Some(Frame::Result {
                aid: a,
                rtype,
                fields,
            }) => {
                assert_eq!(*a, copy);
                assert_eq!(*rtype, ResultType::Progress as u64);
                assert_eq!(fields[0], FVal::Int(99_999_999), "done == expected");
                assert_eq!(fields[1], FVal::Int(99_999_999));
            }
            other => panic!("expected completing progress on the copy aid, got {other:?}"),
        }
        assert_eq!(it.next(), Some(&Frame::Stop(copy)), "child first");
        assert_eq!(it.next(), Some(&Frame::Stop(aids.subst)));
        assert_eq!(it.next(), None);
        assert_eq!(
            progress_results(&frames, aids.subst).len(),
            0,
            "zero resProgress on the structural parent over the whole lifetime"
        );

        // Progress for an untracked drv is a TOLERATED no-op: no
        // frames, the typed dropped lane.
        let mut buf2 = Vec::new();
        let mut w2 = &mut buf2;
        let mut stderr2 = StderrWriter::new(&mut w2);
        let dropped = relay_substitute_progress(
            &mut stderr2,
            &mut BuildActivityState::default(),
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 1,
                bytes_expected: 2,
                upstream_uri: String::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(dropped, SubstRelay::DroppedUntracked);
        assert!(buf2.is_empty(), "no aids → no frames");
    }

    /// A failed substitute fetch reverts to Ready → eventually
    /// dispatches as a build → Started must close the dangling
    /// actSubstitute before opening actBuild.
    #[tokio::test]
    async fn relay_started_after_substituting_stops_subst_first() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/cccccccccccccccccccccccccccccccc-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        assert_eq!(subst_count(&act), 0, "Started must clear subst aids");
        let _ = build_aid(&act, drv); // Started must track the build display

        // Wire order (deferred-start: no sourced tick, no copy child):
        // START(Substitute), STOP(subst), START(Build).
        assert_eq!(aids.copy, None);
        let mut r = std::io::Cursor::new(buf);
        let (_, st) = read_start_activity(&mut r).await;
        assert_eq!(st, ActivityType::Substitute as u64);
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);
        let (_, bt) = read_start_activity(&mut r).await;
        assert_eq!(bt, ActivityType::Build as u64);
    }

    /// Substituting → (silent revert to Queued) → DependencyFailed
    /// cascade → Failed. The Failed arm must close the dangling
    /// actSubstitute aid; pre-fix it only touched `act.drv`, leaving
    /// nom showing a stuck "substituting 'X'" line.
    #[tokio::test]
    async fn relay_substituting_then_failed_stops_subst() {
        use rio_nix::protocol::stderr::STDERR_NEXT;
        use types::DerivationEventKind::*;
        let drv = "/nix/store/dddddddddddddddddddddddddddddddd-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        let mut fail_ev = ev(Failed, drv, &[]);
        fail_ev.error_message = "dependency failed".into();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, fail_ev)
            .await
            .unwrap();
        assert_eq!(
            subst_count(&act),
            0,
            "Failed must clear subst aids (pre-fix: leaked → stuck nom line)"
        );
        assert_eq!(build_count(&act), 0, "drv was never Started");

        // Wire order (deferred-start: no sourced tick, no copy
        // child): START(Substitute), STOP(subst), STDERR_NEXT(log).
        // STOP must precede the failure log so nom clears the
        // substituting line before printing the error.
        assert_eq!(aids.copy, None);
        let mut r = std::io::Cursor::new(buf);
        let (_, st) = read_start_activity(&mut r).await;
        assert_eq!(st, ActivityType::Substitute as u64);
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);

        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_NEXT);
        let msg = wire::read_string(&mut r).await.unwrap();
        assert!(msg.contains("failed"));
    }

    /// Substituting → Completed (defensive — not a normal scheduler
    /// FSM transition). Terminal arm closes any tracked subst aid.
    #[tokio::test]
    async fn relay_substituting_then_completed_stops_subst() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/5555555555555555555555555555555c-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let _ = subst_aids(&act, drv);

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Completed, drv, &[]))
            .await
            .unwrap();
        assert_eq!(
            subst_count(&act),
            0,
            "Completed must clear subst aid (terminal-arm symmetry)"
        );
    }

    // r[verify gw.activity.stop-parity]
    /// sh-035 red: Started → Cached (the reap→store-hit path —
    /// `dispatch.rs` `complete_ready_from_store_batch` emits Cached
    /// for a drv that was Running) must close the open `actBuild` aid.
    /// At base the Cached arm's `if let Subst` left a Build entry in
    /// the map and never wrote `stop_activity` — 26 orphan `actBuild`s
    /// in the iter8 capture, rendered "building" until terminus.
    /// Sibling: Cached with NO prior event (merge-time hit) is the
    /// `None` arm — must stay a no-op.
    #[tokio::test]
    async fn cached_after_started_closes_build_activity() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/s035s035s035s035s035s035s035s035-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        let aid = build_aid(&act, drv);

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();
        assert!(
            act.display.is_empty(),
            "Cached must close the open Build display (sh-035 reap→store-hit); \
             still tracked: {:?}",
            act.display
        );

        let frames = read_frames(buf).await;
        assert!(
            frames.contains(&Frame::Stop(aid)),
            "Cached after Started must emit stop_activity({aid}); frames: {frames:?}"
        );

        // Sibling: Cached with NO prior event (merge-time cache hit
        // never opened a display) → None arm → no-op, no frames.
        let cold = "/nix/store/c01dc01dc01dc01dc01dc01dc01dc01d-cold.drv";
        let mut buf2 = Vec::new();
        let mut w2 = &mut buf2;
        let mut stderr2 = StderrWriter::new(&mut w2);
        relay_derivation_status(&mut stderr2, &mut act, &mut tails, ev(Cached, cold, &[]))
            .await
            .unwrap();
        assert!(act.display.is_empty(), "None arm must not insert");
        assert!(
            read_frames(buf2).await.is_empty(),
            "merge-time Cached (no prior display) must emit no frames"
        );
    }

    // r[verify gw.activity.stop-parity]
    /// Three drvs Started, only one Completed (the other two
    /// completions were lost upstream). `drain_unstopped_activities`
    /// must emit `stop_activity` for the leaked aids and clear the
    /// maps.
    #[tokio::test]
    async fn drain_unstopped_activities_emits_stop_for_leaked() {
        use types::DerivationEventKind::*;
        let drvs = [
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv",
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv",
            "/nix/store/cccccccccccccccccccccccccccccccc-c.drv",
        ];

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        for d in &drvs {
            relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, d, &[]))
                .await
                .unwrap();
        }
        let aid_a = build_aid(&act, drvs[0]);
        let aid_b = build_aid(&act, drvs[1]);
        let aid_c = build_aid(&act, drvs[2]);

        // One Completed arrives normally; aids b/c leak.
        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Completed, drvs[0], &[]),
        )
        .await
        .unwrap();
        assert_eq!(build_count(&act), 2, "two aids leaked into terminal");

        let pre_drain_len = buf.len();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        drain_unstopped_activities(&mut stderr, &mut act)
            .await
            .unwrap();
        assert!(act.display.is_empty(), "map drained");

        // Wire after the drain point: exactly two STOP_ACTIVITY frames
        // for aids b and c (order is HashMap iteration — accept either).
        let mut r = std::io::Cursor::new(&buf[pre_drain_len..]);
        let mut stopped = Vec::new();
        for _ in 0..2 {
            assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_STOP_ACTIVITY);
            stopped.push(wire::read_u64(&mut r).await.unwrap());
        }
        stopped.sort_unstable();
        let mut expected = [aid_b, aid_c];
        expected.sort_unstable();
        assert_eq!(stopped, expected, "leaked aids must be stopped at terminal");
        // No extra bytes — drain emits ONLY the leaked stops, never
        // a duplicate of aid_a (already stopped via Completed).
        assert!(
            r.position() as usize == buf.len() - pre_drain_len,
            "no surplus frames after drain"
        );
        let _ = aid_a;
    }

    /// Build a [`LogTailSet`] against a lazy (never-connected) channel:
    /// `on_started` task spawns are observable via `tracked_drvs()`
    /// without a live LogService.
    fn lazy_tails() -> (LogTailSet, tokio::sync::mpsc::Receiver<TaggedLogChunk>) {
        let chan = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        LogTailSet::new(
            rio_proto::LogServiceClient::new(chan),
            crate::server::session_jwt::SessionTokenSource::none(),
        )
    }

    fn snap_running(entries: &[(&str, &str, types::AttemptKind)]) -> types::BuildSnapshot {
        types::BuildSnapshot {
            running: entries
                .iter()
                .map(|(drv, exec, kind)| types::RunningDerivation {
                    derivation_path: (*drv).to_string(),
                    exec_id: (*exec).to_string(),
                    kind: *kind as i32,
                })
                .collect(),
            ..Default::default()
        }
    }

    // r[verify gw.display.single-map]
    /// A substitution that completed while the gateway was detached must
    /// be closed by the snapshot gone-reconcile: subst-only drv seeded,
    /// snapshot with empty running set ⇒ the pair's stop frames are
    /// emitted and the entry is removed from the display map.
    #[tokio::test]
    async fn apply_snapshot_closes_substitution_display_on_gone() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/cccccccccccccccccccccccccccccccc-gone.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        let aids = subst_aids(&act, drv);

        let outcome = apply_snapshot(&mut stderr, &mut act, &mut tails, snap_running(&[]))
            .await
            .unwrap();
        assert!(outcome.is_none());
        assert!(
            !act.display.contains_key(drv),
            "gone-reconcile must remove the substitution display entry"
        );

        // Wire (deferred-start: no sourced tick, subst-only pair):
        // START(subst), then the root's activity from apply_snapshot,
        // then STOP(subst) from the reconcile.
        assert_eq!(aids.copy, None);
        let mut r = std::io::Cursor::new(buf);
        let (sa, _) = read_start_activity(&mut r).await;
        assert_eq!(sa, aids.subst);
        let (_root, rt) = read_start_activity(&mut r).await;
        assert_eq!(rt, ActivityType::Builds as u64, "snapshot creates root");
        // Root SetExpected result frame precedes the stop:
        // aid + type + nfields + 2 int fields (type,value pairs).
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_RESULT);
        for _ in 0..3 + 2 * 2 {
            let _ = wire::read_u64(&mut r).await.unwrap();
        }
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);
    }

    // r[verify sched.pull.kinded-running-surface]
    // r[verify gw.display.single-map]
    /// Snapshot running entries route by kind: BUILD ⇒ actBuild + log
    /// tail; MATERIALIZATION ⇒ substitute display pair, NO tail, NO
    /// actBuild.
    #[tokio::test]
    async fn apply_snapshot_kinded_running_routes_display() {
        let bdrv = "/nix/store/dddddddddddddddddddddddddddddddd-build.drv";
        let mdrv = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-mat.drv";
        let bexec = "01900000-0000-7000-8000-0000000000b1";
        let mexec = "01900000-0000-7000-8000-0000000000a1";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        let snap = snap_running(&[
            (bdrv, bexec, types::AttemptKind::Build),
            (mdrv, mexec, types::AttemptKind::Materialization),
        ]);
        apply_snapshot(&mut stderr, &mut act, &mut tails, snap)
            .await
            .unwrap();

        assert!(
            matches!(act.display.get(bdrv), Some(DrvDisplay::Build(_))),
            "build entry gets the actBuild display"
        );
        assert!(
            matches!(act.display.get(mdrv), Some(DrvDisplay::Subst(_))),
            "materialization entry gets the substitute display pair, not actBuild"
        );
        let tracked = tails.tracked_drvs();
        assert!(
            tracked.iter().any(|d| d == bdrv),
            "build entry opens a log tail"
        );
        assert!(
            !tracked.iter().any(|d| d == mdrv),
            "materialization entry must NOT open a log tail (no execution log exists)"
        );
    }

    // r[verify gw.display.single-map]
    /// A drv whose kind flipped while detached (materialization running
    /// in the snapshot, but tracked as a build) swaps display families:
    /// the stale actBuild stops, the substitute pair starts.
    #[tokio::test]
    async fn apply_snapshot_kind_flip_swaps_display_family() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/ffffffffffffffffffffffffffffffff-flip.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        let build_aid = match act.display.get(drv) {
            Some(DrvDisplay::Build(aid)) => *aid,
            other => panic!("expected build display, got {other:?}"),
        };

        let snap = snap_running(&[(
            drv,
            "01900000-0000-7000-8000-0000000000c1",
            types::AttemptKind::Materialization,
        )]);
        apply_snapshot(&mut stderr, &mut act, &mut tails, snap)
            .await
            .unwrap();
        assert!(
            matches!(act.display.get(drv), Some(DrvDisplay::Subst(_))),
            "kind flip must swap the display family to subst"
        );
        let _ = build_aid;
    }

    // r[verify gw.display.single-map]
    /// The LIVE twin of `apply_snapshot_kind_flip_swaps_display_family`:
    /// a `Substituting` event for a drv with an open build display flips
    /// the family through the same authority the snapshot reconcile
    /// uses — the stale actBuild stops, the substitute pair starts, and
    /// the root's CopyPath denominator bumps.
    #[tokio::test]
    async fn live_substituting_kind_flip_swaps_display_family() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/gggggggggggggggggggggggggggggggg-liveflip.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        // Root first so the SetExpected{actCopyPath} bump is on-wire.
        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            }),
        )
        .await
        .unwrap();
        let root = act.builds_root.expect("root exists");

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        let aid = build_aid(&act, drv);

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        assert!(
            matches!(act.display.get(drv), Some(DrvDisplay::Subst(_))),
            "live kind flip must swap the display family to subst (got {:?})",
            act.display.get(drv)
        );

        let frames = read_frames(buf).await;
        assert!(
            frames.contains(&Frame::Stop(aid)),
            "the dead build execution's stop_activity frame must be written"
        );
        let copy_bumps: Vec<_> = set_expected_results(&frames, root)
            .into_iter()
            .filter(|f| f.first() == Some(&FVal::Int(ActivityType::CopyPath as u64)))
            .collect();
        assert_eq!(
            copy_bumps,
            vec![&vec![
                FVal::Int(ActivityType::CopyPath as u64),
                FVal::Int(1)
            ]],
            "the root SetExpected{{actCopyPath}} denominator bump must be emitted"
        );
    }

    // r[verify gw.display.single-map]
    /// After the live kind flip, `SubstituteProgress` ticks must relay.
    /// Pre-fix every tick dropped as `DroppedUntracked` — no `Subst`
    /// entry existed for the drv, so the download bar never moved.
    #[tokio::test]
    async fn substitute_progress_relays_after_live_kind_flip() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-livetick.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();

        let relay = relay_substitute_progress(
            &mut stderr,
            &mut act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 7,
                bytes_expected: 100,
                upstream_uri: "https://cache.example".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            relay,
            SubstRelay::Relayed,
            "progress after the live flip must relay, not drop"
        );
    }

    // r[verify gw.display.single-map]
    /// A flipped drv terminates via `Cached`; the flip means terminus
    /// finds NOTHING left to drain — the "upstream event loss" warn is
    /// structurally silent for this path (nothing was lost).
    #[tokio::test]
    async fn cached_after_live_flip_leaves_no_drain_set() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj-livecached.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Cached, drv, &[]))
            .await
            .unwrap();
        assert!(
            act.display.is_empty(),
            "Cached after the live flip closes the (single) display entry — drain set empty, got {:?}",
            act.display
        );

        let pre_drain_len = buf.len();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        drain_unstopped_activities(&mut stderr, &mut act)
            .await
            .unwrap();
        assert_eq!(
            buf.len(),
            pre_drain_len,
            "the terminus drain emits ZERO frames for this path"
        );
    }

    // r[verify gw.display.single-map]
    /// The live flip cuts the dead execution's log tail exactly as the
    /// snapshot arm does: one drv flipped live, one flipped through the
    /// snapshot reconcile — both subscriptions end drain-flagged.
    #[tokio::test]
    async fn live_flip_cuts_the_stale_tail() {
        use types::DerivationEventKind::*;
        let live = "/nix/store/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk-livetail.drv";
        let snap = "/nix/store/llllllllllllllllllllllllllllllll-snaptail.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        for drv in [live, snap] {
            relay_derivation_status(
                &mut stderr,
                &mut act,
                &mut tails,
                types::DerivationEvent {
                    exec_id: "01900000-0000-7000-8000-0000000000e1".into(),
                    ..ev(Started, drv, &[])
                },
            )
            .await
            .unwrap();
            assert_eq!(
                tails.draining(drv),
                Some(false),
                "the execution's subscription opens live"
            );
        }

        // Live flip for `live` — asserted BEFORE any snapshot drive, so
        // the gone-reconcile (which also cuts build tails) cannot mask
        // a missing live cut.
        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, live, &[]),
        )
        .await
        .unwrap();
        assert_eq!(
            tails.draining(live),
            Some(true),
            "the live flip must cut the dead execution's tail (parity with the snapshot arm)"
        );

        // Snapshot kind-flip for `snap` — the parity baseline. `live`
        // rides along as a gone entry (its pair closes; no tail effect).
        apply_snapshot(
            &mut stderr,
            &mut act,
            &mut tails,
            snap_running(&[(
                snap,
                "01900000-0000-7000-8000-0000000000e2",
                types::AttemptKind::Materialization,
            )]),
        )
        .await
        .unwrap();
        assert_eq!(
            tails.draining(snap),
            Some(true),
            "the snapshot flip cuts the tail (the parity baseline)"
        );
    }

    /// Collect the STDERR_NEXT log lines from `buf`.
    async fn log_lines(buf: Vec<u8>) -> Vec<String> {
        read_frames(buf)
            .await
            .into_iter()
            .filter_map(|f| match f {
                Frame::Log(s) => Some(s),
                Frame::Start { .. } | Frame::Stop(_) | Frame::Result { .. } => None,
            })
            .collect()
    }

    // r[verify gw.stderr.activity+2]
    /// bug_080: a failure event NOT backed by a fresh execution (the
    /// no-eligible-source lane fires with "no pod and no attempt")
    /// must print NO `rio-cli logs` hint — any log a drv-named lookup
    /// resolves is a PRIOR attempt's, and the operator would debug the
    /// wrong attempt. Population: Subst display open (the lost-Started
    /// window the old status gate ignored).
    #[tokio::test]
    async fn no_execution_failure_prints_no_log_hint_over_subst_display() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-noexec.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            types::DerivationEvent::failed(
                drv.into(),
                "no eligible source: every spawnable node is excluded".into(),
                types::BuildResultStatus::TransientFailure,
                rio_proto::VerdictBacking::NoExecution,
            ),
        )
        .await
        .unwrap();

        let lines = log_lines(buf).await;
        let failure_line = lines
            .iter()
            .find(|l| l.contains("failed"))
            .expect("the failure line prints");
        assert!(
            !failure_line.contains("rio-cli logs"),
            "no-execution failure must not print the misleading log hint, got: {failure_line:?}"
        );
    }

    // r[verify gw.stderr.activity+2]
    /// bug_080, the under-coverage twin: the same no-execution failure
    /// with NO prior display (a poison before any dispatch in a new
    /// build with carried exclusions) — the rejected `was_subst`
    /// discriminant misses this population entirely.
    #[tokio::test]
    async fn no_execution_failure_prints_no_log_hint_with_no_display() {
        let drv = "/nix/store/nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn-nodisp.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            types::DerivationEvent::failed(
                drv.into(),
                "no eligible source: every spawnable node is excluded".into(),
                types::BuildResultStatus::TransientFailure,
                rio_proto::VerdictBacking::NoExecution,
            ),
        )
        .await
        .unwrap();

        let lines = log_lines(buf).await;
        let failure_line = lines
            .iter()
            .find(|l| l.contains("failed"))
            .expect("the failure line prints");
        assert!(
            !failure_line.contains("rio-cli logs"),
            "no-display no-execution failure must not print the hint, got: {failure_line:?}"
        );
    }

    /// bug_080 over-suppression refutation (GREEN pin, disclosed: green
    /// by accident pre-fix — status-gated; green by statement post-fix):
    /// a worker-reported failure on a drv whose display is still Subst
    /// (a lost `Started` window) has a fresh, useful log — the hint
    /// stays. The rejected `was_subst` discriminant would suppress it.
    #[tokio::test]
    async fn worker_reported_failure_keeps_the_hint_regardless_of_display_family() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/pppppppppppppppppppppppppppppppp-fresh.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            ev(Substituting, drv, &[]),
        )
        .await
        .unwrap();
        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            types::DerivationEvent::failed(
                drv.into(),
                "builder exited 1".into(),
                types::BuildResultStatus::PermanentFailure,
                rio_proto::VerdictBacking::FreshExecution,
            ),
        )
        .await
        .unwrap();

        let lines = log_lines(buf).await;
        let failure_line = lines
            .iter()
            .find(|l| l.contains("failed"))
            .expect("the failure line prints");
        assert!(
            failure_line.contains("rio-cli logs"),
            "a fresh-execution failure keeps its hint whatever the display family, got: {failure_line:?}"
        );
    }

    // r[verify gw.stderr.activity+2]
    /// live_051(f1) — KD-scope: an attempt/cycle-scoped cancellation on
    /// a drv whose build CONTINUES renders in the attempt vocabulary
    /// (retry-implying) and never mints the build-terminal phrase; the
    /// continued work on the same drv is the structural continuation
    /// witness (the relay keeps tracking it).
    #[tokio::test]
    async fn attempt_level_cancel_does_not_render_as_build_termination() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-reaped.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();
        let (mut tails, _rx) = lazy_tails();

        relay_derivation_status(
            &mut stderr,
            &mut act,
            &mut tails,
            types::DerivationEvent::failed(
                drv.into(),
                "stale Job reaped; attempt closed charge-free".into(),
                types::BuildResultStatus::Cancelled,
                rio_proto::VerdictBacking::NoExecution,
            ),
        )
        .await
        .unwrap();
        // The drv requeues and keeps building — the stream continues.
        relay_derivation_status(&mut stderr, &mut act, &mut tails, ev(Started, drv, &[]))
            .await
            .unwrap();
        assert!(
            matches!(act.display.get(drv), Some(DrvDisplay::Build(_))),
            "the build continues after the attempt-scoped cancel (structural continuation)"
        );

        let lines = log_lines(buf).await;
        let cancel_line = lines
            .iter()
            .find(|l| l.contains("cancelled"))
            .expect("the cancellation line prints");
        assert!(
            cancel_line.contains("attempt cancelled")
                && cancel_line.contains("retriable via explicit resubmit"),
            "cycle-scoped cancellation must render retry-implying attempt vocabulary, got: {cancel_line:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("build cancelled")),
            "no attempt-scoped event may mint the build-terminal vocabulary"
        );
    }

    /// live_051(f1) W-F2 (GREEN pin, disclosed): the scope classifier is
    /// total over the closed wire-status alphabet and maps EXACTLY
    /// Cancelled to the attempt vocabulary — rustc exhaustiveness is
    /// the census generator (a new BuildResultStatus variant fails to
    /// compile until it takes a scope position); the build-scoped
    /// "build cancelled" phrase keeps its sole mint at the outcome fold
    /// (the existing Cancelled-outcome wire tests pin that mapping).
    #[test]
    fn terminal_scope_partitions_exactly_the_cancelled_status() {
        use types::BuildResultStatus as S;
        // Membership machine-derived from the prost decoder over the
        // contiguous wire range (the generator is the wire alphabet,
        // not an author list); rustc exhaustiveness in
        // `TerminalScope::of` is the primary census — an added variant
        // fails to COMPILE until it takes a scope position, decoded or
        // not.
        let all: Vec<S> = (0..).map_while(|raw| S::try_from(raw).ok()).collect();
        assert!(
            all.contains(&S::Cancelled) && all.len() > 10,
            "decoder-derived alphabet sane (got {} variants)",
            all.len()
        );
        for status in all {
            let scope = TerminalScope::of(status);
            if status == S::Cancelled {
                assert!(
                    matches!(scope, TerminalScope::AttemptCancelled),
                    "Cancelled is the attempt/cycle scope"
                );
            } else {
                assert!(
                    matches!(scope, TerminalScope::Failure),
                    "{status:?} rides the generic failure lane"
                );
            }
        }
    }

    /// The live Progress arm and the snapshot correction emit the same
    /// resProgress array: field 4 (failed) carries the scheduler's
    /// count on BOTH paths.
    #[tokio::test]
    async fn live_progress_carries_failed_count() {
        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Started(types::BuildStarted {
                total_derivations: 10,
                cached_derivations: 2,
            }),
        )
        .await
        .unwrap();
        relay_build_progress(
            &mut stderr,
            &mut act,
            &types::build_event::Event::Progress(types::BuildProgress {
                completed: 4,
                running: 1,
                queued: 0,
                total: 10,
                failed: 5,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        // Wire: START(Builds root), SetExpected result, then the
        // Progress result whose 4th int field must be `failed`.
        let mut r = std::io::Cursor::new(buf);
        let (root, _) = read_start_activity(&mut r).await;
        // SetExpected: aid + type + nfields + 2 int fields.
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_RESULT);
        for _ in 0..3 + 2 * 2 {
            let _ = wire::read_u64(&mut r).await.unwrap();
        }
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_RESULT);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), root);
        assert_eq!(
            wire::read_u64(&mut r).await.unwrap(),
            ResultType::Progress as u64
        );
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 4, "four fields");
        let mut ints = [0u64; 4];
        for slot in &mut ints {
            assert_eq!(wire::read_u64(&mut r).await.unwrap(), 0, "int field");
            *slot = wire::read_u64(&mut r).await.unwrap();
        }
        assert_eq!(
            ints,
            [4, 10, 1, 5],
            "resProgress [done, expected, running, failed] — failed must carry the scheduler's count"
        );
    }

    // ---- ReattachBudget (merged_bug_056) ----
    //
    // RECORDED RED (pre-fix semantics): the watch loop reset its
    // reconnect counter on ANY event from the stream, snapshot
    // included. Probe: re-adding `Event::Snapshot(_) => self.attempts
    // = 0` to `note_event` (the one-line pre-fix equivalent) fails
    // `resync_storm_exhausts_the_budget` with
    // `assertion failed: budget.exhausted()` after MAX_RECONNECT+1
    // snapshot-cycles — the storm refreshes its own cap forever.
    // Probe applied + reverted, see commit body.

    // r[verify gw.resync.reattach-budget+3]
    /// A snapshot-then-resync storm consumes the budget: snapshots do
    /// NOT reset it, so MAX_RECONNECT cycles exhaust the watch instead
    /// of looping forever at zero backoff.
    #[test]
    fn resync_storm_exhausts_the_budget() {
        let mut budget = super::ReattachBudget::default();
        for cycle in 0..=super::MAX_RECONNECT {
            let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
            if cycle < super::MAX_RECONNECT {
                assert!(!d.exhausted, "budget exhausted early, at cycle {cycle}");
            } else {
                assert!(
                    d.exhausted,
                    "MAX_RECONNECT+1 non-organic cycles must exhaust"
                );
            }
            // Every cycle serves the snapshot — connection machinery,
            // never a reset.
            budget.note_event(&types::build_event::Event::Snapshot(
                types::BuildSnapshot::default(),
            ));
            budget.note_event(&types::build_event::Event::ResyncRequired(
                types::ResyncRequired::default(),
            ));
        }
    }

    // r[verify gw.resync.reattach-budget+3]
    /// Organic events — and ONLY organic events — reset the budget.
    /// Exhaustive over the event alphabet so a new variant must be
    /// classified here as well as in `note_event` itself.
    #[test]
    fn budget_resets_only_on_organic_events() {
        use types::build_event::Event;
        let organic: Vec<Event> = vec![
            Event::Started(types::BuildStarted::default()),
            Event::Progress(types::BuildProgress::default()),
            Event::Derivation(types::DerivationEvent::default()),
            Event::Completed(types::BuildCompleted::default()),
            Event::Failed(types::BuildFailed::default()),
            Event::Cancelled(types::BuildCancelled::default()),
            Event::InputsResolved(types::BuildInputsResolved::default()),
            Event::Phase(types::BuildPhase::default()),
            Event::SubstituteProgress(types::SubstituteProgress::default()),
        ];
        for ev in &organic {
            let mut budget = super::ReattachBudget::default();
            budget.note_reattach();
            budget.note_reattach();
            budget.note_event(ev);
            assert_eq!(budget.attempts, 0, "organic event must reset: {ev:?}");
        }
        let machinery: Vec<Event> = vec![
            Event::Snapshot(types::BuildSnapshot::default()),
            Event::ResyncRequired(types::ResyncRequired::default()),
        ];
        for ev in &machinery {
            let mut budget = super::ReattachBudget::default();
            budget.note_reattach();
            budget.note_reattach();
            budget.note_event(ev);
            assert_eq!(budget.attempts, 2, "machinery event must NOT reset: {ev:?}");
        }
    }

    // r[verify gw.resync.reattach-budget+3]
    /// Two time-separated failovers: charges from a fully-recovered
    /// failover must not bleed into the next outage's budget. A dying
    /// stream whose `Live` tenure outlasted the backoff cap is
    /// recovery evidence — the next death opens a FRESH outage at
    /// attempt 1 (bug_068: pre-fix the budget was a lifetime counter
    /// across organic-quiet windows, so failover #2 of a long quiet
    /// compile exhausted mid-recovery).
    #[test]
    fn second_failover_after_long_live_tenure_gets_a_fresh_budget() {
        let mut budget = super::ReattachBudget::default();
        // Failover #1: stream death + four failed WatchBuild opens.
        budget.next_backoff(super::BackoffCause::Transport);
        for _ in 0..4 {
            budget.next_backoff(super::BackoffCause::ReattachCycleFailed);
        }
        // Re-attach succeeds; Live re-entered; an hour of healthy
        // stream with zero organic events (single-derivation compile
        // phase — logs ride the independent LogTailSet). The rate
        // bucket also fully refills across the hour.
        budget.note_live_entered();
        budget.backdate_live_since_for_test(std::time::Duration::from_secs(3600));
        budget.backdate_rate_window_for_test(std::time::Duration::from_secs(3600));
        // Failover #2: the first charge of a NEW outage.
        let d = budget.next_backoff(super::BackoffCause::Transport);
        assert_eq!(
            d.attempt, 1,
            "a failover after evidenced recovery (long Live tenure) must get a fresh budget"
        );
    }

    // r[verify gw.resync.reattach-budget+3]
    /// The merged_bug_056 storm bound survives the tenure reset: a
    /// storm cycles death→snapshot→Live in seconds, never accruing
    /// the Live tenure that evidences recovery, so its budget still
    /// exhausts.
    #[test]
    fn storm_with_short_live_tenure_still_exhausts() {
        let mut budget = super::ReattachBudget::default();
        for cycle in 0..=super::MAX_RECONNECT {
            // Each cycle re-enters Live for only an instant before
            // the next loss signal.
            budget.note_live_entered();
            let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
            if cycle < super::MAX_RECONNECT {
                assert!(!d.exhausted, "exhausted early at cycle {cycle}");
            } else {
                assert!(d.exhausted, "short-tenure storm cycles must still exhaust");
            }
        }
    }

    // r[verify gw.resync.reattach-budget+3]
    /// RED (bug_160): both resync bounds key on CONSECUTIVE
    /// non-organic cycles — a durably-slow consumer of a CHATTY build
    /// interleaves resync→snapshot→organic forever (organic progress
    /// zeroes the streak every cycle), looping resync at zero backoff
    /// while charging the scheduler one O(DAG) snapshot per cycle.
    /// The wall-clock rate window is the axis the interleave cannot
    /// reset: past RATE_MAX cycles per window, the ladder engages.
    #[test]
    fn interleaved_resync_storm_engages_the_ladder() {
        use types::build_event::Event;
        let mut budget = super::ReattachBudget::default();
        let mut paced = false;
        for _cycle in 0..12 {
            budget.note_live_entered();
            let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
            if d.rate_paced {
                assert!(!d.sleep.is_zero(), "a rate-paced decision must sleep");
                paced = true;
                break;
            }
            // The chatty build delivers an organic event inside the
            // same cycle — the streak axis resets to zero.
            budget.note_event(&Event::Progress(types::BuildProgress::default()));
        }
        assert!(
            paced,
            "12 resync cycles inside one rate window must engage the ladder \
             even though organic events reset the consecutive streak"
        );
    }

    // r[verify gw.resync.reattach-budget+3]
    /// The rate axis' negative space (the sweep over both bounds): a
    /// SLOW interleave — cycles spread wider than the window — never
    /// engages the rate ladder, however many total cycles accrue. An
    /// hour-long chatty build that genuinely reconnects once every
    /// few minutes stays at zero backoff; only sustained churn pays.
    #[test]
    fn slow_interleave_outside_the_window_never_rate_paces() {
        use types::build_event::Event;
        let mut budget = super::ReattachBudget::default();
        for _cycle in 0..40 {
            let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
            assert!(
                d.sleep.is_zero() && !d.rate_paced,
                "a cycle rate below RATE_MAX per window must stay immediate"
            );
            budget.note_event(&Event::Progress(types::BuildProgress::default()));
            // The next cycle arrives after the window has rolled (the
            // bucket refills fully across the gap).
            budget.backdate_rate_window_for_test(super::ReattachBudget::RATE_WINDOW);
        }
    }

    // r[verify gw.resync.reattach-budget+3]
    /// merged_bug_083 red #1: the rate axis must bound EVERY death
    /// arm, not just the resync signal. Mirror of
    /// `interleaved_resync_storm_engages_the_ladder`, charged through
    /// the FAILURE cause: 12 transport cycles inside one rate window,
    /// each followed by an organic `Progress` reset (the chatty
    /// build). The law: past the rate bound the reconnect ladder
    /// engages (sleep at least the first rung above zero-backoff)
    /// regardless of which death cause charged the cycle.
    #[test]
    fn transport_interleave_storm_engages_the_rate_ladder() {
        use types::build_event::Event;
        let mut budget = super::ReattachBudget::default();
        for cycle in 0..12u32 {
            budget.note_live_entered();
            let d = budget.next_backoff(super::BackoffCause::Transport);
            if cycle == 11 {
                assert!(
                    d.sleep >= std::time::Duration::from_secs(2),
                    "12 transport cycles inside one rate window must engage \
                     the rate ladder even though organic events reset the \
                     streak (failure-path sleep stuck at {:?}, want >= 2s \
                     -- the rate axis is bypassed on the failure arm)",
                    d.sleep
                );
                assert!(
                    d.rate_paced,
                    "the decision must carry the rate-axis tick (cause=transport)"
                );
            }
            // The chatty build delivers an organic event inside the
            // same cycle — the streak axis resets to zero.
            budget.note_event(&Event::Progress(types::BuildProgress::default()));
        }
    }

    // r[verify obs.gateway.stream-end-attributed]
    /// W14-F5: every `BackoffCause` face increments
    /// `rio_gateway_build_reattach_total` under its OWN cause label.
    /// Population is the closed enum — the test names every variant
    /// (no `_` arm) so a new cause without a label fails to compile,
    /// and the per-face counter assertion proves the chokepoint
    /// attributes every stream-end, not just the rate-paced subset.
    /// Pre-fix only `rio_gateway_build_resync_rate_paced_total`
    /// existed, so the live_064 benign-EOF vs transport split was
    /// reconstructible only from debug logs.
    #[test]
    fn every_reattach_cause_is_counted_under_its_own_label() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use std::collections::HashMap;

        let alphabet = [
            super::BackoffCause::ResyncSignal,
            super::BackoffCause::Transport,
            super::BackoffCause::EofWithoutTerminal,
            super::BackoffCause::ReattachCycleFailed,
        ];
        // R14: prove the alphabet above IS the enum — a match with no
        // wildcard fails to compile when a variant is added.
        for c in alphabet {
            match c {
                super::BackoffCause::ResyncSignal
                | super::BackoffCause::Transport
                | super::BackoffCause::EofWithoutTerminal
                | super::BackoffCause::ReattachCycleFailed => {}
            }
        }

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let mut budget = super::ReattachBudget::default();
            for cause in alphabet {
                budget.next_backoff(cause);
            }
        });

        // (ppppp): snapshot drains — call exactly once.
        let by_cause: HashMap<String, u64> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, _, _, _)| k.key().name() == "rio_gateway_build_reattach_total")
            .map(|(k, _, _, v)| {
                let cause = k
                    .key()
                    .labels()
                    .find(|l| l.key() == "cause")
                    .expect("cause label")
                    .value()
                    .to_string();
                let DebugValue::Counter(n) = v else {
                    panic!("counter")
                };
                (cause, n)
            })
            .collect();

        for cause in alphabet {
            assert_eq!(
                by_cause.get(cause.label()).copied(),
                Some(1),
                "cause={} must increment exactly once; got {by_cause:?}",
                cause.label()
            );
        }
    }

    // r[verify gw.resync.reattach-budget+3]
    /// merged_bug_083 red #2: pacing must not drain its own evidence.
    /// Sustained churn loop: charge, take the decision's sleep,
    /// advance the simulated clock by exactly that sleep, repeat.
    /// While the long-run cycle count exceeds what RATE_MAX per
    /// RATE_WINDOW admits (burst capacity + elapsed refill), every
    /// decision must stay paced — the pre-fix eviction window
    /// self-drains under its own pacing and re-enters zero backoff
    /// (the limit cycle).
    #[test]
    fn paced_cycles_cannot_drain_the_rate_evidence() {
        use types::build_event::Event;
        let mut budget = super::ReattachBudget::default();
        let mut cycles: u64 = 0;
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..40 {
            let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
            cycles += 1;
            // Admitted by the consts: one burst of RATE_MAX plus
            // RATE_MAX per fully elapsed window since the burst.
            let admitted = (super::ReattachBudget::RATE_MAX as u64)
                * (1 + elapsed.as_secs() / super::ReattachBudget::RATE_WINDOW.as_secs());
            if cycles > admitted {
                assert!(
                    d.sleep > std::time::Duration::ZERO,
                    "cycle {cycles} at t={elapsed:?}: the long-run cycle rate \
                     exceeds RATE_MAX per RATE_WINDOW (admitted {admitted}) yet \
                     the decision is immediate — paced cycles evicted the rate \
                     evidence (the limit-cycle re-entry)"
                );
            }
            // Advance the simulated clock by exactly the sleep the
            // decision demanded.
            budget.backdate_rate_window_for_test(d.sleep);
            elapsed += d.sleep;
            // Organic progress keeps the streak axis at zero.
            budget.note_event(&Event::Progress(types::BuildProgress::default()));
        }
    }

    // r[verify gw.resync.loss-signal+1]
    /// The resync ladder: zero backoff within ZERO_BACKOFF_STREAK
    /// consecutive non-organic cycles, the standard ladder past it.
    /// Failure-path cycles always pace.
    #[test]
    fn resync_backoff_ladders_past_the_streak() {
        let mut budget = super::ReattachBudget::default();
        for _ in 0..super::ReattachBudget::ZERO_BACKOFF_STREAK {
            let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
            assert!(d.sleep.is_zero(), "within streak: immediate");
        }
        let d = budget.next_backoff(super::BackoffCause::ResyncSignal); // 4th
        assert_eq!(
            d.sleep,
            std::time::Duration::from_secs(8),
            "4th cycle joins the ladder at its own rung (2^3)"
        );
        assert!(!d.rate_paced, "4 cycles stay inside the rate capacity");
        // The failure path pays the same rung at the same streak: a
        // sibling budget driven to the same attempts count.
        let mut sibling = super::ReattachBudget::default();
        for _ in 0..super::ReattachBudget::ZERO_BACKOFF_STREAK {
            sibling.next_backoff(super::BackoffCause::ResyncSignal);
        }
        let d = sibling.next_backoff(super::BackoffCause::Transport); // 4th
        assert_eq!(
            d.sleep,
            std::time::Duration::from_secs(8),
            "failure path paces at the same rung"
        );
        // An organic receipt restores zero-backoff resyncs (the
        // 5-cycle total stays inside the rate window's RATE_MAX, so
        // the second axis stays quiet here — the interleave test
        // covers its engagement).
        budget.note_event(&types::build_event::Event::Progress(
            types::BuildProgress::default(),
        ));
        let d = budget.next_backoff(super::BackoffCause::ResyncSignal);
        assert!(d.sleep.is_zero() && !d.rate_paced);
    }
}
