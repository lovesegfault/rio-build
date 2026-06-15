//! `SchedulerService.GetDerivationLog` — tenant-facing log reads.
//!
//! Serves a derivation execution's log for a build the caller owns:
//! `rio log` asks for any derivation it built, and `rio build`'s
//! failure replay asks for the poisoned culprit named by `BuildFailed`.
//! Log bytes live in `rio-store`'s `LogService`; this module is the
//! tenant-scoped resolution + auth proxy in front of `TailLog`.
//!
//! Tenancy (r\[sched.log.tenant-scoped\]): log content is only ever
//! served for an execution attributable to one of the caller's builds
//! via `build_derivations.exec_id`. A pinned execution that belongs to
//! another tenant (the cross-tenant culprit case) yields an empty
//! stream — failure replay falls back to the persisted reason text. A
//! derivation with no execution under the caller's builds is NOT_FOUND,
//! the same answer whether or not other tenants ever built it.

use std::collections::VecDeque;

use rio_common::grpc::StatusExt;
use rio_proto::scheduler::GetDerivationLogRequest;
use rio_proto::store::{TailLogChunk, TailLogRequest};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::SchedulerGrpc;

pub(super) type LogStream = ReceiverStream<Result<TailLogChunk, Status>>;

/// Metadata key under which the resolved execution id is sent as
/// initial metadata. `TailLogChunk.exec_id` carries the same value, but
/// the header lets a caller learn which execution was picked before
/// committing to draining the stream.
pub const RESOLVED_EXEC_HEADER: &str = "x-rio-resolved-exec-id";

/// Handler body for `GetDerivationLog` (the trait method in
/// `scheduler_service.rs` is a thin wrapper around this).
// r[impl sched.log.tenant-scoped]
pub(super) async fn get_derivation_log(
    grpc: &SchedulerGrpc,
    caller_tenant: Option<Uuid>,
    jwt_token: Option<String>,
    request: Request<GetDerivationLogRequest>,
) -> Result<Response<LogStream>, Status> {
    let req = request.into_inner();
    if req.derivation_path.is_empty() {
        return Err(Status::invalid_argument("derivation_path is required"));
    }
    let db = grpc.db.as_ref().ok_or_else(|| {
        Status::failed_precondition("GetDerivationLog requires a database connection")
    })?;
    let pinned_exec: Option<Uuid> = if req.exec_id.is_empty() {
        None
    } else {
        Some(req.exec_id.parse().status_invalid("invalid exec_id UUID")?)
    };

    // Ownership anchor: a pinned build must exist, belong to the caller,
    // and contain the derivation. Empty build_id = no pin; resolution
    // below searches the caller's builds only.
    let mut build_recorded_exec: Option<Uuid> = None;
    let mut drv_in_callers_build = false;
    if !req.build_id.is_empty() {
        let build_id = SchedulerGrpc::parse_build_id(&req.build_id)?;
        let build_tenant = db
            .build_tenant(build_id)
            .await
            .status_internal("build lookup failed")?
            .ok_or_else(|| Status::not_found(format!("unknown build {build_id}")))?;
        if caller_tenant.is_some() && build_tenant != caller_tenant {
            return Err(Status::permission_denied(
                "build belongs to a different tenant",
            ));
        }
        match db
            .build_drv_exec(build_id, &req.derivation_path)
            .await
            .status_internal("build_derivations lookup failed")?
        {
            Some(exec) => {
                drv_in_callers_build = true;
                build_recorded_exec = exec;
            }
            None => {
                return Err(Status::not_found(format!(
                    "derivation {} is not part of build {build_id}",
                    req.derivation_path
                )));
            }
        }
    }

    // Resolve which execution may be served. Log content only ever comes
    // from an execution attributable to the caller's builds — checked on
    // the EXECUTION that would be served, not merely on the requested
    // build, so a culprit that last ran for another tenant never leaks
    // its log content.
    let exec = match pinned_exec {
        Some(exec) => db
            .exec_attributable(exec, caller_tenant)
            .await
            .status_internal("execution attribution lookup failed")?
            .then_some(exec),
        None => match build_recorded_exec {
            Some(exec) => Some(exec),
            None => db
                .latest_exec_for_drv(&req.derivation_path, caller_tenant)
                .await
                .status_internal("execution lookup failed")?,
        },
    };

    let Some(exec) = exec else {
        if drv_in_callers_build || pinned_exec.is_some() {
            // Legitimately the caller's to ask about, but no
            // caller-attributable execution exists (never executed for
            // the caller, or the pinned execution ran for another
            // tenant): close the stream with no content.
            return Ok(Response::new(empty_stream()));
        }
        // Nothing about this derivation is visible to the caller — same
        // NOT_FOUND whether or not other tenants ever built it.
        return Err(Status::not_found(format!(
            "no execution of {} found among your builds",
            req.derivation_path
        )));
    };

    let Some(mut log_client) = grpc.log_client.clone() else {
        return Err(Status::failed_precondition(
            "GetDerivationLog requires a configured store LogService client",
        ));
    };

    let mut tail = Request::new(TailLogRequest {
        derivation: req.derivation_path,
        exec_id: exec.to_string(),
        since_line: req.since_line,
        follow: false,
    });
    if let Some(t) = &jwt_token {
        // Propagate the caller's JWT so the store's own
        // build-membership gate (`authorize_tail`) sees the same tenant
        // — defence in depth; the resolution above already proved
        // attribution.
        let _ = rio_common::grpc::inject_metadata(
            tail.metadata_mut(),
            &[(rio_proto::TENANT_TOKEN_HEADER, t)],
        );
    }
    let upstream = log_client
        .tail_log(tail)
        .await
        .map_err(|s| {
            Status::new(
                s.code(),
                format!("store LogService.TailLog: {}", s.message()),
            )
        })?
        .into_inner();

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let tail_lines = req.tail_lines;
    tokio::spawn(proxy_tail(upstream, tx, tail_lines));

    let mut response = Response::new(ReceiverStream::new(rx));
    let _ = rio_common::grpc::inject_metadata(
        response.metadata_mut(),
        &[(RESOLVED_EXEC_HEADER, &exec.to_string())],
    );
    Ok(response)
}

