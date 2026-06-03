//! Client-side Nix worker protocol implementation.
//!
//! Speaks the Nix worker protocol as a **client**, for two consumers:
//! rio-builder driving a local `nix-daemon --stdio` inside the build
//! sandbox, and operator tooling driving a remote daemon endpoint over a
//! transport supplied by the caller (e.g. an SSH channel).
//!
//! Two expectations cut across the op functions here: callers bound each
//! operation with their own deadline (`tokio::time::timeout`), since a
//! silently stalled peer cannot be detected client-side; and after any error
//! from an op that had started writing a framed payload, the
//! channel/connection must be abandoned — the framed stream is left
//! incomplete and cannot be resynchronized.
//!
//! The client protocol mirrors the server protocol in `handshake.rs` and `stderr.rs`:
//! - Client sends `WORKER_MAGIC_1`, reads `WORKER_MAGIC_2` + server version
//! - Client reads the STDERR loop (log messages, activities, errors)
//! - Client sends opcodes and reads results

use super::build::{BuildMode, BuildResult, read_build_result, write_basic_derivation};
use super::handshake::{
    HandshakeError, HandshakeResult, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2,
};
use super::opcodes::WorkerOp;
use super::pathinfo::{ValidPathInfo, read_valid_path_info, write_valid_path_info};
use super::stderr::{
    ResultField, STDERR_ERROR, STDERR_LAST, STDERR_NEXT, STDERR_READ, STDERR_RESULT,
    STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_WRITE, StderrError,
};
use super::wire::{self, Result, WireError};
use crate::derivation::BasicDerivation;
use std::collections::BTreeSet;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A message received from the daemon during the STDERR loop.
#[derive(Debug)]
pub enum StderrMessage {
    /// Log message (STDERR_NEXT).
    Next(String),
    /// Server requests data from client (STDERR_READ). Contains the byte count.
    Read(u64),
    /// Server sends data to client (STDERR_WRITE).
    Write(Vec<u8>),
    /// End of STDERR loop (STDERR_LAST). Result follows on the stream.
    Last,
    /// Error from the daemon (STDERR_ERROR).
    Error(StderrError),
    /// Structured activity start (STDERR_START_ACTIVITY).
    StartActivity {
        id: u64,
        level: u64,
        activity_type: u64,
        text: String,
        fields: Vec<ResultField>,
        parent_id: u64,
    },
    /// Structured activity stop (STDERR_STOP_ACTIVITY).
    StopActivity { id: u64 },
    /// Structured result for an activity (STDERR_RESULT).
    Result {
        activity_id: u64,
        result_type: u64,
        fields: Vec<ResultField>,
    },
}

/// Read a single STDERR message from the daemon.
pub async fn read_stderr_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<StderrMessage> {
    let msg_type = wire::read_u64(r).await?;

    match msg_type {
        STDERR_NEXT => {
            let msg = wire::read_string(r).await?;
            Ok(StderrMessage::Next(msg))
        }
        STDERR_READ => {
            let count = wire::read_u64(r).await?;
            Ok(StderrMessage::Read(count))
        }
        STDERR_WRITE => {
            let data = wire::read_bytes(r).await?;
            Ok(StderrMessage::Write(data))
        }
        STDERR_LAST => Ok(StderrMessage::Last),
        STDERR_ERROR => {
            let error = read_stderr_error(r).await?;
            Ok(StderrMessage::Error(error))
        }
        STDERR_START_ACTIVITY => {
            let id = wire::read_u64(r).await?;
            let level = wire::read_u64(r).await?;
            let activity_type = wire::read_u64(r).await?;
            let text = wire::read_string(r).await?;
            let fields = read_result_fields(r).await?;
            let parent_id = wire::read_u64(r).await?;
            Ok(StderrMessage::StartActivity {
                id,
                level,
                activity_type,
                text,
                fields,
                parent_id,
            })
        }
        STDERR_STOP_ACTIVITY => {
            let id = wire::read_u64(r).await?;
            Ok(StderrMessage::StopActivity { id })
        }
        STDERR_RESULT => {
            let activity_id = wire::read_u64(r).await?;
            let result_type = wire::read_u64(r).await?;
            let fields = read_result_fields(r).await?;
            Ok(StderrMessage::Result {
                activity_id,
                result_type,
                fields,
            })
        }
        unknown => Err(WireError::Io(std::io::Error::other(format!(
            "unknown STDERR message type: {unknown:#x}"
        )))),
    }
}

/// Read the structured fields of STDERR_ERROR.
async fn read_stderr_error<R: AsyncRead + Unpin>(r: &mut R) -> Result<StderrError> {
    let error_type = wire::read_string(r).await?;
    let level = wire::read_u64(r).await?;
    let name = wire::read_string(r).await?;
    let message = wire::read_string(r).await?;

    // Position
    let have_pos = wire::read_u64(r).await?;
    let position = if have_pos != 0 {
        let file = wire::read_string(r).await?;
        let line = wire::read_u64(r).await?;
        let column = wire::read_u64(r).await?;
        Some(super::stderr::Position::new(file, line, column))
    } else {
        None
    };

    // Traces
    let trace_count = wire::read_u64(r).await?;
    if trace_count > wire::MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(trace_count));
    }
    let mut traces = Vec::with_capacity(trace_count.min(64) as usize);
    for _ in 0..trace_count {
        let trace_have_pos = wire::read_u64(r).await?;
        let trace_pos = if trace_have_pos != 0 {
            let file = wire::read_string(r).await?;
            let line = wire::read_u64(r).await?;
            let column = wire::read_u64(r).await?;
            Some(super::stderr::Position::new(file, line, column))
        } else {
            None
        };
        let trace_msg = wire::read_string(r).await?;
        traces.push(super::stderr::Trace::new(trace_pos, trace_msg));
    }

    Ok(StderrError::new(
        error_type, level, name, message, position, traces,
    ))
}

/// Read typed result fields (used by START_ACTIVITY and RESULT).
async fn read_result_fields<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<ResultField>> {
    let count = wire::read_u64(r).await?;
    if count > wire::MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(count));
    }
    let mut fields = Vec::with_capacity(count.min(64) as usize);
    for _ in 0..count {
        let field_type = wire::read_u64(r).await?;
        let field = match field_type {
            0 => ResultField::Int(wire::read_u64(r).await?),
            1 => ResultField::String(wire::read_string(r).await?),
            _ => {
                return Err(WireError::Io(std::io::Error::other(format!(
                    "unknown result field type: {field_type}"
                ))));
            }
        };
        fields.push(field);
    }
    Ok(fields)
}

/// Maximum STDERR messages to consume before aborting.
///
/// Prevents infinite loops from a buggy or malicious daemon that sends
/// an unbounded stream of log/activity messages without ever sending
/// STDERR_LAST.
const MAX_STDERR_MESSAGES: u64 = 100_000;

/// Drain the STDERR loop until STDERR_LAST, discarding all messages.
///
/// Returns `Ok(())` on STDERR_LAST, `Err` on STDERR_ERROR or I/O error.
/// Aborts after `MAX_STDERR_MESSAGES` to prevent infinite loops.
pub(crate) async fn drain_stderr<R: AsyncRead + Unpin>(r: &mut R) -> Result<()> {
    for _ in 0..MAX_STDERR_MESSAGES {
        match read_stderr_message(r).await? {
            StderrMessage::Last => return Ok(()),
            StderrMessage::Error(e) => {
                return Err(WireError::Io(std::io::Error::other(format!(
                    "daemon error: {}",
                    e.message
                ))));
            }
            StderrMessage::Read(_) => {
                return Err(WireError::Io(std::io::Error::other(
                    "unexpected STDERR_READ during drain",
                )));
            }
            _ => {} // discard log/activity messages
        }
    }
    Err(WireError::Io(std::io::Error::other(format!(
        "exceeded {MAX_STDERR_MESSAGES} STDERR messages without STDERR_LAST"
    ))))
}

