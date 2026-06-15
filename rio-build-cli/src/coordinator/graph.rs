//! The coordinator's global digest state: fold (stage 1) and the
//! per-root all-acked submit gate bookkeeping (stage 4's predicate).
//!
//! Pure data structure — no I/O, no clients — so the gate logic,
//! already-submitted exclusion, and pagination are unit-testable
//! without a cluster.

use std::collections::{HashMap, HashSet};

use rio_proto::evaljob::{ResultFrame, SourceRoot};
use rio_proto::types::{DerivationNode, DrvBlob, SubmitBuildRequest};

pub type Digest32 = [u8; 32];

fn digest32(raw: &[u8]) -> Option<Digest32> {
    raw.try_into().ok()
}

/// One folded derivation node.
pub struct GraphNode {
    pub node: DerivationNode,
    /// Canonical drv bytes. Retained until the node is part of an
    /// ACCEPTED submission — NOT dropped on upload-ack as ADR-024's
    /// coordinator sketch suggests, because stale-ack recovery
    /// (cluster GC'd the blob between ack and submit) must re-upload
    /// from memory; drvs are memory-only client-side, so a body
    /// dropped at ack time would force a full re-eval to recover.
    pub body: Option<DrvBlob>,
    /// Cluster holds the drv blob (uploaded or already present).
    pub acked: bool,
}

/// One source tree referenced by some attr's inputSrcs.
pub struct SourceState {
    pub root: SourceRoot,
    pub acked: bool,
}

/// One submit root (= one attr's final reported drv).
pub struct RootState {
    pub attr: String,
    pub digest: Digest32,
    pub submitted: bool,
}

/// What a fold produced — the digests stages 2/3 should negotiate next.
#[derive(Default)]
pub struct FoldOutcome {
    /// Drv digests first seen in this frame (bodies attached in the
    /// graph).
    pub new_drvs: Vec<Digest32>,
    /// Source roots first seen in this frame (keyed by
    /// [`rio_evalstore::source_root_key`]).
    pub new_sources: Vec<Digest32>,
    /// Set when the frame completed an attr (carried a root digest).
    pub completed_root: Option<Digest32>,
}

/// Why a root is not yet submittable.
#[derive(Debug, PartialEq, Eq)]
pub enum RootGate {
    /// Transitive skeleton complete + all referenced objects acked.
    Ready,
    /// Closure references digests with no folded node yet (eval still
    /// streaming).
    MissingNodes(usize),
    /// Skeleton complete but uploads not yet acked.
    PendingAcks(usize),
}

#[derive(Default)]
pub struct BuildGraph {
    nodes: HashMap<Digest32, GraphNode>,
    sources: HashMap<Digest32, SourceState>,
    /// attr → source roots its frames referenced. The all-acked gate
    /// requires an attr's sources acked before its root submits
    /// (skeleton nodes don't carry per-node inputSrcs digests, so
    /// sources gate at attr granularity).
    attr_sources: HashMap<String, HashSet<Digest32>>,
    roots: Vec<RootState>,
    /// Drv digests included in an accepted submission this session —
    /// excluded from later roots' pages (the multi-root overlap
    /// filter; the scheduler resolves them via the store's drv_blobs).
    submitted: HashSet<Digest32>,
}

