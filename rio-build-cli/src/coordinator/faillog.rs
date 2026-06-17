//! Failure replay: when a build fail-fasts on a derivation that already
//! failed in an earlier build, the `BuildFailed` event names the
//! poisoned culprit and the execution that originally failed
//! (`culprit_*` fields). This module fetches that execution's stored
//! log via `SchedulerService.GetDerivationLog` and re-prints it through
//! the renderer — tail by default, full log under
//! `-L/--print-build-logs` — falling back to the persisted reason text
//! when no log content is available (the execution produced no output,
//! the log expired, or it belongs to another tenant).

use rio_proto::scheduler::GetDerivationLogRequest;
use rio_proto::types::BuildFailed;
use tracing::debug;

use crate::render::{RenderHandle, fmt_duration, short_drv};

use super::clients::Clients;

/// How a fail-fast failure's original log is replayed
/// (`--log-lines` / `-L`).
#[derive(Debug, Clone, Copy)]
pub struct FailureLogOpts {
    /// Lines of the original log to replay (server-side tail).
    pub log_lines: u32,
    /// `-L/--print-build-logs`: replay the full log instead of a tail.
    pub print_build_logs: bool,
}

impl Default for FailureLogOpts {
    fn default() -> Self {
        Self {
            log_lines: 20,
            print_build_logs: false,
        }
    }
}

impl FailureLogOpts {
    /// The wire `tail_lines` value: 0 = full log.
    fn tail_lines(self) -> u32 {
        if self.print_build_logs {
            0
        } else {
            self.log_lines
        }
    }
}

/// Fetch and re-print the original failure's log for a fail-fast
/// `BuildFailed` (one renderer Note per line — each renderer's existing
/// per-line sanitization applies). Returns the emitted lines so tests
/// can assert on them without capturing stderr. No-op (empty return)
/// when the event carries no culprit attribution.
// r[impl bc.render.failure-log-tail]
pub async fn replay_failure_log(
    clients: &mut Clients,
    build_id: &str,
    failed: &BuildFailed,
    opts: FailureLogOpts,
    render: &RenderHandle,
) -> Vec<String> {
    if failed.culprit_derivation.is_empty() {
        return Vec::new();
    }
    let culprit = short_drv(&failed.culprit_derivation).to_string();
    let when = failed
        .culprit_failed_at
        .as_ref()
        .and_then(|ts| {
            let failed_at =
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts.seconds.max(0) as u64);
            std::time::SystemTime::now().duration_since(failed_at).ok()
        })
        .map(|age| format!(", {} ago", fmt_duration(age)))
        .unwrap_or_default();
    let exec = if failed.culprit_exec_id.is_empty() {
        String::new()
    } else {
        format!(" (exec {}{when})", failed.culprit_exec_id)
    };

    let lines = fetch_log_lines(
        clients,
        GetDerivationLogRequest {
            build_id: build_id.to_string(),
            derivation_path: failed.culprit_derivation.clone(),
            exec_id: failed.culprit_exec_id.clone(),
            tail_lines: opts.tail_lines(),
            since_line: 0,
        },
    )
    .await;

    let mut notes = Vec::new();
    if lines.is_empty() {
        // Reason-text fallback: the original execution produced no log
        // lines we are allowed (or able) to show.
        let reason = if failed.culprit_error_message.is_empty() {
            "no failure reason was recorded".to_string()
        } else {
            failed.culprit_error_message.clone()
        };
        notes.push(format!("{culprit} failed previously{exec}: {reason}"));
    } else {
        let what = if opts.print_build_logs {
            "original build log".to_string()
        } else {
            format!("last {} line(s) of its original build log", lines.len())
        };
        notes.push(format!("{culprit} failed previously{exec}; {what}:"));
        notes.extend(lines.iter().map(|l| format!("{culprit}> {l}")));
    }
    for note in &notes {
        render.note(note.clone());
    }
    notes
}

/// Drain a `GetDerivationLog` stream into displayable lines. Errors
/// (UNIMPLEMENTED scheduler, NOT_FOUND, transport) degrade to "no
/// lines" — the caller falls back to the persisted reason text.
async fn fetch_log_lines(clients: &mut Clients, req: GetDerivationLogRequest) -> Vec<String> {
    let request = match clients.req(req) {
        Ok(request) => request,
        Err(e) => {
            debug!(error = %e, "failure-log request construction failed");
            return Vec::new();
        }
    };
    let mut stream = match clients.scheduler.get_derivation_log(request).await {
        Ok(resp) => resp.into_inner(),
        Err(status) => {
            debug!(status = %status, "GetDerivationLog unavailable; falling back to reason text");
            return Vec::new();
        }
    };
    let mut lines = Vec::new();
    loop {
        match stream.message().await {
            Ok(Some(chunk)) => {
                for raw in &chunk.lines {
                    // Display path, not parse: build output is arbitrary
                    // bytes; the renderer sanitizes each Note line.
                    #[allow(clippy::disallowed_methods)]
                    lines.push(String::from_utf8_lossy(raw).into_owned());
                }
                if chunk.is_complete {
                    break;
                }
            }
            Ok(None) => break,
            Err(status) => {
                debug!(status = %status, "GetDerivationLog stream ended with an error");
                break;
            }
        }
    }
    lines
}