/// Error from a client-side daemon operation.
///
/// [`Daemon`](ClientOpError::Daemon) is the daemon's verdict on the
/// operation, delivered as an STDERR_ERROR frame with protocol framing intact
/// (the transport is not corrupted) — callers may report it as a daemon-side
/// rejection or retry the operation. [`Wire`](ClientOpError::Wire) means the
/// channel itself is suspect and must be abandoned.
#[derive(Debug, thiserror::Error)]
pub enum ClientOpError {
    /// The daemon refused the operation (STDERR_ERROR frame).
    #[error("daemon refused operation: {}", .0.message)]
    Daemon(StderrError),
    /// Wire-level failure (I/O, framing, bounds).
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Per-ROOT ceiling on STDERR messages a build-op drain consumes.
///
/// Deliberately ~100× the legacy `MAX_STDERR_MESSAGES`: BuildPathsWithResults
/// streams every build-log line of every build it triggers through one
/// drain, so the cap must never fire on a legitimate long build. It exists
/// only to cut off a pathological/looping daemon that never sends
/// `STDERR_LAST`.
///
/// The calibration unit is one submitted ROOT (whose closure can
/// legitimately stream millions of lines), but the drain's consumption
/// scope is one whole op — and a batch submitter may pack many roots into
/// one op. [`stderr_budget_for_workload`] therefore scales the budget with
/// the op's workload (merged-closure node estimate, with
/// [`stderr_budget_for_roots`] as the floor): without that, a healthy
/// log-heavy batch — many roots, or one root with a large closure — trips
/// a cap calibrated for one build, the resulting Wire error mandates
/// channel abandonment, and every still-running build in the DAG is
/// cancelled. Single-unit ops ([`drain_stderr_typed`] /
/// [`drain_stderr_with_observer`] callers) consume exactly one unit of this
/// budget.
pub const MAX_BUILD_LOG_STDERR_MESSAGES: usize = 10_000_000;

/// Cap on the root-count multiplier in [`stderr_budget_for_roots`], so the
/// count belt stays a real bound on a runaway daemon instead of scaling
/// toward infinity with pathological root counts. 256 is >5× the largest
/// in-repo roots-per-op (the replay engine's default batch packing submits
/// up to 50 roots per op, pinned by a calibration test in that crate);
/// callers packing more roots than this per op get no additional count
/// headroom and should split the op or rely on their wall-clock deadline.
pub const STDERR_BUDGET_ROOT_MULTIPLIER_CAP: usize = 256;

/// STDERR message budget for one build op submitting `roots` derived
/// paths: [`MAX_BUILD_LOG_STDERR_MESSAGES`] per root, with the multiplier
/// clamped to `1..=`[`STDERR_BUDGET_ROOT_MULTIPLIER_CAP`].
///
/// This bounds MESSAGE COUNT only — wall-clock liveness remains the
/// caller's per-op deadline (a count cap cannot protect against a
/// silently stalled peer). The no-buffering claim is scoped to the DRAIN
/// itself: it never buffers the messages it counts, so the budget changes
/// no memory bound inside this module — but an observer fed by
/// [`drain_stderr_with_observer`] sees up to the full budget of lines,
/// and what it RETAINS per line is that consumer's own bound to declare
/// and enforce, not a property this budget grants. (The replay engine's
/// build observer, the one production observer today, caps its retained
/// captures — evidence-tail line count, capture-map cardinality and value
/// bytes — at its own parse boundary for exactly this reason.)
///
/// Root count is the workload's ARITY, not its size: build-op log volume
/// scales with the merged-closure node count (one frame per build-log line
/// per derivation the DAG builds), and a one-root op can legally carry a
/// many-thousand-node closure. This function is therefore only the FLOOR
/// of [`stderr_budget_for_workload`] — the budget every op keeps even with
/// no workload estimate.
pub fn stderr_budget_for_roots(roots: usize) -> usize {
    MAX_BUILD_LOG_STDERR_MESSAGES.saturating_mul(roots.clamp(1, STDERR_BUDGET_ROOT_MULTIPLIER_CAP))
}

/// Per-merged-closure-node STDERR allowance in
/// [`stderr_budget_for_workload`].
///
/// Calibration contract: the replay engine's cross-crate calibration test
/// (`default_batch_shape_fits_the_stderr_drain_budget` in that crate)
/// models a deliberately log-heavy but HEALTHY batch at 10,000 lines per
/// merged-closure node (~4.5× the mid-range from-source nixpkgs average of
/// ~2.2k lines/drv); this allowance is one decade above that model, so a
/// healthy closure of any legal shape never consumes more than a tenth of
/// its node-scaled budget. The belt exists for a daemon that streams
/// FOREVER, not for trimming heavy-but-finite logs.
pub const STDERR_BUDGET_PER_CLOSURE_NODE: usize = 100_000;

/// Cap on the closure-node multiplier in [`stderr_budget_for_workload`],
/// so the count belt stays a real bound on a runaway daemon instead of
/// scaling toward infinity with a pathological node estimate. 65,536 is
/// more than 7× the largest closure pinned by an in-repo batch-assembly
/// test (a 9,001-node oversized singleton) and more than 4× a
/// chromium/texlive-class nixpkgs closure; the estimate is computed
/// engine-side from the replay archive's embedded derivation texts (the
/// realized import-closure walk at the engine's submission chokepoint —
/// archive-controlled, not wire-peer-controlled), and a hostile or
/// corrupt archive embedding absurd reference chains buys at most this
/// multiplier. Ops estimating more nodes get no additional count headroom
/// and rely on their wall-clock deadline, exactly like the root cap
/// above.
pub const STDERR_BUDGET_NODE_MULTIPLIER_CAP: usize = 65_536;

/// STDERR message budget for one build op submitting `roots` derived
/// paths whose merged closure is estimated at `closure_nodes` derivations:
/// the larger of the per-root floor ([`stderr_budget_for_roots`]) and the
/// per-node allowance ([`STDERR_BUDGET_PER_CLOSURE_NODE`] ×
/// `closure_nodes`, multiplier clamped to
/// `1..=`[`STDERR_BUDGET_NODE_MULTIPLIER_CAP`]).
///
/// The budget is keyed on the quantity it bounds: BuildPathsWithResults
/// streams every triggered build's log lines through one drain, and that
/// volume scales with how many derivations the DAG builds — the merged-
/// closure node count — not with how many roots were submitted. Scaling by
/// roots alone leaves the few-roots/large-closure corner (oversized
/// singletons, fail-fast isolation singletons, wave-tail batches) at the
/// single-unit budget while its healthy volume is hundreds of times that.
///
/// `closure_nodes` provenance: an engine-side estimate the replay engine
/// derives at its submission chokepoint as the realized import closure of
/// the submitted roots (the archive's embedded-ATerm reference walk —
/// every batch producer funnels through that one derivation site, so no
/// producer-written estimate can under-key the budget). Callers with no
/// estimate pass `0`: the floor keeps them at exactly the roots-scaled
/// budget. The estimate can only RAISE the budget above the floor, never
/// lower it, so a too-small estimate degrades to the pre-existing roots
/// calibration rather than below it.
pub fn stderr_budget_for_workload(roots: usize, closure_nodes: usize) -> usize {
    stderr_budget_for_roots(roots).max(
        STDERR_BUDGET_PER_CLOSURE_NODE
            .saturating_mul(closure_nodes.clamp(1, STDERR_BUDGET_NODE_MULTIPLIER_CAP)),
    )
}

/// Drain the STDERR loop until `STDERR_LAST`, surfacing `STDERR_ERROR` as
/// [`ClientOpError::Daemon`]. Log lines, activities, and results are
/// discarded (build output is not collected by the callers of this path).
/// `STDERR_READ`/`STDERR_WRITE` are protocol violations for the operations
/// this is used with and map to [`ClientOpError::Wire`].
///
/// Unlike `drain_stderr` (handshake/SetOptions-scale traffic, 100k cap), this
/// drain accepts build-scale log/activity streams and is bounded only by the
/// very high `MAX_BUILD_LOG_STDERR_MESSAGES` ceiling. Callers are expected to
/// bound wall-clock time with a per-op deadline (e.g.
/// `tokio::time::timeout`), since a message-count cap cannot protect against
/// a silently stalled peer.
pub async fn drain_stderr_typed<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::result::Result<(), ClientOpError> {
    drain_stderr_with_observer(r, None).await
}

/// [`drain_stderr_typed`] with a log-line observer: `STDERR_NEXT` payloads
/// are passed to `observer` (when present) as display text — relayed daemon
/// log lines, never wire-parse data — instead of being discarded. The
/// observer cannot fail the drain. Bounds, error mapping, and the handling
/// of every other STDERR message kind are identical to
/// [`drain_stderr_typed`], including the caller-supplied-deadline
/// expectation.
pub async fn drain_stderr_with_observer<R: AsyncRead + Unpin>(
    r: &mut R,
    observer: Option<&mut (dyn FnMut(&str) + Send)>,
) -> std::result::Result<(), ClientOpError> {
    drain_stderr_typed_bounded(r, observer, MAX_BUILD_LOG_STDERR_MESSAGES).await
}

/// Single implementation behind [`drain_stderr_typed`] and
/// [`drain_stderr_with_observer`]: drain with an explicit message-count
/// ceiling, handing `STDERR_NEXT` payloads to `observer` when present.
/// Returns a [`ClientOpError::Wire`] I/O error if more than `max_messages`
/// messages arrive without `STDERR_LAST`.
async fn drain_stderr_typed_bounded<R: AsyncRead + Unpin>(
    r: &mut R,
    mut observer: Option<&mut (dyn FnMut(&str) + Send)>,
    max_messages: usize,
) -> std::result::Result<(), ClientOpError> {
    for _ in 0..max_messages {
        match read_stderr_message(r).await? {
            StderrMessage::Last => return Ok(()),
            StderrMessage::Error(e) => return Err(ClientOpError::Daemon(e)),
            StderrMessage::Read(count) => {
                return Err(ClientOpError::Wire(WireError::Io(std::io::Error::other(
                    format!("unexpected STDERR_READ (n={count}) during client operation"),
                ))));
            }
            StderrMessage::Write(data) => {
                return Err(ClientOpError::Wire(WireError::Io(std::io::Error::other(
                    format!(
                        "unexpected STDERR_WRITE (len={}) during client operation",
                        data.len()
                    ),
                ))));
            }
            StderrMessage::Next(line) => {
                if let Some(observer) = observer.as_mut() {
                    observer(&line);
                }
            }
            StderrMessage::StartActivity { .. }
            | StderrMessage::StopActivity { .. }
            | StderrMessage::Result { .. } => continue,
        }
    }
    Err(ClientOpError::Wire(WireError::Io(std::io::Error::other(
        format!("exceeded {max_messages} stderr messages without STDERR_LAST"),
    ))))
}

// ---------------------------------------------------------------------------
// Client handshake
// ---------------------------------------------------------------------------

/// Perform the client-side handshake with a `nix-daemon --stdio` process.
///
/// Mirror of `server_handshake_split` in `handshake.rs`.
pub async fn client_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> std::result::Result<HandshakeResult, HandshakeError> {
    // Phase 1: Magic + version exchange
    wire::write_u64(writer, WORKER_MAGIC_1).await?;
    writer.flush().await.map_err(WireError::Io)?;

    let server_magic = wire::read_u64(reader).await?;
    if server_magic != WORKER_MAGIC_2 {
        // The DAEMON's greeting failed validation — blame it as such.
        // `InvalidMagic` is the server-side variant: it renders "client
        // magic" and prints WORKER_MAGIC_1, both wrong from this side.
        return Err(HandshakeError::InvalidServerMagic(server_magic));
    }

    let server_version = wire::read_u64(reader).await?;
    if server_version < super::handshake::MIN_DAEMON_VERSION {
        // Mirror server_handshake's MIN_CLIENT_VERSION floor: an
        // older-than-expected daemon must fail HERE with a clear error,
        // not desync later at the first version-dependent read. The
        // stale version is the DAEMON's — `VersionTooOld` would render
        // it as the client's and send operators after the wrong side.
        let (daemon_major, daemon_minor) = super::handshake::decode_version(server_version);
        return Err(HandshakeError::DaemonVersionTooOld {
            daemon_major,
            daemon_minor,
        });
    }
    wire::write_u64(writer, PROTOCOL_VERSION).await?;
    writer.flush().await.map_err(WireError::Io)?;

    let negotiated_version = server_version.min(PROTOCOL_VERSION);

    // Phase 2: Feature exchange (protocol >= 1.38)
    if negotiated_version >= super::handshake::encode_version(1, 38) {
        // Send empty client features
        wire::write_strings(writer, wire::NO_STRINGS).await?;
        writer.flush().await.map_err(WireError::Io)?;
        // Read server features
        let _server_features = wire::read_strings(reader).await?;
    }

    // Phase 3: Post-handshake
    // Send CPU affinity = 0, reserveSpace = 0
    wire::write_u64(writer, 0).await?;
    wire::write_u64(writer, 0).await?;
    writer.flush().await.map_err(WireError::Io)?;

    // Read server version string + trusted status
    let _version_string = wire::read_string(reader).await?;
    let _trusted = wire::read_u64(reader).await?;

    // Phase 4: Read initial STDERR_LAST
    drain_stderr(reader).await?;

    Ok(HandshakeResult::new(negotiated_version))
}

// ---------------------------------------------------------------------------
// Client opcodes
// ---------------------------------------------------------------------------

/// Send `wopSetOptions` to the local daemon.
///
/// `max_silent_time`: seconds without build output before the daemon
/// kills the builder. 0 = unbounded (daemon default). Maps to Nix's
/// `--max-silent-time`.
///
/// `build_cores`: value of `NIX_BUILD_CORES` in the builder's env.
/// 0 = "use all cores" (daemon substitutes `nproc`). Maps to Nix's
/// `--cores`. The daemon sets this env var inside the builder's
/// sandbox namespace; setting `NIX_BUILD_CORES` on the daemon
/// PROCESS would be ignored — `wopSetOptions` is the correct channel.
// r[impl nix.client.set-options]
pub async fn client_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    max_silent_time: u64,
    build_cores: u64,
) -> Result<()> {
    wire::write_u64(writer, WorkerOp::SetOptions as u64).await?;

    // keepFailed, keepGoing, tryFallback
    wire::write_bool(writer, false).await?;
    wire::write_bool(writer, false).await?;
    wire::write_bool(writer, false).await?;
    // verbosity
    wire::write_u64(writer, 0).await?;
    // maxBuildJobs
    wire::write_u64(writer, 1).await?;
    // maxSilentTime
    wire::write_u64(writer, max_silent_time).await?;
    // obsolete_useBuildHook (always 1)
    wire::write_u64(writer, 1).await?;
    // verboseBuild — encoded as a Verbosity level, NOT a bool. Nix
    // daemon.cc decodes via `lvlError == readInt()`, so 0=true, 7=false.
    // Ref client encodes `(verboseBuild ? lvlError : lvlVomit)`.
    wire::write_u64(writer, super::stderr::verbosity::VOMIT).await?;
    // obsolete_logType, obsolete_printBuildTrace
    wire::write_u64(writer, 0).await?;
    wire::write_u64(writer, 0).await?;
    // buildCores
    wire::write_u64(writer, build_cores).await?;
    // useSubstitutes
    wire::write_bool(writer, false).await?;
    // overrides (empty)
    wire::write_string_pairs::<_, &str, &str>(writer, &[]).await?;

    writer.flush().await?;

    // nix-daemon sends only STDERR_LAST for SetOptions
    drain_stderr(reader).await?;

    Ok(())
}