impl BuildGraph {
    /// Stage 1: dedup nodes by digest into the global graph; attach
    /// drv bodies; register source roots; close out the attr when the
    /// frame carries its root digest.
    // r[impl bc.fold.dedup-by-digest]
    pub fn fold(&mut self, frame: ResultFrame) -> anyhow::Result<FoldOutcome> {
        let mut out = FoldOutcome::default();
        let mut bodies: HashMap<Digest32, DrvBlob> = HashMap::new();
        for blob in frame.drv_blobs {
            let d = digest32(&blob.digest)
                .ok_or_else(|| anyhow::anyhow!("drv blob digest is not 32 bytes"))?;
            bodies.insert(d, blob);
        }
        for node in frame.nodes {
            let d = digest32(&node.drv_digest).ok_or_else(|| {
                anyhow::anyhow!(
                    "skeleton node {} has no 32-byte drv_digest — the worker channel only \
                     carries digest-bearing skeletons",
                    node.drv_path
                )
            })?;
            if self.nodes.contains_key(&d) {
                continue; // dedup by digest — the 4.47× multi-attr overlap
            }
            self.nodes.insert(
                d,
                GraphNode {
                    node,
                    body: bodies.remove(&d),
                    acked: false,
                },
            );
            out.new_drvs.push(d);
        }
        for src in frame.source_roots {
            // Same key the eval side dedups on and the upload path acks
            // on — dir roots keep the raw dir_digest, file/symlink
            // roots a domain-separated digest over store path + root.
            let d = rio_evalstore::source_root_key(&src).ok_or_else(|| {
                anyhow::anyhow!(
                    "source root {} carries neither a root node nor a 32-byte dir_digest",
                    src.store_path
                )
            })?;
            self.attr_sources
                .entry(frame.attr.clone())
                .or_default()
                .insert(d);
            if let std::collections::hash_map::Entry::Vacant(e) = self.sources.entry(d) {
                e.insert(SourceState {
                    root: src,
                    acked: false,
                });
                out.new_sources.push(d);
            }
        }
        if !frame.root_drv_digest.is_empty() {
            let d = digest32(&frame.root_drv_digest)
                .ok_or_else(|| anyhow::anyhow!("root_drv_digest is not 32 bytes"))?;
            self.roots.push(RootState {
                attr: frame.attr,
                digest: d,
                submitted: false,
            });
            out.completed_root = Some(d);
        }
        Ok(out)
    }

    pub fn mark_drv_acked(&mut self, digest: &Digest32) {
        if let Some(n) = self.nodes.get_mut(digest) {
            n.acked = true;
        }
    }

    pub fn mark_source_acked(&mut self, digest: &Digest32) {
        if let Some(s) = self.sources.get_mut(digest) {
            s.acked = true;
        }
    }

    pub fn source(&self, digest: &Digest32) -> Option<&SourceState> {
        self.sources.get(digest)
    }

    /// Transitive closure of `root` over `input_drv_digests`, in
    /// deterministic (DFS post-order-ish) order. `None` entries are
    /// digests with no folded node yet.
    fn closure(&self, root: &Digest32) -> (Vec<Digest32>, usize) {
        let mut seen = HashSet::new();
        let mut order = Vec::new();
        let mut missing = 0usize;
        let mut stack = vec![*root];
        while let Some(d) = stack.pop() {
            if !seen.insert(d) {
                continue;
            }
            match self.nodes.get(&d) {
                None => missing += 1,
                Some(n) => {
                    order.push(d);
                    for raw in &n.node.input_drv_digests {
                        if let Some(input) = digest32(raw) {
                            stack.push(input);
                        }
                    }
                }
            }
        }
        (order, missing)
    }

    /// The committed all-acked gate: a root submits once its
    /// transitive skeleton is complete AND every node's drv blob is
    /// acked AND every source root its attr referenced is acked.
    // r[impl bc.submit.all-acked]
    pub fn root_gate(&self, root: &RootState) -> RootGate {
        let (order, missing) = self.closure(&root.digest);
        if missing > 0 {
            return RootGate::MissingNodes(missing);
        }
        let mut pending = order
            .iter()
            .filter(|d| !self.nodes[*d].acked && !self.submitted.contains(*d))
            .count();
        if let Some(srcs) = self.attr_sources.get(&root.attr) {
            pending += srcs
                .iter()
                .filter(|d| self.sources.get(*d).is_none_or(|s| !s.acked))
                .count();
        }
        if pending > 0 {
            RootGate::PendingAcks(pending)
        } else {
            RootGate::Ready
        }
    }

