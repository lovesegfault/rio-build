//! The worker side of the IFD relay (ADR-024 "IFD"): a worker hitting
//! import-from-derivation sends the needed drv's input closure (an
//! intermediate `ResultFrame`) plus an `IfdRequest`, then BLOCKS on
//! its socketpair until the matching `IfdCompletion` arrives. On
//! success the coordinator has materialized the outputs into
//! `<cas_root>/fetched/`; this side imports them into the worker's
//! eval store so the blocked import can read them.

use std::io;

use rio_proto::evaljob::{
    CoordinatorFrame, IfdRequest, WorkerFrame, coordinator_frame, worker_frame,
};

use super::framing::{self, FdIo};
use crate::store::{EvalStore, EvalStoreError};

/// Relay one IFD to the coordinator and block until it resolves.
/// Returns the realized output store paths (already imported into the
/// eval store). The error string is what the nix-side import fails
/// with — including the coordinator's `--local-ifd` named refusal,
/// which arrives as an `IfdCompletion.error`.
// r[impl bc.evalparent.ifd-relay]
pub fn ifd_request_blocking(
    store: &EvalStore,
    fd: std::os::fd::RawFd,
    drv_path: &str,
) -> Result<Vec<String>, String> {
    let mut io = FdIo(fd);

    // 1. The IFD drv's transitive skeleton, as an intermediate batch
    //    under the coordinator's mini-submission attr — the
    //    coordinator's per-root gate needs the closure folded before
    //    the IfdRequest root can submit (`bc.submit.all-acked`).
    let mut pre = store
        .assemble_subgraph(drv_path)
        .map_err(|e| format!("assembling IFD closure for {drv_path}: {e}"))?;
    pre.attr = format!("ifd:{drv_path}");
    pre.root_drv_digest = Vec::new(); // intermediate batch, not a root
    framing::write_frame(
        &mut io,
        &WorkerFrame {
            msg: Some(worker_frame::Msg::Result(pre)),
        },
    )
    .map_err(|e| format!("sending IFD closure frame: {e}"))?;

    // 2. The request itself (node + body unconditionally — the request
    //    must carry the root even when an earlier frame shipped it).
    let (node, blob) = store
        .ifd_materials(drv_path)
        .map_err(|e| format!("IFD materials for {drv_path}: {e}"))?;
    framing::write_frame(
        &mut io,
        &WorkerFrame {
            msg: Some(worker_frame::Msg::IfdRequest(IfdRequest {
                node: Some(node),
                blob: Some(blob),
            })),
        },
    )
    .map_err(|e| format!("sending IfdRequest: {e}"))?;

    // 3. Block. The parent assigns one attr at a time and only sends
    //    Shutdown to idle workers, so the ONLY expected frame here is
    //    our completion.
    loop {
        let frame: Option<CoordinatorFrame> =
            framing::read_frame(&mut io).map_err(|e| format!("reading IFD completion: {e}"))?;
        let Some(frame) = frame else {
            return Err("channel closed while waiting for IFD completion".to_string());
        };
        match frame.msg {
            Some(coordinator_frame::Msg::IfdCompletion(c)) if c.drv_path == drv_path => {
                if !c.error.is_empty() {
                    return Err(c.error);
                }
                import_outputs(store, &c.output_paths)?;
                return Ok(c.output_paths);
            }
            // A completion for another drv cannot arrive (one blocked
            // import per worker; the parent routes by drv_path) — if
            // it does, the routing table is broken: fail loudly.
            Some(other) => {
                return Err(format!(
                    "unexpected frame while blocked on IFD {drv_path}: {other:?}"
                ));
            }
            None => continue, // unknown future frame kind
        }
    }
}

/// Map coordinator-fetched outputs into the eval store. The fetched
/// copies live under `<cas_root>/fetched/<basename>` (narHash-verified
/// by the coordinator on arrival — `bc.fetch.narhash-verify`).
fn import_outputs(store: &EvalStore, paths: &[String]) -> Result<(), String> {
    for full in paths {
        let basename = full
            .rsplit('/')
            .next()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| format!("malformed IFD output path {full:?}"))?;
        let fetched = store.cas_root().join("fetched").join(basename);
        if !fetched.exists() {
            return Err(format!(
                "IFD output {full} was not materialized at {} — coordinator/fetch bug",
                fetched.display()
            ));
        }
        let fetched_str = fetched
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 fetched path {fetched:?}"))?;
        match store.import_local_tree_as(fetched_str, full) {
            Ok(()) => {}
            Err(EvalStoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                return Err(format!("IFD output {full} vanished during import: {e}"));
            }
            Err(e) => return Err(format!("importing IFD output {full}: {e}")),
        }
    }
    Ok(())
}
