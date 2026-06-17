//! Paginated `SubmitBuild` assembly (ADR-024 P2a).
//!
//! The submission skeleton is a measured ~334B/node, so the 16MB
//! `SubmitBuild` budget is exceeded around ~50k raw nodes. Larger
//! submissions arrive as N pages sharing a client-chosen
//! `submission_id`: non-final pages are staged keyed by
//! `(tenant, submission_id)` and acked with an immediately-closed
//! empty event stream; the final page assembles every staged page (in
//! arrival order) plus itself into ONE request that then flows through
//! the SAME validation, digest classification, and bulk-verify as an
//! unpaged submission — pagination changes transport framing, never
//! acceptance semantics.
//!
//! Staging is bounded two ways: per-submission and globally by
//! `MAX_DAG_NODES`, and by a TTL — a client that never sends its final
//! page (crash, retry under a fresh id) must not pin memory forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tonic::Status;
use uuid::Uuid;

use rio_proto::types::SubmitBuildRequest;

/// Staged pages expire when no page for the submission has arrived
/// for this long. Generous: a client streaming 20+ pages over a slow
/// link still touches the entry on every page.
const STAGE_TTL: Duration = Duration::from_secs(10 * 60);

/// `submission_id` length cap — it is a client-chosen opaque string
/// used as a map key; unbounded would be a memory lever.
const MAX_SUBMISSION_ID_LEN: usize = 128;

#[derive(Default)]
struct Entry {
    nodes: Vec<rio_proto::types::DerivationNode>,
    edges: Vec<rio_proto::types::DerivationEdge>,
    last_update: Option<Instant>,
}

/// The staging area shared by all `SchedulerGrpc` clones.
#[derive(Default)]
pub(crate) struct StagedPages {
    entries: HashMap<(Option<Uuid>, String), Entry>,
    /// Σ nodes across all entries — the global memory bound.
    total_nodes: usize,
    /// Σ edges across all entries. Bounded separately from nodes: a
    /// page of zero nodes and millions of edges would otherwise pass
    /// the node cap while pinning unbounded memory.
    total_edges: usize,
}

impl StagedPages {
    fn evict_stale(&mut self, now: Instant) {
        self.entries.retain(|_, e| {
            let live = e
                .last_update
                .is_some_and(|t| now.duration_since(t) < STAGE_TTL);
            if !live {
                self.total_nodes -= e.nodes.len();
                self.total_edges -= e.edges.len();
            }
            live
        });
    }

    fn remove(&mut self, key: &(Option<Uuid>, String)) -> Option<Entry> {
        let e = self.entries.remove(key)?;
        self.total_nodes -= e.nodes.len();
        self.total_edges -= e.edges.len();
        Some(e)
    }
}

/// Outcome of [`stage_or_assemble`].
pub(super) enum PageOutcome {
    /// Non-final page staged; ack with an empty, immediately-closed
    /// event stream. Carries the submission's staged-node total for
    /// the `x-rio-staged-nodes` response header.
    Staged { total_nodes: usize },
    /// Final page (or unpaged request): proceed with this request.
    Ready(Box<SubmitBuildRequest>),
}