    /// Roots not yet submitted, with their current gate state.
    pub fn pending_roots(&self) -> Vec<(usize, RootGate)> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.submitted)
            .map(|(i, r)| (i, self.root_gate(r)))
            .collect()
    }

    pub fn root(&self, idx: usize) -> &RootState {
        &self.roots[idx]
    }

    pub fn roots(&self) -> &[RootState] {
        &self.roots
    }

    /// All attrs that have reported a root (the run-completion check:
    /// every requested attr must appear here or in errors).
    pub fn completed_attrs(&self) -> HashSet<&str> {
        self.roots.iter().map(|r| r.attr.as_str()).collect()
    }

    /// The submission set for a root: its closure EXCLUDING nodes
    /// already submitted this session. Returns `(nodes, body_map)` —
    /// bodies for the FULL closure (including excluded nodes), so the
    /// submit task can re-upload any digest a stale-ack reject names.
    pub fn submission_for(&self, idx: usize) -> (Vec<DerivationNode>, HashMap<Digest32, DrvBlob>) {
        let root = &self.roots[idx];
        let (order, _missing) = self.closure(&root.digest);
        let mut nodes = Vec::new();
        let mut bodies = HashMap::new();
        for d in &order {
            let gn = &self.nodes[d];
            if let Some(b) = &gn.body {
                bodies.insert(*d, b.clone());
            }
            if !self.submitted.contains(d) {
                let mut n = gn.node.clone();
                // The root the user asked for is an explicit target.
                n.explicitly_requested = *d == root.digest;
                nodes.push(n);
            }
        }
        // Deterministic wire order (children-ish last is fine — the
        // scheduler derives edges from digests, order-independent; but
        // a stable order keeps pagination reproducible).
        nodes.sort_by(|a, b| a.drv_digest.cmp(&b.drv_digest));
        (nodes, bodies)
    }

    /// Claim a root's submission at spawn time: record its digests in
    /// the session-level exclusion set so a CONCURRENTLY-ready root's
    /// pages exclude them (two roots sharing a leaf must not both ship
    /// it — the 4.47× multi-attr overlap). Claimed ≠ accepted: bodies
    /// stay retained until [`Self::drop_bodies`] (the submit task's
    /// stale-ack recovery may still need them).
    // r[impl bc.submit.exclude-submitted]
    pub fn claim_submitted(&mut self, idx: usize, digests: &[Digest32]) {
        self.roots[idx].submitted = true;
        for d in digests {
            self.submitted.insert(*d);
        }
    }

    /// Submission accepted: drop retained drv bodies (the cluster has
    /// them; a future stale-ack against these digests fails hard
    /// rather than re-eval — see the recovery contract in `submit`).
    pub fn drop_bodies(&mut self, digests: &[Digest32]) {
        for d in digests {
            if let Some(n) = self.nodes.get_mut(d) {
                n.body = None;
            }
        }
    }

    /// Unacked drv digests among `digests` (stale-ack recovery: which
    /// of the named-missing digests we can re-upload).
    pub fn bodies_for(
        &self,
        digests: &[Digest32],
        extra: &HashMap<Digest32, DrvBlob>,
    ) -> Vec<DrvBlob> {
        digests
            .iter()
            .filter_map(|d| {
                self.nodes
                    .get(d)
                    .and_then(|n| n.body.clone())
                    .or_else(|| extra.get(d).cloned())
            })
            .collect()
    }
}

/// Stage 4 transport shaping: one `SubmitBuildRequest` below the page
/// limit, N pages sharing a client-chosen `submission_id` above it
/// (`r[sched.submit.paginate]` is the server side; this is the client
/// side). The FINAL page carries the build options; non-final pages
/// carry nodes only and are acked by an immediately-closed empty event
/// stream.
pub struct SubmitOptions {
    pub priority_class: String,
    pub tenant_name: String,
    pub keep_going: bool,
}

