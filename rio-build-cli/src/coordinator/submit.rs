//! Stage 4: per-root submission on the all-acked gate, with client
//! pagination and the ADR-024 stale-ack recovery contract.
//!
//! Pagination (server side: `r[sched.submit.paginate]`): pages share a
//! client-chosen `submission_id`; non-final pages are acked by an
//! immediately-closed empty event stream; the final page's response
//! stream IS the build's event stream.
//!
//! Stale-ack recovery: a `FAILED_PRECONDITION` reject names every
//! missing drv digest (the scheduler's submit-time bulk-verify). The
//! client evicts those acks, re-`Has`es, re-uploads, and resubmits —
//! ONCE. A second reject is a hard error: either the cluster is GCing
//! faster than the ack TTL models (a config bug) or the upload path is
//! broken; retrying forever would mask both.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rio_proto::types::{BuildEvent, DerivationNode, DrvBlob, HasDrvsRequest, SubmitBuildRequest};
use tonic::Streaming;
use tracing::{info, instrument, warn};

use crate::acks::{ClusterAckTable, ObjectKind};
use crate::coordinator::clients::{Clients, bitmap_bit};
use crate::coordinator::graph::{Digest32, SubmitOptions, paginate};
use crate::coordinator::upload::reupload_drvs;

/// Materials one root submission needs, captured from the graph at
/// gate time. `bodies` covers the FULL closure (including nodes
/// excluded as already-submitted) so recovery can re-upload any digest
/// the scheduler names.
pub struct SubmitMaterials {
    pub nodes: Vec<DerivationNode>,
    pub bodies: HashMap<Digest32, DrvBlob>,
    pub opts: SubmitOptions,
    pub page_max_nodes: usize,
}