/// Write the `wopBuildDerivation` request payload (opcode + path + drv + mode) and flush.
///
/// Read side is left to the caller: loop on [`read_stderr_message`] until
/// [`StderrMessage::Last`], then call [`read_build_result`].
/// rio-builder's `run_daemon_build` does this with cancel-safe batching and a
/// silence deadline layered on top of the protocol read.
pub async fn client_send_build_derivation<W: AsyncWrite + Unpin>(
    writer: &mut W,
    drv_path: &str,
    drv: &BasicDerivation,
    build_mode: BuildMode,
) -> Result<()> {
    wire::write_u64(writer, WorkerOp::BuildDerivation as u64).await?;
    wire::write_string(writer, drv_path).await?;
    write_basic_derivation(writer, drv).await?;
    wire::write_u64(writer, build_mode as u64).await?;
    writer.flush().await?;
    Ok(())
}

/// One entry of a `wopBuildPathsWithResults` (46) response.
#[derive(Debug, Clone)]
pub struct KeyedBuildResult {
    /// The derived path exactly as echoed by the daemon (submission order).
    pub derived_path: String,
    /// The daemon's build result for that derived path.
    pub result: BuildResult,
}

/// Send `wopBuildPathsWithResults` (46) with `buildMode = Normal`: build the
/// given derived paths (`"<drvpath>!<out1>,<out2>"`, `"<drvpath>!*"`, or an
/// opaque store path) and collect the daemon's per-path [`BuildResult`]s.
///
/// Build logs and activities arrive on the STDERR loop and are discarded by
/// the typed drain (use [`client_build_paths_with_results_observed`] to
/// receive the relayed log lines instead); after `STDERR_LAST` the daemon
/// replies with a count followed by one (echoed derived path,
/// [`BuildResult`]) entry per requested path, in submission order. Correlate
/// results with the submitted slice positionally (and check the returned
/// length matches) rather than parsing or matching the echoed string —
/// against non-rio daemons the echo may be a re-serialization of the parsed
/// path, not a byte-identical copy.
///
/// `negotiated_version` comes from [`client_handshake`]; it gates
/// version-dependent [`BuildResult`] fields (e.g. cpu stats at >= 1.37).
///
/// `closure_nodes` is the caller's estimate of how many derivations the
/// op's merged closure builds — the variable the stderr drain budget
/// scales with (see [`stderr_budget_for_workload`]). Pass `0` when no
/// estimate exists; the roots-scaled floor then applies.
pub async fn client_build_paths_with_results<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    derived_paths: &[S],
    negotiated_version: u64,
    closure_nodes: usize,
) -> std::result::Result<Vec<KeyedBuildResult>, ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsRef<str>,
{
    client_build_paths_with_results_inner(
        reader,
        writer,
        derived_paths,
        negotiated_version,
        closure_nodes,
        None,
    )
    .await
}

/// [`client_build_paths_with_results`] with a log-line observer: every
/// `STDERR_NEXT` payload the daemon relays while the build runs (e.g. the
/// gateway's `rio: build <uuid>` announcement and the relayed
/// `derivation '<drv>' failed:` lines) is passed to `observer` as display
/// text — never wire-parse data — and the observer cannot fail the
/// operation. Everything else is identical to the unobserved op: request
/// layout, response parsing, the caller-supplied-deadline expectation, and
/// the contract that the channel must be abandoned after any error from an
/// op that started writing a framed payload.
pub async fn client_build_paths_with_results_observed<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    derived_paths: &[S],
    negotiated_version: u64,
    closure_nodes: usize,
    observer: &mut (dyn FnMut(&str) + Send),
) -> std::result::Result<Vec<KeyedBuildResult>, ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsRef<str>,
{
    client_build_paths_with_results_inner(
        reader,
        writer,
        derived_paths,
        negotiated_version,
        closure_nodes,
        Some(observer),
    )
    .await
}

/// Single wire implementation behind [`client_build_paths_with_results`] and
/// [`client_build_paths_with_results_observed`]; the observer only changes
/// what happens to `STDERR_NEXT` payloads during the drain.
async fn client_build_paths_with_results_inner<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    derived_paths: &[S],
    negotiated_version: u64,
    closure_nodes: usize,
    observer: Option<&mut (dyn FnMut(&str) + Send)>,
) -> std::result::Result<Vec<KeyedBuildResult>, ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsRef<str>,
{
    wire::write_u64(writer, WorkerOp::BuildPathsWithResults as u64).await?;
    wire::write_strings(writer, derived_paths).await?;
    wire::write_u64(writer, BuildMode::Normal as u64).await?;
    writer.flush().await.map_err(WireError::Io)?;

    // The drain budget scales with the workload the op triggers: log
    // volume follows the merged-closure node count (every triggered
    // build's lines stream through this one drain), with the submitted
    // root count as the floor for callers without an estimate — a flat
    // per-build budget would be consumed at whole-op scope.
    drain_stderr_typed_bounded(
        reader,
        observer,
        stderr_budget_for_workload(derived_paths.len(), closure_nodes),
    )
    .await?;

    let count = wire::read_u64(reader).await?;
    if count > wire::MAX_COLLECTION_COUNT {
        return Err(ClientOpError::Wire(WireError::CollectionTooLarge(count)));
    }
    let mut results = Vec::with_capacity(count.min(64) as usize);
    for _ in 0..count {
        let derived_path = wire::read_string(reader).await?;
        let result = read_build_result(reader, negotiated_version).await?;
        results.push(KeyedBuildResult {
            derived_path,
            result,
        });
    }
    Ok(results)
}

/// Send `wopQueryValidPaths` (31): ask which of `paths` the daemon already
/// has.
///
/// `substitute = false` mirrors `nix copy` behaviour: answering the query
/// must not trigger target-side substitution. With `substitute = true` the
/// daemon may try to substitute missing paths from its configured
/// substituters before answering, so paths it can fetch count as valid.
///
/// The daemon replies (after the STDERR loop) with the subset of `paths` it
/// considers valid, returned as a [`BTreeSet`] for deduplication and cheap
/// membership checks against the queried chunk.
pub async fn client_query_valid_paths<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    paths: &[S],
    substitute: bool,
) -> std::result::Result<BTreeSet<String>, ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsRef<str>,
{
    wire::write_u64(writer, WorkerOp::QueryValidPaths as u64).await?;
    wire::write_strings(writer, paths).await?;
    wire::write_bool(writer, substitute).await?;
    writer.flush().await.map_err(WireError::Io)?;

    drain_stderr_typed(reader).await?;
    let valid = wire::read_strings(reader).await?;
    Ok(valid.into_iter().collect())
}

/// Send `wopQueryPathInfo` (26): fetch one path's [`ValidPathInfo`], `None`
/// if the daemon does not have the path.
///
/// The daemon replies (after the STDERR loop) with a found/not-found bool;
/// the [`ValidPathInfo`] body follows only when found.
pub async fn client_query_path_info<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    path: &str,
) -> std::result::Result<Option<ValidPathInfo>, ClientOpError> {
    wire::write_u64(writer, WorkerOp::QueryPathInfo as u64).await?;
    wire::write_string(writer, path).await?;
    writer.flush().await.map_err(WireError::Io)?;

    drain_stderr_typed(reader).await?;
    if !wire::read_bool(reader).await? {
        return Ok(None);
    }
    Ok(Some(read_valid_path_info(reader).await?))
}

// ---------------------------------------------------------------------------
// Store uploads (AddToStoreNar / AddMultipleToStore)
// ---------------------------------------------------------------------------

/// Read-chunk size for streaming [`NarPayload::Reader`] payloads into the
/// framed stream (64 KiB).
const NAR_COPY_CHUNK: usize = 64 * 1024;

/// NAR payload for an upload entry: in-memory bytes or a streaming reader of
/// exactly `len` bytes.
pub enum NarPayload {
    /// The whole NAR serialization, already in memory.
    Bytes(Vec<u8>),
    /// A streaming source of exactly `len` NAR bytes. Only the first `len`
    /// bytes are consumed (a longer source is never over-read); a source that
    /// ends before `len` bytes is a wire error.
    Reader {
        /// Exact number of bytes to take from `reader`.
        len: u64,
        /// The byte source.
        reader: Box<dyn AsyncRead + Send + Unpin>,
    },
}

impl NarPayload {
    /// Number of NAR bytes this payload puts on the wire.
    pub fn len(&self) -> u64 {
        match self {
            NarPayload::Bytes(bytes) => bytes.len() as u64,
            NarPayload::Reader { len, .. } => *len,
        }
    }

    /// Whether the payload is zero bytes long.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One path to upload: its store path, wire path-info, and NAR bytes.
///
/// `info.nar_size` MUST equal `nar.len()`; both upload ops check this before
/// writing the entry and refuse to start it otherwise (a mismatch would
/// desync the framed stream).
pub struct StoreEntry {
    /// Full store path being imported (e.g. `/nix/store/<hash>-<name>`).
    pub store_path: String,
    /// Wire path-info body sent ahead of the NAR bytes.
    pub info: ValidPathInfo,
    /// NAR serialization of the path contents.
    pub nar: NarPayload,
}

/// Enforce the `info.nar_size == nar.len()` precondition for one entry.
fn check_entry_nar_size(entry: &StoreEntry) -> Result<()> {
    let declared = entry.info.nar_size;
    let actual = entry.nar.len();
    if declared != actual {
        return Err(WireError::Io(std::io::Error::other(format!(
            "nar size mismatch for {}: info says {declared}, payload is {actual}",
            entry.store_path
        ))));
    }
    Ok(())
}

/// Stream one entry's NAR bytes through the framed writer.
///
/// `Bytes` payloads are handed to the framed writer whole (it slices them
/// into frames itself); `Reader` payloads are copied in [`NAR_COPY_CHUNK`]
/// pieces. Each read is capped to the bytes still owed, so a longer source
/// can never overrun the declared length; a source that ends early is a wire
/// error.
async fn write_nar_payload<W: AsyncWrite + Unpin>(
    framed: &mut wire::FramedWriter<W>,
    store_path: &str,
    nar: NarPayload,
) -> std::result::Result<(), ClientOpError> {
    match nar {
        NarPayload::Bytes(bytes) => framed.write(&bytes).await?,
        NarPayload::Reader { len, mut reader } => {
            let mut buf = vec![0u8; NAR_COPY_CHUNK];
            let mut remaining = len;
            while remaining > 0 {
                let cap = remaining.min(NAR_COPY_CHUNK as u64) as usize;
                let n = reader.read(&mut buf[..cap]).await.map_err(WireError::Io)?;
                if n == 0 {
                    return Err(ClientOpError::Wire(WireError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "NAR reader for {store_path} ended {remaining} bytes short of \
                             the declared {len}"
                        ),
                    ))));
                }
                framed.write(&buf[..n]).await?;
                remaining -= n as u64;
            }
        }
    }
    Ok(())
}