// r[impl bc.submit.paginate]
pub fn paginate(
    nodes: Vec<DerivationNode>,
    opts: &SubmitOptions,
    page_max_nodes: usize,
    submission_id: &str,
) -> Vec<SubmitBuildRequest> {
    let base = |nodes: Vec<DerivationNode>| SubmitBuildRequest {
        nodes,
        priority_class: opts.priority_class.clone(),
        tenant_name: opts.tenant_name.clone(),
        keep_going: opts.keep_going,
        ..Default::default()
    };
    if nodes.len() <= page_max_nodes {
        return vec![base(nodes)];
    }
    let mut pages = Vec::new();
    let mut rest = nodes;
    while rest.len() > page_max_nodes {
        let tail = rest.split_off(page_max_nodes);
        let mut page = base(rest);
        page.submission_id = submission_id.to_string();
        page.final_page = false;
        // Non-final pages carry nodes only — options ride the final
        // page (the scheduler applies them at assembly).
        page.priority_class = String::new();
        page.keep_going = false;
        pages.push(page);
        rest = tail;
    }
    let mut last = base(rest);
    last.submission_id = submission_id.to_string();
    last.final_page = true;
    pages.push(last);
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(tag: u8, inputs: &[u8]) -> (DerivationNode, DrvBlob) {
        let mut n =
            rio_test_support::fixtures::make_derivation_node(&format!("n{tag}"), "x86_64-linux");
        n.drv_digest = vec![tag; 32];
        n.input_drv_digests = inputs.iter().map(|b| vec![*b; 32]).collect();
        let blob = DrvBlob {
            digest: vec![tag; 32],
            drv_path: n.drv_path.clone(),
            body: vec![tag, tag, tag],
        };
        (n, blob)
    }

    fn frame(attr: &str, tags: &[(u8, &[u8])], root: Option<u8>) -> ResultFrame {
        let mut f = ResultFrame {
            attr: attr.into(),
            ..Default::default()
        };
        for (tag, inputs) in tags {
            let (n, b) = node(*tag, inputs);
            f.nodes.push(n);
            f.drv_blobs.push(b);
        }
        if let Some(r) = root {
            f.root_drv_digest = vec![r; 32];
        }
        f
    }

    // r[verify bc.submit.all-acked]
    #[test]
    fn gate_walks_missing_then_acks_then_ready() {
        let mut g = BuildGraph::default();
        // Root 3 depends on 1 and 2; frame delivers 3 and 1 only.
        let out = g
            .fold(frame("a", &[(3, &[1, 2]), (1, &[])], Some(3)))
            .unwrap();
        assert_eq!(out.new_drvs.len(), 2);
        assert_eq!(out.completed_root, Some([3u8; 32]));
        assert_eq!(g.root_gate(g.root(0)), RootGate::MissingNodes(1));

        g.fold(frame("a", &[(2, &[])], None)).unwrap();
        assert_eq!(g.root_gate(g.root(0)), RootGate::PendingAcks(3));

        for d in [[1u8; 32], [2u8; 32], [3u8; 32]] {
            g.mark_drv_acked(&d);
        }
        assert_eq!(g.root_gate(g.root(0)), RootGate::Ready);
    }

    /// A cyclic `input_drv_digests` graph (malicious or buggy eval
    /// parent — a real drv DAG cannot cycle) must not hang or
    /// stack-overflow the closure walk: the seen-set makes the
    /// iterative DFS terminate, the gate and submission still resolve,
    /// and the scheduler's submit-time verify is the authority that
    /// rejects a non-DAG skeleton.
    #[test]
    fn cyclic_inputs_terminate_gate_and_submission() {
        let mut g = BuildGraph::default();
        // 1 → 2 → 1 cycle, root 1.
        g.fold(frame("a", &[(1, &[2]), (2, &[1])], Some(1)))
            .unwrap();
        g.mark_drv_acked(&[1; 32]);
        g.mark_drv_acked(&[2; 32]);
        assert_eq!(g.root_gate(g.root(0)), RootGate::Ready);
        let (nodes, _) = g.submission_for(0);
        assert_eq!(nodes.len(), 2, "each cycle member appears once");

        // Self-loop terminates too.
        let mut g = BuildGraph::default();
        g.fold(frame("b", &[(3, &[3])], Some(3))).unwrap();
        g.mark_drv_acked(&[3; 32]);
        assert_eq!(g.root_gate(g.root(0)), RootGate::Ready);
    }

    #[test]
    fn sources_gate_their_attr_only() {
        let mut g = BuildGraph::default();
        let mut f = frame("a", &[(1, &[])], Some(1));
        f.source_roots.push(SourceRoot {
            store_path: "/nix/store/x-src".into(),
            dir_digest: vec![9; 32],
            nar_hash: vec![0; 32],
            nar_size: 1,
            origin: "/src".into(),
            // Old-worker shape (no root node): keys by dir_digest.
            root_node: None,
        });
        g.fold(f).unwrap();
        g.fold(frame("b", &[(2, &[])], Some(2))).unwrap();
        g.mark_drv_acked(&[1; 32]);
        g.mark_drv_acked(&[2; 32]);
        // Attr a waits on its source; attr b does not reference it.
        assert_eq!(g.root_gate(g.root(0)), RootGate::PendingAcks(1));
        assert_eq!(g.root_gate(g.root(1)), RootGate::Ready);
        g.mark_source_acked(&[9; 32]);
        assert_eq!(g.root_gate(g.root(0)), RootGate::Ready);
    }

    /// Multi-root overlap: nodes submitted with root A are excluded
    /// from root B's pages, and B's gate treats them as satisfied.
    // r[verify bc.submit.exclude-submitted]
    #[test]
    fn already_submitted_nodes_are_excluded() {
        let mut g = BuildGraph::default();
        // Shared leaf 1; roots 2 (a) and 3 (b) both depend on it.
        g.fold(frame("a", &[(1, &[]), (2, &[1])], Some(2))).unwrap();
        g.fold(frame("b", &[(3, &[1])], Some(3))).unwrap();
        for d in [[1u8; 32], [2u8; 32], [3u8; 32]] {
            g.mark_drv_acked(&d);
        }
        let (nodes_a, _) = g.submission_for(0);
        assert_eq!(nodes_a.len(), 2);
        g.claim_submitted(0, &[[1; 32], [2; 32]]);

        let (nodes_b, bodies_b) = g.submission_for(1);
        assert_eq!(nodes_b.len(), 1, "shared leaf must be excluded");
        assert_eq!(nodes_b[0].drv_digest, vec![3u8; 32]);
        // Claimed-but-not-accepted bodies stay retained (stale-ack
        // recovery may need them); after acceptance they drop.
        assert!(bodies_b.contains_key(&[1u8; 32]));
        assert!(bodies_b.contains_key(&[3u8; 32]));
        g.drop_bodies(&[[1; 32], [2; 32]]);
        let (_, bodies_after) = g.submission_for(1);
        assert!(!bodies_after.contains_key(&[1u8; 32]));
        assert_eq!(g.root_gate(g.root(1)), RootGate::Ready);
    }

    // r[verify bc.submit.paginate]
    #[test]
    fn pagination_splits_and_marks_final_page() {
        let nodes: Vec<DerivationNode> = (0..7)
            .map(|i| {
                let (n, _) = node(i as u8 + 1, &[]);
                n
            })
            .collect();
        let opts = SubmitOptions {
            priority_class: "interactive".into(),
            tenant_name: "t".into(),
            keep_going: true,
        };
        let pages = paginate(nodes.clone(), &opts, 3, "sub-1");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].nodes.len(), 3);
        assert!(!pages[0].final_page);
        assert!(pages[0].priority_class.is_empty());
        assert_eq!(pages[2].nodes.len(), 1);
        assert!(pages[2].final_page);
        assert_eq!(pages[2].priority_class, "interactive");
        assert!(pages[2].keep_going);
        assert!(pages.iter().all(|p| p.submission_id == "sub-1"));
        let total: usize = pages.iter().map(|p| p.nodes.len()).sum();
        assert_eq!(total, 7);

        // Below the limit: one unpaged request, no submission_id.
        let single = paginate(nodes, &opts, 100, "sub-2");
        assert_eq!(single.len(), 1);
        assert!(single[0].submission_id.is_empty());
    }
}