/// Submit a root (paginating as needed). Returns the accepted
/// submission's event stream.
// r[impl bc.submit.stale-ack-once]
#[instrument(skip_all, fields(component = "build-client", nodes = mats.nodes.len()))]
pub async fn submit_root(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    mats: &SubmitMaterials,
) -> anyhow::Result<Streaming<BuildEvent>> {
    match try_submit(clients, &mats.nodes, &mats.opts, mats.page_max_nodes, "a").await {
        Ok(stream) => Ok(stream),
        Err(SubmitError::StaleAcks(missing)) => {
            warn!(
                missing = missing.len(),
                "submission rejected on missing drv digests — running stale-ack recovery"
            );
            recover_stale_acks(clients, acks, mats, &missing).await?;
            match try_submit(clients, &mats.nodes, &mats.opts, mats.page_max_nodes, "b").await {
                Ok(stream) => {
                    info!("stale-ack recovery succeeded on resubmit");
                    Ok(stream)
                }
                Err(SubmitError::StaleAcks(still_missing)) => bail!(
                    "submission rejected twice on missing drv digests — recovery uploaded \
                     {} blob(s) but the scheduler still names {} missing ({}); giving up \
                     (second reject is a hard error per ADR-024)",
                    missing.len(),
                    still_missing.len(),
                    still_missing
                        .iter()
                        .map(hex::encode)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Err(SubmitError::Other(e)) => Err(e),
            }
        }
        Err(SubmitError::Other(e)) => Err(e),
    }
}

enum SubmitError {
    /// FAILED_PRECONDITION naming missing drv digests.
    StaleAcks(Vec<Digest32>),
    Other(anyhow::Error),
}

/// One submission attempt: stage non-final pages, return the final
/// page's event stream. `attempt` salts the submission_id so a
/// recovery resubmit never collides with the rejected attempt's
/// (already-discarded) staged pages.
async fn try_submit(
    clients: &mut Clients,
    nodes: &[DerivationNode],
    opts: &SubmitOptions,
    page_max_nodes: usize,
    attempt: &str,
) -> Result<Streaming<BuildEvent>, SubmitError> {
    let submission_id = format!("{}-{attempt}", uuid::Uuid::new_v4());
    let mut pages = paginate(nodes.to_vec(), opts, page_max_nodes, &submission_id);
    let last = pages.pop().expect("paginate returns at least one page");
    for (i, page) in pages.into_iter().enumerate() {
        let mut stream = submit_page(clients, page).await?;
        // Staged ack == clean close with zero events.
        match stream.message().await {
            Ok(None) => {}
            Ok(Some(ev)) => {
                return Err(SubmitError::Other(anyhow::anyhow!(
                    "scheduler emitted an event for staged page {i} (sequence {}) — \
                     expected an empty close",
                    ev.sequence
                )));
            }
            Err(status) => return Err(classify(status)),
        }
    }
    submit_page(clients, last).await
}

async fn submit_page(
    clients: &mut Clients,
    page: SubmitBuildRequest,
) -> Result<Streaming<BuildEvent>, SubmitError> {
    let req = clients.req(page).map_err(SubmitError::Other)?;
    match clients.scheduler.submit_build(req).await {
        Ok(resp) => Ok(resp.into_inner()),
        Err(status) => Err(classify(status)),
    }
}

fn classify(status: tonic::Status) -> SubmitError {
    if status.code() == tonic::Code::FailedPrecondition {
        let missing = parse_missing_digests(status.message());
        if !missing.is_empty() {
            return SubmitError::StaleAcks(missing);
        }
    }
    SubmitError::Other(anyhow::Error::new(status).context("SubmitBuild"))
}

/// The recovery cycle: evict the named acks, re-`Has`, re-upload what
/// the cluster still misses (from retained bodies), then let the
/// caller resubmit.
async fn recover_stale_acks(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    mats: &SubmitMaterials,
    missing: &[Digest32],
) -> anyhow::Result<()> {
    acks.lock()
        .expect("ack table mutex poisoned")
        .evict(ObjectKind::Drv, missing)?;

    let bitmap = clients
        .drv_blobs
        .has_drvs(clients.req(HasDrvsRequest {
            digests: missing.iter().map(|d| d.to_vec()).collect(),
        })?)
        .await
        .context("HasDrvs (stale-ack recovery)")?
        .into_inner()
        .bitmap;

    let mut to_upload: Vec<DrvBlob> = Vec::new();
    for (i, d) in missing.iter().enumerate() {
        if bitmap_bit(&bitmap, i) {
            // Present after all (raced a concurrent uploader, or the
            // scheduler's view lagged) — re-record the ack.
            acks.lock()
                .expect("ack table mutex poisoned")
                .record(ObjectKind::Drv, &[*d])?;
            continue;
        }
        match mats.bodies.get(d) {
            Some(b) => to_upload.push(b.clone()),
            None => bail!(
                "scheduler names drv digest {} missing but its body is no longer retained \
                 (dropped after an earlier accepted submission) — cannot re-upload without \
                 a re-eval; rerun the build",
                hex::encode(d)
            ),
        }
    }
    reupload_drvs(clients, acks, to_upload).await
}

/// Extract 32-byte digests from a reject message: every 64-char hex
/// token. This parses the scheduler's documented reject shape
/// (`digest_submit::verify_resolved` lists each missing digest in
/// hex); scanning for hex tokens keeps the client tolerant of message
/// rewording around the list.
fn parse_missing_digests(message: &str) -> Vec<Digest32> {
    let mut out = Vec::new();
    let bytes = message.as_bytes();
    let is_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let mut i = 0;
    while i < bytes.len() {
        if !is_hex(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_hex(bytes[i]) {
            i += 1;
        }
        if i - start == 64
            && let Ok(raw) = hex::decode(&message[start..i])
            && let Ok(d) = Digest32::try_from(raw.as_slice())
        {
            out.push(d);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_digest_list_from_reject_message() {
        let d1 = hex::encode([0xAB; 32]);
        let d2 = hex::encode([0x01; 32]);
        let msg = format!(
            "unknown drv digests (not in this submission, not in the store): [{d1}, {d2}] — \
             re-check presence (HasDrvs), upload the missing drv blobs (PutDrvBlobs), and resubmit"
        );
        let parsed = parse_missing_digests(&msg);
        assert_eq!(parsed, vec![[0x01; 32], [0xAB; 32]]);
    }

    #[test]
    fn ignores_short_hex_and_dedups() {
        let d = hex::encode([0x42; 32]);
        let msg = format!("deadbeef {d} cafe {d}");
        assert_eq!(parse_missing_digests(&msg), vec![[0x42; 32]]);
    }
}