/// Stage a page or assemble the full submission.
///
/// `tenant` is the ATTESTED caller identity (`claims.sub`), not the
/// body field — one tenant can never append pages into another
/// tenant's staging slot. Assembly order is arrival order: staged
/// pages first, the final page's nodes/edges last.
// r[impl sched.submit.paginate]
pub(super) fn stage_or_assemble(
    staged: &std::sync::Mutex<StagedPages>,
    tenant: Option<Uuid>,
    mut req: SubmitBuildRequest,
) -> Result<PageOutcome, Status> {
    if req.submission_id.is_empty() {
        // Unpaged. final_page without a submission_id has nothing to
        // refer to — tolerate (treat as unpaged) rather than reject:
        // the flag is meaningless without an id by construction.
        return Ok(PageOutcome::Ready(Box::new(req)));
    }
    if req.submission_id.len() > MAX_SUBMISSION_ID_LEN {
        return Err(Status::invalid_argument(format!(
            "submission_id too long: {} > {MAX_SUBMISSION_ID_LEN}",
            req.submission_id.len()
        )));
    }

    let key = (tenant, std::mem::take(&mut req.submission_id));
    let now = Instant::now();
    let mut staged = staged.lock().expect("staged_pages mutex poisoned");
    staged.evict_stale(now);

    // Global bounds: the assembled submission is also capped at
    // MAX_DAG_NODES/MAX_DAG_EDGES later, but the staging area must be
    // bounded NOW — before any final page — or k abandoned submissions
    // × MAX pages would pin unbounded memory. Nodes and edges are
    // bounded independently so neither can be used as a lever while
    // the other stays small.
    let incoming_nodes = req.nodes.len();
    let incoming_edges = req.edges.len();
    if staged.total_nodes + incoming_nodes > rio_common::limits::MAX_DAG_NODES
        || staged.total_edges + incoming_edges > rio_common::limits::MAX_DAG_EDGES
    {
        // Drop this submission's partial state too: the client must
        // restart the whole paged submission, and keeping the prefix
        // would only hold memory for a submission that cannot finish.
        staged.remove(&key);
        return Err(Status::resource_exhausted(format!(
            "staged submission pages exceed {} total nodes / {} total edges; \
             restart the submission",
            rio_common::limits::MAX_DAG_NODES,
            rio_common::limits::MAX_DAG_EDGES
        )));
    }

    if req.final_page {
        if let Some(mut e) = staged.remove(&key) {
            e.nodes.extend(std::mem::take(&mut req.nodes));
            e.edges.extend(std::mem::take(&mut req.edges));
            req.nodes = e.nodes;
            req.edges = e.edges;
        }
        // else: single-page "paged" submission — fine, nothing staged.
        Ok(PageOutcome::Ready(Box::new(req)))
    } else {
        let entry = staged.entries.entry(key).or_default();
        entry.nodes.extend(std::mem::take(&mut req.nodes));
        entry.edges.extend(std::mem::take(&mut req.edges));
        entry.last_update = Some(now);
        let total = entry.nodes.len();
        staged.total_nodes += incoming_nodes;
        staged.total_edges += incoming_edges;
        Ok(PageOutcome::Staged { total_nodes: total })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str, nodes: usize, edges: usize, final_page: bool) -> SubmitBuildRequest {
        SubmitBuildRequest {
            submission_id: id.to_string(),
            final_page,
            nodes: (0..nodes)
                .map(|i| {
                    rio_test_support::fixtures::make_derivation_node(
                        &format!("{id}-{i}"),
                        "x86_64-linux",
                    )
                })
                .collect(),
            edges: (0..edges)
                .map(|i| rio_proto::types::DerivationEdge {
                    parent_drv_path: format!("/nix/store/p{i}.drv"),
                    child_drv_path: format!("/nix/store/c{i}.drv"),
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Same submission_id under two different tenants must stage into
    /// two separate slots — the key is the ATTESTED tenant, so one
    /// tenant can never append pages into (or assemble) another
    /// tenant's submission.
    // r[verify sched.submit.paginate]
    #[test]
    fn cross_tenant_submission_id_collision_is_isolated() {
        let staged = std::sync::Mutex::new(StagedPages::default());
        let t_a = Some(Uuid::from_u128(0xA));
        let t_b = Some(Uuid::from_u128(0xB));
        assert!(matches!(
            stage_or_assemble(&staged, t_a, req("shared-id", 2, 0, false)).unwrap(),
            PageOutcome::Staged { total_nodes: 2 }
        ));
        // Tenant B's final page with the SAME id must see an empty
        // slot: only its own single page, never tenant A's nodes.
        match stage_or_assemble(&staged, t_b, req("shared-id", 1, 0, true)).unwrap() {
            PageOutcome::Ready(r) => assert_eq!(r.nodes.len(), 1),
            PageOutcome::Staged { .. } => panic!("final page must assemble"),
        }
        // Tenant A's staged page is still intact.
        match stage_or_assemble(&staged, t_a, req("shared-id", 1, 0, true)).unwrap() {
            PageOutcome::Ready(r) => assert_eq!(r.nodes.len(), 3),
            PageOutcome::Staged { .. } => panic!("final page must assemble"),
        }
        let s = staged.lock().unwrap();
        assert_eq!(s.total_nodes, 0, "assembly must release the global count");
        assert_eq!(s.total_edges, 0);
    }

    /// Edges are bounded independently of nodes: zero-node pages with
    /// huge edge lists must trip the global cap, and the rejected
    /// submission's staged prefix must be released.
    // r[verify sched.submit.paginate]
    #[test]
    fn staged_edges_are_globally_bounded() {
        let staged = std::sync::Mutex::new(StagedPages::default());
        let half = rio_common::limits::MAX_DAG_EDGES / 2 + 1;
        assert!(matches!(
            stage_or_assemble(&staged, None, req("edge-bomb", 0, half, false)).unwrap(),
            PageOutcome::Staged { .. }
        ));
        let Err(e) = stage_or_assemble(&staged, None, req("edge-bomb", 0, half, false)) else {
            panic!("second over-cap page must be rejected");
        };
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        let s = staged.lock().unwrap();
        assert_eq!(
            s.total_edges, 0,
            "over-cap reject must drop the submission's staged prefix"
        );
        assert!(s.entries.is_empty());
    }
}