/// Relay an upstream `TailLog` stream to the client, honouring
/// `tail_lines` server-side. The store has no tail cursor, so for a
/// non-zero tail this drains the (intra-cluster) upstream and forwards
/// only a window holding the last `tail_lines` lines — the client never
/// downloads the whole log just to show a tail.
async fn proxy_tail(
    mut upstream: tonic::Streaming<TailLogChunk>,
    tx: tokio::sync::mpsc::Sender<Result<TailLogChunk, Status>>,
    tail_lines: u32,
) {
    if tail_lines == 0 {
        loop {
            match upstream.message().await {
                Ok(Some(chunk)) => {
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(status) => {
                    let _ = tx.send(Err(status)).await;
                    return;
                }
            }
        }
    }

    let cap = tail_lines as usize;
    let mut window: VecDeque<TailLogChunk> = VecDeque::new();
    let mut buffered_lines = 0usize;
    loop {
        match upstream.message().await {
            Ok(Some(chunk)) => {
                buffered_lines += chunk.lines.len();
                window.push_back(chunk);
                // Drop whole chunks from the front while doing so still
                // leaves at least `cap` lines buffered. The client may
                // therefore receive up to (cap + one chunk's worth)
                // lines — chunk boundaries are preserved so
                // `first_line_number` stays truthful.
                while let Some(front) = window.front() {
                    if buffered_lines - front.lines.len() < cap {
                        break;
                    }
                    buffered_lines -= front.lines.len();
                    window.pop_front();
                }
            }
            Ok(None) => break,
            Err(status) => {
                let _ = tx.send(Err(status)).await;
                return;
            }
        }
    }
    for chunk in window {
        if tx.send(Ok(chunk)).await.is_err() {
            return;
        }
    }
}

/// A stream that closes immediately with no chunks: the
/// "nothing to serve, and that is not an error" answer.
fn empty_stream() -> LogStream {
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(first: u64, n: usize) -> TailLogChunk {
        TailLogChunk {
            exec_id: String::new(),
            lines: (0..n).map(|i| format!("line{i}").into_bytes()).collect(),
            first_line_number: first,
            is_complete: false,
        }
    }

    /// `tail_lines` keeps a chunk-granular suffix of at least N lines,
    /// dropping whole chunks from the front so `first_line_number`
    /// stays correct on every forwarded chunk.
    #[tokio::test]
    async fn tail_window_drops_whole_chunks_from_front() {
        // Three upstream chunks of 4/4/4 lines (line numbers 0..12).
        let (utx, urx) = tokio::sync::mpsc::channel::<Result<TailLogChunk, Status>>(8);
        for c in [chunk(0, 4), chunk(4, 4), chunk(8, 4)] {
            utx.send(Ok(c)).await.unwrap();
        }
        drop(utx);
        // Feed via an in-memory tonic::Streaming would need a server;
        // exercise the windowing on a Vec instead by inlining the loop.
        // (proxy_tail's untailed path is a straight relay; only the
        // window arithmetic warrants a unit test.)
        let cap = 5usize;
        let mut window: VecDeque<TailLogChunk> = VecDeque::new();
        let mut buffered = 0usize;
        let mut up = ReceiverStream::new(urx);
        use tokio_stream::StreamExt;
        while let Some(Ok(c)) = up.next().await {
            buffered += c.lines.len();
            window.push_back(c);
            while let Some(front) = window.front() {
                if buffered - front.lines.len() < cap {
                    break;
                }
                buffered -= front.lines.len();
                window.pop_front();
            }
        }
        // cap=5 over 4/4/4: dropping the first chunk leaves 8 ≥ 5;
        // dropping the second would leave 4 < 5 → keep last two chunks.
        assert_eq!(window.len(), 2);
        assert_eq!(window[0].first_line_number, 4);
        assert_eq!(window[1].first_line_number, 8);
        assert_eq!(buffered, 8);
    }
}