/// Send `wopAddToStoreNar` (39): import one store path together with its NAR
/// serialization.
///
/// Request layout, in the order rio-gateway's `handle_add_to_store_nar`
/// consumes it: opcode, store path, the 8-field [`ValidPathInfo`] body,
/// `repair`, `dont_check_sigs`, then the NAR bytes as a framed byte stream
/// (`u64(len) + data` chunks, `u64(0)` terminator). The response is the
/// STDERR loop alone — nothing follows `STDERR_LAST`.
///
/// `entry.info.nar_size` MUST equal `entry.nar.len()`: the framed stream has
/// to carry exactly the declared byte count, so the op refuses to start
/// (without writing anything) on a mismatch.
///
/// `dont_check_sigs` asks the daemon not to require signatures on the
/// uploaded path info. rio-gateway reads and ignores the flag (signature
/// policy is delegated to rio-store); a real nix-daemon skips its signature
/// check when a trusted client sets it.
///
/// # Concurrency and failure
///
/// The daemon may emit STDERR messages — including a refusal — while the
/// payload is still being written; if they were left unread, both sides could
/// deadlock on full buffers. The payload write and the STDERR drain therefore
/// run concurrently, and the first error cancels the other side. On
/// [`ClientOpError::Daemon`] (or any error after writing began) the framed
/// upload is left incomplete: the connection MUST be abandoned afterwards
/// (the same contract as [`wire::FramedWriter`]'s cancellation note). A
/// refusal can also race the daemon tearing down the session, so the failure
/// may surface as a [`Wire`](ClientOpError::Wire) I/O error instead of
/// [`Daemon`](ClientOpError::Daemon) — treat both variants as "upload
/// rejected/failed" rather than classifying on the variant alone.
///
/// As with [`drain_stderr_typed`], callers are expected to bound the call
/// with their own deadline (e.g. `tokio::time::timeout`): a silently stalled
/// peer cannot be detected client-side.
pub async fn client_add_to_store_nar<R, W>(
    reader: &mut R,
    writer: &mut W,
    entry: StoreEntry,
    repair: bool,
    dont_check_sigs: bool,
) -> std::result::Result<(), ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    check_entry_nar_size(&entry)?;
    let StoreEntry {
        store_path,
        info,
        nar,
    } = entry;

    let write_side = async {
        wire::write_u64(&mut *writer, WorkerOp::AddToStoreNar as u64).await?;
        wire::write_string(&mut *writer, &store_path).await?;
        write_valid_path_info(&mut *writer, &info).await?;
        wire::write_bool(&mut *writer, repair).await?;
        wire::write_bool(&mut *writer, dont_check_sigs).await?;

        let mut framed = wire::FramedWriter::new(&mut *writer);
        write_nar_payload(&mut framed, &store_path, nar).await?;
        let inner = framed.finish().await?;
        inner.flush().await.map_err(WireError::Io)?;
        Ok::<(), ClientOpError>(())
    };
    let drain_side = drain_stderr_typed(&mut *reader);

    tokio::try_join!(write_side, drain_side)?;
    Ok(())
}

/// Send `wopAddMultipleToStore` (44): import a batch of store paths in one
/// framed stream.
///
/// Request layout, in the order rio-gateway's `handle_add_multiple_to_store`
/// consumes it: opcode, `repair`, `dont_check_sigs`, then ONE framed byte
/// stream whose de-framed content is `u64(count)` followed by, per entry, the
/// store path string, the 8-field [`ValidPathInfo`] body, and exactly
/// `nar_size` raw NAR bytes (NOT nested-framed). Entry boundaries are
/// independent of frame boundaries. The response is the STDERR loop alone —
/// nothing follows `STDERR_LAST`.
///
/// Every entry's `info.nar_size` MUST equal its `nar.len()`; the batch
/// refuses to start (without writing anything) if any entry mismatches, since
/// a wrong length would desync every entry that follows in the framed stream.
///
/// Against a stock `nix-daemon`, entries should be sent in reference
/// (dependency) order — each entry's references registered before its
/// dependents; rio's gateway does not require any particular order, so
/// callers that plan uploads topologically satisfy both.
///
/// `dont_check_sigs` asks the daemon not to require signatures on the
/// uploaded path infos. rio-gateway reads and ignores the flag (signature
/// policy is delegated to rio-store); a real nix-daemon skips its signature
/// check when a trusted client sets it.
///
/// # Concurrency and failure
///
/// Same contract as [`client_add_to_store_nar`]: the payload write and the
/// STDERR drain run concurrently so a mid-upload refusal cannot deadlock on
/// full buffers, and the first error cancels the other side. On
/// [`ClientOpError::Daemon`] (or any error after writing began) the framed
/// stream is left incomplete and the connection MUST be abandoned. A refusal
/// can also race the daemon tearing down the session, so the failure may
/// surface as a [`Wire`](ClientOpError::Wire) I/O error instead of
/// [`Daemon`](ClientOpError::Daemon) — treat both variants as "upload
/// rejected/failed" rather than classifying on the variant alone.
///
/// As with [`drain_stderr_typed`], callers are expected to bound the call
/// with their own deadline (e.g. `tokio::time::timeout`): a silently stalled
/// peer cannot be detected client-side.
pub async fn client_add_multiple_to_store<R, W>(
    reader: &mut R,
    writer: &mut W,
    repair: bool,
    dont_check_sigs: bool,
    entries: Vec<StoreEntry>,
) -> std::result::Result<(), ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let count = entries.len() as u64;
    if count > wire::MAX_COLLECTION_COUNT {
        return Err(ClientOpError::Wire(WireError::CollectionTooLarge(count)));
    }
    for entry in &entries {
        check_entry_nar_size(entry)?;
    }

    let write_side = async {
        wire::write_u64(&mut *writer, WorkerOp::AddMultipleToStore as u64).await?;
        wire::write_bool(&mut *writer, repair).await?;
        wire::write_bool(&mut *writer, dont_check_sigs).await?;

        let mut framed = wire::FramedWriter::new(&mut *writer);
        // The count and per-entry metadata are encoded with the ordinary
        // wire/pathinfo helpers into a scratch buffer, then routed through
        // the framed writer — frames are cut wherever 256 KiB lands,
        // independent of entry boundaries.
        let mut head = Vec::new();
        wire::write_u64(&mut head, count).await?;
        framed.write(&head).await?;
        for entry in entries {
            let StoreEntry {
                store_path,
                info,
                nar,
            } = entry;
            head.clear();
            wire::write_string(&mut head, &store_path).await?;
            write_valid_path_info(&mut head, &info).await?;
            framed.write(&head).await?;
            write_nar_payload(&mut framed, &store_path, nar).await?;
        }
        let inner = framed.finish().await?;
        inner.flush().await.map_err(WireError::Io)?;
        Ok::<(), ClientOpError>(())
    };
    let drain_side = drain_stderr_typed(&mut *reader);

    tokio::try_join!(write_side, drain_side)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derivation::DerivationOutput;
    use crate::protocol::build::{
        BuildResult, BuildStatus, BuiltOutput, read_basic_derivation, read_build_result,
        write_build_result,
    };
    use crate::protocol::handshake::{MIN_CLIENT_VERSION, encode_version, server_handshake_split};
    use crate::protocol::pathinfo::write_valid_path_info;
    use crate::protocol::stderr::StderrWriter;

    fn test_drv() -> BasicDerivation {
        BasicDerivation::new(
            vec![DerivationOutput::new("out", "/nix/store/abc-hello", "", "").unwrap()],
            std::collections::BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec!["-c".into(), "true".into()],
            std::collections::BTreeMap::new(),
        )
        .unwrap()
    }

    /// Roundtrip the decomposed wopBuildDerivation client primitives the
    /// way rio-builder's `run_daemon_build` drives them:
    /// `client_send_build_derivation` → `read_stderr_message`* →
    /// `read_build_result`.
    // r[verify builder.daemon.stdio-client]
    #[tokio::test]
    async fn client_build_derivation_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let drv = test_drv();
        let drv_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello.drv";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::BuildDerivation as u64);
            let path = wire::read_string(&mut sr).await?;
            let received = read_basic_derivation(&mut sr).await?;
            let mode = wire::read_u64(&mut sr).await?;
            assert_eq!(mode, BuildMode::Normal as u64);

            wire::write_u64(&mut sw, STDERR_NEXT).await?;
            wire::write_string(&mut sw, "building...").await?;
            wire::write_u64(&mut sw, STDERR_NEXT).await?;
            wire::write_string(&mut sw, "done").await?;
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            write_build_result(
                &mut sw,
                &BuildResult {
                    status: BuildStatus::Built,
                    ..Default::default()
                },
                PROTOCOL_VERSION,
            )
            .await?;
            sw.flush().await?;
            anyhow::Ok((path, received))
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_send_build_derivation(&mut cw, drv_path, &drv, BuildMode::Normal).await?;

        let mut log_lines = Vec::new();
        let result = loop {
            match read_stderr_message(&mut cr).await? {
                StderrMessage::Last => break read_build_result(&mut cr, PROTOCOL_VERSION).await?,
                StderrMessage::Error(e) => panic!("daemon error: {}", e.message),
                StderrMessage::Next(s) => log_lines.push(s),
                _ => {}
            }
        };

        let (server_path, server_drv) = server.await??;
        assert_eq!(server_path, drv_path);
        assert_eq!(server_drv.platform(), drv.platform());
        assert_eq!(result.status, BuildStatus::Built);
        assert_eq!(log_lines, vec!["building...", "done"]);
        Ok(())
    }

    /// Test client handshake against our own server handshake.
    #[tokio::test]
    async fn client_handshake_against_server() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            server_handshake_split(&mut sr, &mut sw, "test-server 0.1.0").await
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let client_result = client_handshake(&mut cr, &mut cw).await?;

        let server_result = server_handle.await??;

        assert_eq!(
            client_result.negotiated_version(),
            server_result.negotiated_version()
        );
        assert!(client_result.negotiated_version() >= MIN_CLIENT_VERSION);
        Ok(())
    }

    /// Test client handshake with 1.37 server (no feature exchange).
    #[tokio::test]
    async fn client_handshake_v137_server() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_version = encode_version(1, 37);

        // Simulate a 1.37 server
        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);

            // Read client MAGIC_1
            let magic = wire::read_u64(&mut sr).await?;
            assert_eq!(magic, WORKER_MAGIC_1);

            // Send MAGIC_2 + version
            wire::write_u64(&mut sw, WORKER_MAGIC_2).await?;
            wire::write_u64(&mut sw, server_version).await?;
            sw.flush().await?;

            // Read client version
            let _client_version = wire::read_u64(&mut sr).await?;

            // NO feature exchange for 1.37

            // Read affinity + reserveSpace
            let _affinity = wire::read_u64(&mut sr).await?;
            let _reserve = wire::read_u64(&mut sr).await?;

            // Send version string + trusted
            wire::write_string(&mut sw, "test-daemon 1.37").await?;
            wire::write_u64(&mut sw, 1).await?;
            sw.flush().await?;

            // Send STDERR_LAST
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let result = client_handshake(&mut cr, &mut cw).await?;

        assert_eq!(result.negotiated_version(), server_version);
        server_handle.await??;
        Ok(())
    }

    /// A daemon advertising < `MIN_DAEMON_VERSION` is rejected at
    /// handshake. Mirror of `handshake::test_version_too_old_rejected`.
    /// Regression: previously `client_handshake` had no floor, so a
    /// 1.34 daemon negotiated successfully and then desynced at
    /// `read_build_result`'s `>=1.37` cpu-field read.
    #[tokio::test]
    async fn client_handshake_rejects_v134_daemon() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_version = encode_version(1, 34);

        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let _magic = wire::read_u64(&mut sr).await?;
            wire::write_u64(&mut sw, WORKER_MAGIC_2).await?;
            wire::write_u64(&mut sw, server_version).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let err = client_handshake(&mut cr, &mut cw)
            .await
            .expect_err("1.34 daemon must be rejected at handshake");
        assert!(
            matches!(
                err,
                HandshakeError::DaemonVersionTooOld {
                    daemon_major: 1,
                    daemon_minor: 34
                }
            ),
            "got: {err:?}"
        );
        // The too-old version is the DAEMON's: the rendered diagnosis
        // must say so, or operators upgrade the wrong component (the
        // client here is at PROTOCOL_VERSION, well above the floor).
        let msg = err.to_string();
        assert!(
            msg.contains("daemon protocol version 1.34"),
            "must attribute the stale version to the daemon: {msg}"
        );
        assert!(
            !msg.contains("client"),
            "must not blame the client for the daemon's version: {msg}"
        );
        server_handle.await??;
        Ok(())
    }

    /// Test reading STDERR messages.
    #[tokio::test]
    async fn read_stderr_messages() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let aid;
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("building foo").await?;
            aid = w
                .start_activity(
                    crate::protocol::stderr::ActivityType::Build,
                    "building",
                    0,
                    0,
                    &[],
                )
                .await?;
            w.stop_activity(aid).await?;
            w.finish().await?;
        }

        let mut reader = std::io::Cursor::new(buf);
        // Read NEXT
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::Next(ref s) if s == "building foo"));

        // Read START_ACTIVITY
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::StartActivity { id, .. } if id == aid));

        // Read STOP_ACTIVITY
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::StopActivity { id } if id == aid));

        // Read LAST
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::Last));
        Ok(())
    }

    /// Test reading STDERR_ERROR.
    #[tokio::test]
    async fn read_stderr_error_message() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.error(&StderrError::simple("test", "something failed"))
                .await?;
        }

        let mut reader = std::io::Cursor::new(buf);
        let msg = read_stderr_message(&mut reader).await?;
        match msg {
            StderrMessage::Error(e) => {
                assert_eq!(e.error_type, "Error");
                assert_eq!(e.name, "test");
                assert_eq!(e.message, "something failed");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // STDERR variant coverage: READ, WRITE, RESULT, unknown, bounds
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_read_stderr_read_variant() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 1024).await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        assert!(matches!(msg, StderrMessage::Read(1024)));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_write_variant() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_WRITE).await?;
        wire::write_bytes(&mut buf, b"payload").await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        assert!(matches!(msg, StderrMessage::Write(ref d) if d == b"payload"));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_result_variant_with_fields() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 42).await?; // activity_id
        wire::write_u64(&mut buf, 7).await?; // result_type
        wire::write_u64(&mut buf, 2).await?; // field count
        // Field 0: Int(123)
        wire::write_u64(&mut buf, 0).await?; // field_type = Int
        wire::write_u64(&mut buf, 123).await?;
        // Field 1: String("hi")
        wire::write_u64(&mut buf, 1).await?; // field_type = String
        wire::write_string(&mut buf, "hi").await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        match msg {
            StderrMessage::Result {
                activity_id,
                result_type,
                fields,
            } => {
                assert_eq!(activity_id, 42);
                assert_eq!(result_type, 7);
                assert_eq!(fields.len(), 2);
                assert!(matches!(fields[0], ResultField::Int(123)));
                assert!(matches!(&fields[1], ResultField::String(s) if s == "hi"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
        Ok(())
    }

    /// Full STDERR_ERROR with position + traces — exercises the have_pos
    /// branch and the trace-with-position loop in read_stderr_error.
    #[tokio::test]
    async fn test_read_stderr_error_with_position_and_traces() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_ERROR).await?;
        wire::write_string(&mut buf, "Error").await?; // error_type
        wire::write_u64(&mut buf, 1).await?; // level
        wire::write_string(&mut buf, "nix::EvalError").await?; // name
        wire::write_string(&mut buf, "undefined variable").await?; // message
        // Position: have_pos=1, file/line/column
        wire::write_u64(&mut buf, 1).await?;
        wire::write_string(&mut buf, "default.nix").await?;
        wire::write_u64(&mut buf, 10).await?;
        wire::write_u64(&mut buf, 5).await?;
        // Traces: count=2
        wire::write_u64(&mut buf, 2).await?;
        // Trace 0: with position
        wire::write_u64(&mut buf, 1).await?;
        wire::write_string(&mut buf, "lib.nix").await?;
        wire::write_u64(&mut buf, 3).await?;
        wire::write_u64(&mut buf, 1).await?;
        wire::write_string(&mut buf, "while calling").await?;
        // Trace 1: no position
        wire::write_u64(&mut buf, 0).await?;
        wire::write_string(&mut buf, "from CLI").await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        match msg {
            StderrMessage::Error(e) => {
                assert_eq!(e.message, "undefined variable");
                let pos = e.position.expect("should have position");
                assert_eq!(pos.file, "default.nix");
                assert_eq!(pos.line, 10);
                assert_eq!(pos.column, 5);
                assert_eq!(e.traces.len(), 2);
                assert!(e.traces[0].position.is_some());
                assert_eq!(e.traces[0].message, "while calling");
                assert!(e.traces[1].position.is_none());
                assert_eq!(e.traces[1].message, "from CLI");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_unknown_message_type() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, 0xDEADBEEF).await?;

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("unknown type should error");
        assert!(err.to_string().contains("unknown STDERR message type"));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_result_fields_unknown_field_type() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 1).await?; // activity_id
        wire::write_u64(&mut buf, 0).await?; // result_type
        wire::write_u64(&mut buf, 1).await?; // field count
        wire::write_u64(&mut buf, 99).await?; // field_type = invalid

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("unknown field type should error");
        assert!(err.to_string().contains("unknown result field type"));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_result_fields_too_large() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 1).await?;
        wire::write_u64(&mut buf, 0).await?;
        wire::write_u64(&mut buf, wire::MAX_COLLECTION_COUNT + 1).await?; // field count > max

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("oversized count should error");
        assert!(matches!(err, WireError::CollectionTooLarge(_)));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_error_traces_too_large() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_ERROR).await?;
        wire::write_string(&mut buf, "Error").await?;
        wire::write_u64(&mut buf, 0).await?; // level
        wire::write_string(&mut buf, "name").await?;
        wire::write_string(&mut buf, "msg").await?;
        wire::write_u64(&mut buf, 0).await?; // have_pos
        wire::write_u64(&mut buf, wire::MAX_COLLECTION_COUNT + 1).await?; // trace_count > max

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("oversized trace count should error");
        assert!(matches!(err, WireError::CollectionTooLarge(_)));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // drain_stderr error paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_drain_stderr_daemon_error_aborts() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("doing stuff").await?;
            w.error(&StderrError::simple("Build", "oh no")).await?;
        }

        let err = drain_stderr(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("drain should error on STDERR_ERROR");
        assert!(err.to_string().contains("daemon error"));
        assert!(err.to_string().contains("oh no"));
        Ok(())
    }

    #[tokio::test]
    async fn test_drain_stderr_unexpected_read_aborts() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 10).await?;

        let err = drain_stderr(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("STDERR_READ during drain should abort");
        assert!(
            err.to_string()
                .contains("unexpected STDERR_READ during drain")
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // drain_stderr_typed: daemon refusal vs wire failure
    // -----------------------------------------------------------------------

    /// A structured STDERR_ERROR from the daemon surfaces as
    /// `ClientOpError::Daemon` with the daemon's message text intact, after
    /// passing over a preceding log line.
    #[tokio::test]
    async fn drain_stderr_typed_surfaces_daemon_error() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (_sr, sw) = tokio::io::split(server_stream);
            let mut w = StderrWriter::new(sw);
            w.log("about to fail").await?;
            w.error(&StderrError::simple("rio-build", "tenant quota exceeded"))
                .await?;
            anyhow::Ok(())
        });

        let (mut cr, _cw) = tokio::io::split(client_stream);
        let err = drain_stderr_typed(&mut cr)
            .await
            .expect_err("STDERR_ERROR must surface as an error");
        match &err {
            ClientOpError::Daemon(e) => {
                assert_eq!(e.message, "tenant quota exceeded");
            }
            other => panic!("expected ClientOpError::Daemon, got {other:?}"),
        }
        // The daemon's text must survive into the human-readable Display.
        assert!(
            err.to_string().contains("tenant quota exceeded"),
            "Display must carry the daemon message: {err}"
        );
        server.await??;
        Ok(())
    }

    /// Activities, structured results, and log lines are discarded; the drain
    /// completes with `Ok(())` at STDERR_LAST.
    #[tokio::test]
    async fn drain_stderr_typed_passes_activities_and_stops_at_last() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (_sr, sw) = tokio::io::split(server_stream);
            let mut w = StderrWriter::new(sw);
            let aid = w
                .start_activity(
                    crate::protocol::stderr::ActivityType::Build,
                    "building foo",
                    0,
                    0,
                    &[],
                )
                .await?;
            w.result(
                aid,
                crate::protocol::stderr::ResultType::BuildLogLine,
                &[ResultField::String("checking for gcc... yes".into())],
            )
            .await?;
            w.stop_activity(aid).await?;
            w.finish().await?;
            anyhow::Ok(())
        });

        let (mut cr, _cw) = tokio::io::split(client_stream);
        drain_stderr_typed(&mut cr).await?;
        server.await??;
        Ok(())
    }

    /// STDERR_READ is a protocol violation for the operations this drain
    /// serves: it maps to `ClientOpError::Wire` and the message names the
    /// violating frame and its requested byte count.
    #[tokio::test]
    async fn drain_stderr_typed_read_frame_is_wire_violation() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 4096).await?;

        let err = drain_stderr_typed(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("STDERR_READ must abort the typed drain");
        assert!(matches!(err, ClientOpError::Wire(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("STDERR_READ"),
            "message must name the violating frame: {err}"
        );
        assert!(
            err.to_string().contains("4096"),
            "message must carry the requested byte count: {err}"
        );
        Ok(())
    }

    /// The message-count ceiling aborts a drain whose peer keeps streaming
    /// log lines without ever sending STDERR_LAST.
    #[tokio::test]
    async fn drain_stderr_typed_bounded_aborts_past_max_messages() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            for _ in 0..5 {
                w.log("still going").await?;
            }
        }

        let err = drain_stderr_typed_bounded(&mut std::io::Cursor::new(buf), None, 3)
            .await
            .expect_err("exceeding the message ceiling must abort the drain");
        assert!(matches!(err, ClientOpError::Wire(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("exceeded 3 stderr messages"),
            "message must mention the exceeded bound: {err}"
        );
        Ok(())
    }

    /// The build-op drain budget is the per-root ceiling times the op's
    /// root count, clamped to `1..=STDERR_BUDGET_ROOT_MULTIPLIER_CAP` —
    /// the per-build calibration documented on
    /// `MAX_BUILD_LOG_STDERR_MESSAGES` survives multi-root batch packing
    /// (the budget's consumption scope is the whole op), while the cap
    /// keeps the belt finite for pathological root counts. Universe: every
    /// `client_build_paths_with_results*` call derives its drain budget
    /// through `stderr_budget_for_workload`, whose floor this function is.
    #[test]
    fn stderr_budget_scales_per_root_with_a_capped_multiplier() {
        // Floor: zero/one root keep the single-build calibration.
        assert_eq!(stderr_budget_for_roots(0), MAX_BUILD_LOG_STDERR_MESSAGES);
        assert_eq!(stderr_budget_for_roots(1), MAX_BUILD_LOG_STDERR_MESSAGES);
        // Linear region: a 50-root batch op (the replay engine's default
        // packing) gets 50 builds' worth of headroom.
        assert_eq!(
            stderr_budget_for_roots(50),
            50 * MAX_BUILD_LOG_STDERR_MESSAGES
        );
        // Cap region: the multiplier saturates at the documented cap.
        assert_eq!(
            stderr_budget_for_roots(STDERR_BUDGET_ROOT_MULTIPLIER_CAP),
            STDERR_BUDGET_ROOT_MULTIPLIER_CAP * MAX_BUILD_LOG_STDERR_MESSAGES
        );
        assert_eq!(
            stderr_budget_for_roots(STDERR_BUDGET_ROOT_MULTIPLIER_CAP + 1_000),
            STDERR_BUDGET_ROOT_MULTIPLIER_CAP * MAX_BUILD_LOG_STDERR_MESSAGES
        );
    }

    /// The workload budget is keyed on the variable the drained volume
    /// actually follows — merged-closure nodes — with the roots budget as
    /// the floor. Universe: every `client_build_paths_with_results*` call
    /// derives its drain budget through this function; the corners come
    /// from the legal (roots, nodes) shapes the replay engine's batch
    /// assembler emits (its cross-crate calibration test enumerates them
    /// from the assembler itself), where roots and nodes are independent
    /// axes — budget(f(roots)) against volume(g(nodes)) must be evaluated
    /// where f is minimal and g maximal, not only on the diagonal.
    #[test]
    fn stderr_budget_for_workload_scales_with_nodes_above_the_roots_floor() {
        // No estimate (0) and tiny closures keep exactly the roots floor:
        // non-replay single-unit callers are unchanged.
        assert_eq!(stderr_budget_for_workload(1, 0), stderr_budget_for_roots(1));
        assert_eq!(stderr_budget_for_workload(1, 1), stderr_budget_for_roots(1));
        assert_eq!(
            stderr_budget_for_workload(50, 0),
            stderr_budget_for_roots(50)
        );
        // The binding corner the roots key missed: ONE root with a large
        // closure gets a node-scaled budget, not the single-unit floor.
        assert_eq!(
            stderr_budget_for_workload(1, 4_500),
            4_500 * STDERR_BUDGET_PER_CLOSURE_NODE
        );
        assert!(
            stderr_budget_for_workload(1, 4_500) > stderr_budget_for_roots(1),
            "a 1-root/4500-node op must not be budgeted as a single unit"
        );
        // The floor wins whenever it is larger: many roots with a modest
        // shared closure never drop below the per-root calibration.
        assert_eq!(
            stderr_budget_for_workload(50, 100),
            stderr_budget_for_roots(50)
        );
        // Hostile/corrupt magnitudes CLAMP — they must not scale. The
        // node estimate is archive-derived (a corrupt or hostile archive
        // can declare absurd dependency lists), so the absurd inputs
        // assert the cap explicitly rather than being assumed
        // unreachable.
        for absurd in [STDERR_BUDGET_NODE_MULTIPLIER_CAP + 1, usize::MAX] {
            assert_eq!(
                stderr_budget_for_workload(1, absurd),
                STDERR_BUDGET_NODE_MULTIPLIER_CAP * STDERR_BUDGET_PER_CLOSURE_NODE,
                "an absurd node estimate ({absurd}) must clamp at the node multiplier cap"
            );
        }
    }

    // -----------------------------------------------------------------------
    // client_set_options + client_handshake error path
    // -----------------------------------------------------------------------

    /// client_set_options wire layout round-trip. Field order and values
    /// per Nix src/libstore/daemon.cc case SetOptions.
    // r[verify nix.client.set-options]
    #[tokio::test]
    async fn test_client_set_options_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            // Opcode
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::SetOptions as u64);
            // 13 fields in the exact order the client sends them
            let keep_failed = wire::read_bool(&mut sr).await?;
            let keep_going = wire::read_bool(&mut sr).await?;
            let try_fallback = wire::read_bool(&mut sr).await?;
            assert!(!keep_failed && !keep_going && !try_fallback);
            let verbosity = wire::read_u64(&mut sr).await?;
            assert_eq!(verbosity, 0);
            let max_build_jobs = wire::read_u64(&mut sr).await?;
            assert_eq!(max_build_jobs, 1);
            let max_silent_time = wire::read_u64(&mut sr).await?;
            assert_eq!(max_silent_time, 0, "zero → unbounded (daemon default)");
            let obsolete_use_build_hook = wire::read_u64(&mut sr).await?;
            assert_eq!(obsolete_use_build_hook, 1);
            // Nix decodes via `lvlError == readInt()`; 7 (lvlVomit) = false.
            let verbose_build = wire::read_u64(&mut sr).await?;
            assert_eq!(verbose_build, super::super::stderr::verbosity::VOMIT);
            let _obsolete_log_type = wire::read_u64(&mut sr).await?;
            let _obsolete_print_build_trace = wire::read_u64(&mut sr).await?;
            let build_cores = wire::read_u64(&mut sr).await?;
            assert_eq!(build_cores, 0, "zero → daemon substitutes nproc");
            let use_substitutes = wire::read_bool(&mut sr).await?;
            assert!(!use_substitutes);
            let overrides = wire::read_string_pairs(&mut sr).await?;
            assert!(overrides.is_empty());
            // Send STDERR_LAST to unblock the client's drain_stderr.
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_set_options(&mut cr, &mut cw, 0, 0).await?;
        server.await??;
        Ok(())
    }

    /// Nonzero `max_silent_time` / `build_cores` appear at the correct
    /// wire positions. Field positions per Nix `src/libstore/daemon.cc`
    /// case SetOptions — same layout as the roundtrip test above, but
    /// asserts the params aren't hardcoded-0 (the phase4a P1 plumbing
    /// gap: scheduler sent per-tenant limits, worker ignored them).
    #[tokio::test]
    async fn test_client_set_options_plumbs_values() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::SetOptions as u64);
            // keepFailed, keepGoing, tryFallback, verbosity, maxBuildJobs
            wire::read_bool(&mut sr).await?;
            wire::read_bool(&mut sr).await?;
            wire::read_bool(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            // maxSilentTime — position 6
            let mst = wire::read_u64(&mut sr).await?;
            assert_eq!(mst, 3600, "max_silent_time must reach the wire");
            // useBuildHook, verboseBuild, logType, printBuildTrace
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            // buildCores — position 11
            let bc = wire::read_u64(&mut sr).await?;
            assert_eq!(bc, 4, "build_cores must reach the wire");
            // useSubstitutes, overrides
            wire::read_bool(&mut sr).await?;
            wire::read_string_pairs(&mut sr).await?;
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_set_options(&mut cr, &mut cw, 3600, 4).await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn test_client_handshake_bad_magic() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(64);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            // Read client MAGIC_1 then send WRONG magic.
            let _m1 = wire::read_u64(&mut sr).await?;
            wire::write_u64(&mut sw, 0xBADC0FFE).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let err = client_handshake(&mut cr, &mut cw)
            .await
            .expect_err("bad magic should fail handshake");
        assert!(
            matches!(err, HandshakeError::InvalidServerMagic(0xBADC0FFE)),
            "got: {err:?}"
        );
        // The DAEMON's greeting failed the WORKER_MAGIC_2 check: the
        // rendered diagnosis must blame the daemon and print the
        // constant the client actually compared against.
        let msg = err.to_string();
        assert!(
            msg.contains("daemon magic"),
            "must attribute the bad greeting to the daemon: {msg}"
        );
        assert!(
            msg.contains(&format!("{WORKER_MAGIC_2:#018x}")),
            "must print the WORKER_MAGIC_2 value the client checked: {msg}"
        );
        assert!(
            msg.contains(&format!("{0:#018x}", 0xBADC0FFE_u64)),
            "must echo the daemon's actual greeting: {msg}"
        );
        server.await??;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // client_query_valid_paths / client_query_path_info
    // -----------------------------------------------------------------------

    /// client_query_valid_paths wire layout round-trip: the request carries
    /// the opcode, the path collection, and the substitute flag in the order
    /// the gateway's `handle_query_valid_paths` reads them; the response
    /// (after STDERR_LAST) is the daemon's collection of valid paths.
    #[tokio::test]
    async fn client_query_valid_paths_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let path_a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello";
        let path_b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryValidPaths as u64);
            let paths = wire::read_strings(&mut sr).await?;
            assert_eq!(paths, vec![path_a.to_string(), path_b.to_string()]);
            let substitute = wire::read_bool(&mut sr).await?;
            assert!(!substitute, "substitute flag must reach the wire as false");
            // Response: STDERR_LAST, then the valid subset (only path_a).
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_strings(&mut sw, &[path_a]).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let valid = client_query_valid_paths(&mut cr, &mut cw, &[path_a, path_b], false).await?;
        server.await??;
        assert_eq!(valid, BTreeSet::from([path_a.to_string()]));
        Ok(())
    }

    /// Two sequential client_query_path_info calls on one connection: a
    /// found path yields `Some(ValidPathInfo)`, a missing path yields
    /// `None`. Found/not-found is the bool the gateway's
    /// `handle_query_path_info` writes after STDERR_LAST, with the
    /// `ValidPathInfo` body following only when found.
    #[tokio::test]
    async fn client_query_path_info_found_and_missing() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let found_path = "/nix/store/cccccccccccccccccccccccccccccccc-found";
        let missing_path = "/nix/store/dddddddddddddddddddddddddddddddd-missing";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);

            // Call 1: path present — bool(true) + ValidPathInfo body.
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryPathInfo as u64);
            let path = wire::read_string(&mut sr).await?;
            assert_eq!(path, found_path);
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_bool(&mut sw, true).await?;
            write_valid_path_info(
                &mut sw,
                &ValidPathInfo {
                    nar_hash: vec![0xab; 32],
                    nar_size: 123,
                    ..Default::default()
                },
            )
            .await?;
            sw.flush().await?;

            // Call 2: path missing — bool(false), nothing else.
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryPathInfo as u64);
            let path = wire::read_string(&mut sr).await?;
            assert_eq!(path, missing_path);
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_bool(&mut sw, false).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let info = client_query_path_info(&mut cr, &mut cw, found_path).await?;
        assert_eq!(info.expect("path should be found").nar_size, 123);
        let missing = client_query_path_info(&mut cr, &mut cw, missing_path).await?;
        assert!(missing.is_none(), "missing path must map to None");
        server.await??;
        Ok(())
    }

    /// `substitute: true` is plumbed to the wire: the fake server reads the
    /// flag after the path collection and asserts it arrives as true.
    #[tokio::test]
    async fn client_query_valid_paths_substitute_true_plumbed() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let path = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-subst";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryValidPaths as u64);
            let paths = wire::read_strings(&mut sr).await?;
            assert_eq!(paths, vec![path.to_string()]);
            let substitute = wire::read_bool(&mut sr).await?;
            assert!(substitute, "substitute flag must reach the wire as true");
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_strings(&mut sw, &[path]).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let valid = client_query_valid_paths(&mut cr, &mut cw, &[path], true).await?;
        server.await??;
        assert_eq!(valid, BTreeSet::from([path.to_string()]));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // client_build_paths_with_results
    // -----------------------------------------------------------------------

    /// client_build_paths_with_results wire layout round-trip: the request
    /// carries the opcode, the derived-path collection, and buildMode Normal
    /// in the order the gateway's `handle_build_paths_with_results` reads
    /// them; the response (after STDERR_LAST) is a count followed by
    /// per-entry (derived path echo, BuildResult) pairs whose statuses,
    /// error message, and built outputs must be preserved.
    #[tokio::test]
    async fn client_build_paths_with_results_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let dp_all = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv!*";
        let dp_out = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y.drv!out";
        let built_out = BuiltOutput {
            drv_output_id: "sha256:abcdef0123456789!out".to_string(),
            out_path: "/nix/store/cccccccccccccccccccccccccccccccc-x".to_string(),
        };

        let server = tokio::spawn({
            let built_out = built_out.clone();
            async move {
                let (mut sr, mut sw) = tokio::io::split(server_stream);
                let op = wire::read_u64(&mut sr).await?;
                assert_eq!(op, WorkerOp::BuildPathsWithResults as u64);
                let paths = wire::read_strings(&mut sr).await?;
                assert_eq!(paths, vec![dp_all.to_string(), dp_out.to_string()]);
                let mode = wire::read_u64(&mut sr).await?;
                assert_eq!(mode, BuildMode::Normal as u64);

                // Build logs/activities stream on the STDERR loop first.
                wire::write_u64(&mut sw, STDERR_NEXT).await?;
                wire::write_string(&mut sw, "building x...").await?;
                wire::write_u64(&mut sw, STDERR_LAST).await?;
                // Then: count, and per entry the echoed derived path + result.
                wire::write_u64(&mut sw, 2).await?;
                wire::write_string(&mut sw, dp_all).await?;
                write_build_result(
                    &mut sw,
                    &BuildResult {
                        status: BuildStatus::Built,
                        times_built: 1,
                        built_outputs: vec![built_out],
                        ..Default::default()
                    },
                    PROTOCOL_VERSION,
                )
                .await?;
                wire::write_string(&mut sw, dp_out).await?;
                write_build_result(
                    &mut sw,
                    &BuildResult::failure(BuildStatus::PermanentFailure, "boom"),
                    PROTOCOL_VERSION,
                )
                .await?;
                sw.flush().await?;
                anyhow::Ok(())
            }
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let results = client_build_paths_with_results(
            &mut cr,
            &mut cw,
            &[dp_all, dp_out],
            PROTOCOL_VERSION,
            0,
        )
        .await?;
        server.await??;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].derived_path, dp_all);
        assert_eq!(results[0].result.status, BuildStatus::Built);
        assert_eq!(
            results[0].result.built_outputs,
            vec![built_out],
            "built output of the successful entry must be preserved"
        );
        assert_eq!(results[1].derived_path, dp_out);
        assert_eq!(results[1].result.status, BuildStatus::PermanentFailure);
        assert_eq!(results[1].result.error_msg, "boom");
        Ok(())
    }

    /// A response count above `MAX_COLLECTION_COUNT` is rejected as a wire
    /// error before any per-entry reads (bounds check on the count prefix).
    #[tokio::test]
    async fn client_build_paths_with_results_count_too_large_is_wire_error() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let dp = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv!*";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let _op = wire::read_u64(&mut sr).await?;
            let _paths = wire::read_strings(&mut sr).await?;
            let _mode = wire::read_u64(&mut sr).await?;
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_u64(&mut sw, wire::MAX_COLLECTION_COUNT + 1).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let err = client_build_paths_with_results(&mut cr, &mut cw, &[dp], PROTOCOL_VERSION, 0)
            .await
            .expect_err("oversized result count must be rejected");
        assert!(
            matches!(err, ClientOpError::Wire(WireError::CollectionTooLarge(_))),
            "got: {err:?}"
        );
        server.await??;
        Ok(())
    }

    /// An empty request is valid: the daemon sees an empty derived-path
    /// collection and the client maps a zero-count response to an empty vec.
    #[tokio::test]
    async fn client_build_paths_with_results_empty_request_ok() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::BuildPathsWithResults as u64);
            let paths = wire::read_strings(&mut sr).await?;
            assert!(
                paths.is_empty(),
                "empty request must reach the wire as an empty collection"
            );
            let mode = wire::read_u64(&mut sr).await?;
            assert_eq!(mode, BuildMode::Normal as u64);
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_u64(&mut sw, 0).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let results = client_build_paths_with_results(
            &mut cr,
            &mut cw,
            wire::NO_STRINGS,
            PROTOCOL_VERSION,
            0,
        )
        .await?;
        assert!(results.is_empty());
        server.await??;
        Ok(())
    }

    /// The observed variant hands every relayed `STDERR_NEXT` log line (the
    /// gateway's `rio: build <uuid>` announcement, the relayed
    /// `derivation '<drv>' failed:` lines) to the caller's observer verbatim,
    /// and parses the same response bytes into the same results as the
    /// unobserved op.
    #[tokio::test]
    async fn client_build_paths_with_results_observed_captures_log_lines() -> anyhow::Result<()> {
        let dp = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv!*";
        let build_id_line = "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";
        let failed_line =
            "derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv' failed: builder failed";

        // The same scripted server bytes back both the observed and the
        // unobserved op so their parsed results can be compared directly.
        let spawn_server = |server_stream: tokio::io::DuplexStream| {
            tokio::spawn(async move {
                let (mut sr, mut sw) = tokio::io::split(server_stream);
                let op = wire::read_u64(&mut sr).await?;
                assert_eq!(op, WorkerOp::BuildPathsWithResults as u64);
                let paths = wire::read_strings(&mut sr).await?;
                assert_eq!(paths, vec![dp.to_string()]);
                let mode = wire::read_u64(&mut sr).await?;
                assert_eq!(mode, BuildMode::Normal as u64);

                // Relayed build-log lines stream on the STDERR loop first.
                wire::write_u64(&mut sw, STDERR_NEXT).await?;
                wire::write_string(&mut sw, build_id_line).await?;
                wire::write_u64(&mut sw, STDERR_NEXT).await?;
                wire::write_string(&mut sw, failed_line).await?;
                wire::write_u64(&mut sw, STDERR_LAST).await?;
                // Then the one-entry result list.
                wire::write_u64(&mut sw, 1).await?;
                wire::write_string(&mut sw, dp).await?;
                write_build_result(
                    &mut sw,
                    &BuildResult::failure(BuildStatus::PermanentFailure, "builder failed"),
                    PROTOCOL_VERSION,
                )
                .await?;
                sw.flush().await?;
                anyhow::Ok(())
            })
        };

        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server = spawn_server(server_stream);
        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let mut seen: Vec<String> = Vec::new();
        let observed = client_build_paths_with_results_observed(
            &mut cr,
            &mut cw,
            &[dp],
            PROTOCOL_VERSION,
            0,
            &mut |line| seen.push(line.to_string()),
        )
        .await?;
        server.await??;

        assert_eq!(
            seen,
            vec![build_id_line.to_string(), failed_line.to_string()],
            "observer must receive every relayed log line verbatim"
        );

        // Same bytes through the unobserved op: identical parsed results.
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server = spawn_server(server_stream);
        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let unobserved =
            client_build_paths_with_results(&mut cr, &mut cw, &[dp], PROTOCOL_VERSION, 0).await?;
        server.await??;

        assert_eq!(observed.len(), unobserved.len());
        assert_eq!(observed[0].derived_path, unobserved[0].derived_path);
        assert_eq!(
            observed[0].result, unobserved[0].result,
            "observation must not change how the response bytes are parsed"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // client_add_to_store_nar / client_add_multiple_to_store
    // -----------------------------------------------------------------------

    /// client_add_to_store_nar wire layout round-trip with a streaming
    /// (`NarPayload::Reader`) payload large enough to span multiple 256 KiB
    /// frames: the fake server reads the metadata head exactly as the
    /// gateway's `handle_add_to_store_nar` does (path, ValidPathInfo body,
    /// repair, dontCheckSigs), then de-frames the NAR through
    /// `FramedStreamReader` and must recover the original bytes; the client
    /// completes with Ok(()) once STDERR_LAST arrives.
    #[tokio::test]
    async fn client_add_to_store_nar_streams_framed_payload() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let store_path = "/nix/store/ffffffffffffffffffffffffffffffff-uploaded";
        let payload: Vec<u8> = (0..600 * 1024).map(|i| (i % 251) as u8).collect();
        let payload_len = payload.len() as u64;

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::AddToStoreNar as u64);
            let path = wire::read_string(&mut sr).await?;
            assert_eq!(path, store_path);
            let info = read_valid_path_info(&mut sr).await?;
            assert_eq!(info.nar_size, payload_len);
            assert_eq!(info.nar_hash, vec![0xab; 32]);
            let repair = wire::read_bool(&mut sr).await?;
            assert!(!repair, "repair flag must reach the wire as false");
            let dont_check_sigs = wire::read_bool(&mut sr).await?;
            assert!(dont_check_sigs, "dontCheckSigs must reach the wire as true");
            // The NAR arrives as a framed stream; de-frame it through the
            // same reader the gateway uses.
            let mut framed = wire::FramedStreamReader::new(&mut sr, info.nar_size);
            let mut nar = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut framed, &mut nar).await?;
            // Acknowledge only after the payload is fully consumed.
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(nar)
        });

        let entry = StoreEntry {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                nar_hash: vec![0xab; 32],
                nar_size: payload_len,
                ..Default::default()
            },
            nar: NarPayload::Reader {
                len: payload_len,
                reader: Box::new(std::io::Cursor::new(payload.clone())),
            },
        };
        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_add_to_store_nar(&mut cr, &mut cw, entry, false, true).await?;

        let received = server.await??;
        assert_eq!(
            received, payload,
            "de-framed NAR must equal the original payload"
        );
        Ok(())
    }

    /// A streaming (`NarPayload::Reader`) source that ends before its declared
    /// `len` is a prompt `ClientOpError::Wire` failure: the local short read
    /// must be reported without waiting on the (silent) daemon. The fake
    /// server reads only the op-39 header, then parks on a oneshot without
    /// writing anything and without reading the framed payload — as in the
    /// refusal test, the duplex buffer is far smaller than the declared
    /// payload. The bytes the source DOES yield are kept under one 256 KiB
    /// frame so the short read is hit before any frame write would need the
    /// non-reading peer to drain the transport; once a frame is in flight,
    /// backpressure from a stalled peer is the caller-deadline's job per the
    /// module docs.
    #[tokio::test]
    async fn client_add_to_store_nar_reader_short_source_is_prompt_wire_error() -> anyhow::Result<()>
    {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let store_path = "/nix/store/gggggggggggggggggggggggggggggggg-short";
        let declared_len = 1024 * 1024u64; // 1 MiB declared…
        let provided = vec![0x77u8; 192 * 1024]; // …but the source ends early.
        // Held until the client call returns so the server halves stay open:
        // the failure must come from the short source, not a broken pipe.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut sr, _sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::AddToStoreNar as u64);
            let path = wire::read_string(&mut sr).await?;
            assert_eq!(path, store_path);
            let info = read_valid_path_info(&mut sr).await?;
            assert_eq!(info.nar_size, declared_len);
            let _repair = wire::read_bool(&mut sr).await?;
            let _dont_check_sigs = wire::read_bool(&mut sr).await?;
            // Stay silent: no STDERR frames, no reads of the framed payload.
            let _ = done_rx.await;
            anyhow::Ok(())
        });

        let entry = StoreEntry {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                nar_hash: vec![0xcd; 32],
                nar_size: declared_len,
                ..Default::default()
            },
            nar: NarPayload::Reader {
                len: declared_len,
                reader: Box::new(std::io::Cursor::new(provided)),
            },
        };

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_add_to_store_nar(&mut cr, &mut cw, entry, false, true),
        )
        .await
        .expect("short source must fail promptly, not hang on the silent daemon");

        let err = result.expect_err("short source must surface as an error");
        assert!(matches!(err, ClientOpError::Wire(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("bytes short of the declared"),
            "message must say the payload ended short: {err}"
        );
        drop(done_tx);
        server.await??;
        Ok(())
    }

    /// An entry whose `info.nar_size` disagrees with its payload length is
    /// rejected by the upfront size check: the error names the mismatch and
    /// nothing — not even the opcode — reaches the wire.
    #[tokio::test]
    async fn client_add_to_store_nar_size_mismatch_rejected_before_any_write() -> anyhow::Result<()>
    {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let entry = StoreEntry {
            store_path: "/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-mismatch".to_string(),
            info: ValidPathInfo {
                nar_hash: vec![0xee; 32],
                nar_size: 100, // declared 100 bytes…
                ..Default::default()
            },
            nar: NarPayload::Bytes(vec![0u8; 50]), // …but the payload is 50.
        };

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let err = client_add_to_store_nar(&mut cr, &mut cw, entry, false, false)
            .await
            .expect_err("size mismatch must be rejected");
        assert!(matches!(err, ClientOpError::Wire(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("nar size mismatch"),
            "message must name the size mismatch: {err}"
        );

        // Nothing may have been written: dropping the client halves EOFs the
        // server side, which must then observe zero buffered bytes.
        drop(cr);
        drop(cw);
        let mut probe = [0u8; 1];
        let n = server_stream.read(&mut probe).await?;
        assert_eq!(
            n, 0,
            "no opcode/bytes may reach the wire on a size mismatch"
        );
        Ok(())
    }

    /// client_add_multiple_to_store batch round-trip: two entries (one
    /// in-memory, one streaming) travel inside ONE framed stream whose
    /// decoded content is the count, then per entry the path string, the
    /// ValidPathInfo body, and exactly nar_size raw NAR bytes (not
    /// nested-framed). The fake server de-frames with `FramedStreamReader`
    /// and parses the interior exactly as the gateway's
    /// `handle_add_multiple_to_store` does.
    #[tokio::test]
    async fn client_add_multiple_to_store_two_entries_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let path_a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-first";
        let path_b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-second";
        let nar_a = b"first entry nar bytes".to_vec();
        let nar_b: Vec<u8> = (0..300 * 1024).map(|i| (i % 247) as u8).collect();
        let (len_a, len_b) = (nar_a.len() as u64, nar_b.len() as u64);

        let server = tokio::spawn({
            let (nar_a, nar_b) = (nar_a.clone(), nar_b.clone());
            async move {
                let (mut sr, mut sw) = tokio::io::split(server_stream);
                let op = wire::read_u64(&mut sr).await?;
                assert_eq!(op, WorkerOp::AddMultipleToStore as u64);
                let repair = wire::read_bool(&mut sr).await?;
                assert!(!repair, "repair flag must reach the wire as false");
                let dont_check_sigs = wire::read_bool(&mut sr).await?;
                assert!(dont_check_sigs, "dontCheckSigs must reach the wire as true");

                // Everything else lives inside one framed stream.
                let mut framed = wire::FramedStreamReader::new_unbounded(&mut sr);
                let count = wire::read_u64(&mut framed).await?;
                assert_eq!(count, 2);

                // Entry 1.
                let path = wire::read_string(&mut framed).await?;
                assert_eq!(path, path_a);
                let info = read_valid_path_info(&mut framed).await?;
                assert_eq!(info.nar_size, len_a);
                let mut got_a = vec![0u8; len_a as usize];
                tokio::io::AsyncReadExt::read_exact(&mut framed, &mut got_a).await?;
                assert_eq!(got_a, nar_a);

                // Entry 2 — its metadata starts wherever entry 1's NAR ended,
                // with no relation to frame boundaries.
                let path = wire::read_string(&mut framed).await?;
                assert_eq!(path, path_b);
                let info = read_valid_path_info(&mut framed).await?;
                assert_eq!(info.nar_size, len_b);
                let mut got_b = vec![0u8; len_b as usize];
                tokio::io::AsyncReadExt::read_exact(&mut framed, &mut got_b).await?;
                assert_eq!(got_b, nar_b);

                // Nothing after the last entry but the frame terminator.
                let mut probe = [0u8; 1];
                let n = tokio::io::AsyncReadExt::read(&mut framed, &mut probe).await?;
                assert_eq!(n, 0, "no trailing data after the declared entries");

                wire::write_u64(&mut sw, STDERR_LAST).await?;
                sw.flush().await?;
                anyhow::Ok(())
            }
        });

        let entries = vec![
            StoreEntry {
                store_path: path_a.to_string(),
                info: ValidPathInfo {
                    nar_hash: vec![0x11; 32],
                    nar_size: len_a,
                    ..Default::default()
                },
                nar: NarPayload::Bytes(nar_a),
            },
            StoreEntry {
                store_path: path_b.to_string(),
                info: ValidPathInfo {
                    nar_hash: vec![0x22; 32],
                    nar_size: len_b,
                    ..Default::default()
                },
                nar: NarPayload::Reader {
                    len: len_b,
                    reader: Box::new(std::io::Cursor::new(nar_b)),
                },
            },
        ];
        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_add_multiple_to_store(&mut cr, &mut cw, false, true, entries).await?;
        server.await??;
        Ok(())
    }

    /// An empty batch is a valid `wopAddMultipleToStore` request: the framed
    /// stream carries just the count 0 (then the frame terminator), the
    /// daemon answers with STDERR_LAST alone, and the client completes with
    /// Ok(()).
    #[tokio::test]
    async fn client_add_multiple_to_store_empty_batch_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::AddMultipleToStore as u64);
            let _repair = wire::read_bool(&mut sr).await?;
            let _dont_check_sigs = wire::read_bool(&mut sr).await?;

            // The framed stream still arrives, with just the zero count
            // inside it.
            let mut framed = wire::FramedStreamReader::new_unbounded(&mut sr);
            let count = wire::read_u64(&mut framed).await?;
            assert_eq!(count, 0, "empty batch must reach the wire as count 0");
            // Nothing after the count but the frame terminator.
            let mut probe = [0u8; 1];
            let n = tokio::io::AsyncReadExt::read(&mut framed, &mut probe).await?;
            assert_eq!(n, 0, "no trailing data after the zero count");

            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_add_multiple_to_store(&mut cr, &mut cw, false, false, Vec::new()).await?;
        server.await??;
        Ok(())
    }

    /// A daemon refusal that arrives while the upload is still in flight is
    /// surfaced as `ClientOpError::Daemon` instead of deadlocking. The fake
    /// server reads only the fixed header, then refuses and STOPS READING
    /// while keeping the connection open; the payload (1 MiB across two
    /// entries) is far larger than the 64 KiB duplex buffer, so the client
    /// can only complete if the stderr drain runs concurrently with the
    /// (stalled) payload write.
    #[tokio::test]
    async fn client_add_multiple_to_store_daemon_refusal_is_typed() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        // Held until the client call returns so the server halves stay open:
        // a dropped server would fail the client's writes with a broken pipe
        // instead of exercising the buffer-full + concurrent-drain path.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut sr, sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::AddMultipleToStore as u64);
            let _repair = wire::read_bool(&mut sr).await?;
            let _dont_check_sigs = wire::read_bool(&mut sr).await?;
            // Refuse immediately, without consuming any of the framed
            // payload, and stop reading.
            let mut w = StderrWriter::new(sw);
            w.error(&StderrError::simple("rio-build", "store quota exceeded"))
                .await?;
            let _ = done_rx.await;
            anyhow::Ok(())
        });

        let entry = |path: &str, fill: u8| StoreEntry {
            store_path: path.to_string(),
            info: ValidPathInfo {
                nar_hash: vec![fill; 32],
                nar_size: 512 * 1024,
                ..Default::default()
            },
            nar: NarPayload::Bytes(vec![fill; 512 * 1024]),
        };
        let entries = vec![
            entry("/nix/store/cccccccccccccccccccccccccccccccc-big-one", 0x33),
            entry("/nix/store/dddddddddddddddddddddddddddddddd-big-two", 0x44),
        ];

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_add_multiple_to_store(&mut cr, &mut cw, false, false, entries),
        )
        .await
        .expect("refusal must be picked up concurrently, not deadlock on the unread payload");

        match result.expect_err("daemon refusal must surface as an error") {
            ClientOpError::Daemon(e) => {
                assert!(
                    e.message.contains("store quota exceeded"),
                    "daemon message must be preserved: {}",
                    e.message
                );
            }
            other => panic!("expected ClientOpError::Daemon, got {other:?}"),
        }
        drop(done_tx);
        server.await??;
        Ok(())
    }
}
