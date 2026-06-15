//! End-to-end coordinator pipeline tests (ADR-024 P3a): a scripted
//! eval-parent stub feeds real canonical drv closures over the
//! `rio.evaljob` channel; the coordinator negotiates, uploads, and
//! submits against REAL in-process rio-store services (ephemeral
//! postgres) and a purpose-built scheduler stub that runs the actual
//! digest bulk-verify against the store's `drv_blobs` table.

mod common;
mod drvgen;
mod fake_daemon;
mod stub_parent;
mod stub_scheduler;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;

use common::{TestCluster, single_frame};
use rio_build_cli::coordinator::OutcomeState;
use rio_evalstore::dirblob::{BuiltDir, BuiltEntry};
use rio_evalstore::ingest::{IngestConfig, IngestNode, chunk_bytes, ingest_tree};
use rio_proto::evaljob::{ResultFrame, SourceRoot};

type TestResult = anyhow::Result<()>;

/// Ingest a real temp tree and report it the way an eval worker would.
fn source_root_for(origin: &Path, name: &str) -> SourceRoot {
    fn build(node: &IngestNode) -> BuiltEntry {
        match node {
            IngestNode::File(f) => BuiltEntry::File {
                digest: rio_packstore::Digest(f.digest),
                size: f.size,
                executable: f.executable,
            },
            IngestNode::Symlink(s) => BuiltEntry::Symlink {
                target: s.target.clone(),
            },
            IngestNode::Dir(d) => {
                let mut b = BuiltDir::new();
                for e in &d.entries {
                    b.push(e.name.clone(), build(&e.node));
                }
                BuiltEntry::Dir(b)
            }
        }
    }
    let result = ingest_tree(origin, &IngestConfig::default()).expect("ingest");
    let BuiltEntry::Dir(dir) = build(&result.root) else {
        panic!("source fixture must be a directory");
    };
    let folded = dir.fold().expect("fold");
    SourceRoot {
        store_path: drvgen::fake_out_path(name),
        dir_digest: folded.root_digest.0.to_vec(),
        nar_hash: result.nar_sha256.to_vec(),
        nar_size: result.nar_size,
        origin: origin.to_str().expect("utf8 temp path").to_string(),
        root_node: Some(rio_proto::castore::RootNode {
            node: Some(rio_proto::castore::root_node::Node::DirDigest(
                folded.root_digest.0.to_vec(),
            )),
        }),
    }
}

/// Ingest a real single-file or symlink origin and report it the way an
/// eval worker would: inline castore root node, empty `dir_digest`.
fn leaf_source_root(origin: &Path, name: &str) -> SourceRoot {
    use rio_proto::castore::{FileEntry, RootNode, SymlinkEntry, root_node::Node};
    let result = ingest_tree(origin, &IngestConfig::default()).expect("ingest");
    let node = match &result.root {
        IngestNode::File(f) => Node::File(FileEntry {
            name: vec![],
            digest: f.digest.to_vec(),
            size: f.size,
            executable: f.executable,
        }),
        IngestNode::Symlink(s) => Node::Symlink(SymlinkEntry {
            name: vec![],
            target: s.target.clone(),
        }),
        IngestNode::Dir(_) => panic!("leaf fixture must not be a directory"),
    };
    SourceRoot {
        store_path: drvgen::fake_out_path(name),
        dir_digest: vec![],
        nar_hash: result.nar_sha256.to_vec(),
        nar_size: result.nar_size,
        origin: origin.to_str().expect("utf8 temp path").to_string(),
        root_node: Some(RootNode { node: Some(node) }),
    }
}

/// Seed a streamed (no origin tree on disk) source into the cluster's
/// client CAS via a NAR ingest — the shape a fetched flake input lands
/// as — and report it with an empty origin so the coordinator must
/// serve the upload from the CAS. Returns the SourceRoot.
fn streamed_source_root(
    cas_root: &Path,
    name: &str,
    nar_root: &rio_nix::nar::NarNode,
) -> SourceRoot {
    use rio_proto::castore::{FileEntry, RootNode, SymlinkEntry, root_node::Node};
    use sha2::Digest as _;

    let store = rio_evalstore::EvalStore::open(Some(cas_root.to_str().expect("utf8 cas root")))
        .expect("open CAS");
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, nar_root).expect("serialize NAR");
    let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
    let store_path = drvgen::fake_out_path(name);
    store
        .add_nar(
            &rio_evalstore::store::ProvidedInfo {
                path: store_path.clone(),
                nar_hash: hex::encode(nar_hash),
                nar_size: nar.len() as u64,
                references: vec![],
                ca: None,
            },
            &mut nar.as_slice(),
        )
        .expect("add_nar");
    store.flush().expect("flush CAS");

    // Castore identity for the same content, computed the way the eval
    // store does (shared dirblob fold / blake3 keys).
    fn build(node: &rio_nix::nar::NarNode) -> BuiltEntry {
        match node {
            rio_nix::nar::NarNode::Regular {
                executable,
                contents,
            } => BuiltEntry::File {
                digest: rio_packstore::Digest(*blake3::hash(contents).as_bytes()),
                size: contents.len() as u64,
                executable: *executable,
            },
            rio_nix::nar::NarNode::Symlink { target } => BuiltEntry::Symlink {
                target: target.clone().into_bytes(),
            },
            rio_nix::nar::NarNode::Directory { entries } => {
                let mut b = BuiltDir::new();
                for e in entries {
                    b.push(e.name.as_bytes(), build(&e.node));
                }
                BuiltEntry::Dir(b)
            }
        }
    }
    let (dir_digest, node) = match build(nar_root) {
        BuiltEntry::Dir(dir) => {
            let folded = dir.fold().expect("fold");
            (
                folded.root_digest.0.to_vec(),
                Node::DirDigest(folded.root_digest.0.to_vec()),
            )
        }
        BuiltEntry::File {
            digest,
            size,
            executable,
        } => (
            vec![],
            Node::File(FileEntry {
                name: vec![],
                digest: digest.0.to_vec(),
                size,
                executable,
            }),
        ),
        BuiltEntry::Symlink { target } => (
            vec![],
            Node::Symlink(SymlinkEntry {
                name: vec![],
                target,
            }),
        ),
    };
    SourceRoot {
        store_path,
        dir_digest,
        nar_hash: nar_hash.to_vec(),
        nar_size: nar.len() as u64,
        origin: String::new(),
        root_node: Some(RootNode { node: Some(node) }),
    }
}

fn write_source_tree(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("default.nix"), b"{ }: null\n").unwrap();
    std::fs::write(dir.join("src/main.c"), vec![0x42; 100_000]).unwrap();
    std::fs::write(dir.join("src/util.c"), b"static int x;\n").unwrap();
}

/// Count of committed narinfo rows for one store path.
async fn narinfo_rows(cluster: &TestCluster, store_path: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM narinfo WHERE store_path = $1")
        .bind(store_path)
        .fetch_one(&cluster.db.pool)
        .await
        .expect("narinfo count")
}

/// Cold submit end-to-end: a 3-node closure plus one real source tree
/// — the coordinator negotiates (all misses), uploads drv blobs and
/// the chunked source, submits one digest-bearing DAG, and renders the
/// synthetic BuildEvents to a Completed outcome.
#[tokio::test]
async fn cold_submit_end_to_end() -> TestResult {
    let cluster = TestCluster::new().await?;
    let src_dir = tempfile::tempdir()?;
    write_source_tree(src_dir.path());
    let src = source_root_for(src_dir.path(), "hello-src");

    let chain = drvgen::chain(&["cold-leaf", "cold-mid", "cold-root"]);
    let root = &chain[2];
    let script = HashMap::from([(
        "pkgs.hello".to_string(),
        vec![single_frame(
            "pkgs.hello",
            &[&chain[0], &chain[1], root],
            vec![src.clone()],
            root,
        )],
    )]);

    let mut coordinator = cluster.coordinator(|_| {});
    let (summary, parent) = cluster
        .run(&mut coordinator, script, &["pkgs.hello"])
        .await?;

    assert!(!summary.detached);
    assert_eq!(summary.outcomes.len(), 1);
    let o = &summary.outcomes[0];
    assert_eq!(o.attr, "pkgs.hello");
    assert_eq!(
        o.state,
        OutcomeState::Completed {
            output_paths: vec![root.out_path.clone()]
        },
        "build must complete with the root's output"
    );
    assert!(
        o.drv_events.iter().any(|(p, _)| p == &chain[0].drv_path),
        "per-drv events must be rendered from the stream"
    );

    // All three canonical blobs landed in the real store (server-side
    // verified: digest, canonical bytes, drv_path recompute).
    assert_eq!(cluster.drv_blob_count().await, 3);

    // The submission is digest-bearing, edge-free, single-page.
    // (Block-scoped: clippy's await_holding_lock is lexical.)
    {
        let st = cluster.sched.state.lock().unwrap();
        assert_eq!(st.accepted.len(), 1);
        let sub = &st.accepted[0];
        assert_eq!(sub.nodes.len(), 3);
        assert!(sub.edges.is_empty(), "edges retire under ADR-024");
        assert!(sub.nodes.iter().all(|n| n.drv_digest.len() == 32));
        assert!(sub.submission_id.is_empty(), "below the page threshold");
    }

    // The chunked source upload committed the real narinfo row.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo WHERE store_path = $1")
        .bind(&src.store_path)
        .fetch_one(&cluster.db.pool)
        .await?;
    assert_eq!(n, 1, "source tree committed via PutPathChunked");

    // Ack feedback reached the eval parent (re-fork pre-warm path).
    let seen = parent.seen.lock().unwrap();
    assert!(seen.shutdown, "coordinator must send Shutdown");
    assert_eq!(seen.ack_digests.len(), 3, "drv acks fed back");
    Ok(())
}

/// Warm path: a second invocation against the same CAS root finds
/// every ack in the persistent table — zero `HasDrvs` probes, zero
/// `PutDrvBlobs`, immediate submit.
// r[verify bc.negotiate.ack-short-circuit]
#[tokio::test]
async fn warm_acks_skip_negotiation_and_upload() -> TestResult {
    let cluster = TestCluster::new().await?;
    let chain = drvgen::chain(&["warm-leaf", "warm-root"]);
    let root = &chain[1];
    let script = || {
        HashMap::from([(
            "a".to_string(),
            vec![single_frame("a", &[&chain[0], root], vec![], root)],
        )])
    };

    let mut cold = cluster.coordinator(|_| {});
    cluster.run(&mut cold, script(), &["a"]).await?;
    assert!(cluster.drv_put_calls.load(Ordering::SeqCst) >= 1);

    cluster.drv_has_calls.store(0, Ordering::SeqCst);
    cluster.drv_put_calls.store(0, Ordering::SeqCst);

    // Fresh coordinator, same CAS root: the ack table survived the
    // "process restart" — that's its point.
    let mut warm = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut warm, script(), &["a"]).await?;
    assert!(matches!(
        summary.outcomes[0].state,
        OutcomeState::Completed { .. }
    ));
    assert_eq!(
        cluster.drv_has_calls.load(Ordering::SeqCst),
        0,
        "warm acks must short-circuit negotiation"
    );
    assert_eq!(
        cluster.drv_put_calls.load(Ordering::SeqCst),
        0,
        "warm acks must skip every upload"
    );
    assert_eq!(
        cluster.sched.state.lock().unwrap().accepted.len(),
        2,
        "the warm run still submits (cluster verifies from drv_blobs)"
    );
    Ok(())
}

/// Stale-ack recovery: the cluster GC'd a blob the client's ack table
/// remembers. The submit is rejected naming the digest; the client
/// evicts the ack, re-Has-es, re-uploads from its retained body, and
/// resubmits — exactly one recovery cycle.
// r[verify bc.submit.stale-ack-once]
#[tokio::test]
async fn stale_ack_recovery_single_cycle() -> TestResult {
    let cluster = TestCluster::new().await?;
    let chain = drvgen::chain(&["stale-leaf", "stale-mid", "stale-root"]);
    let root = &chain[2];
    let script = || {
        HashMap::from([(
            "a".to_string(),
            vec![single_frame(
                "a",
                &[&chain[0], &chain[1], root],
                vec![],
                root,
            )],
        )])
    };

    let mut cold = cluster.coordinator(|_| {});
    cluster.run(&mut cold, script(), &["a"]).await?;
    assert_eq!(cluster.drv_blob_count().await, 3);

    // "Cluster GC": the mid blob vanishes server-side; the client's
    // ack table still says present.
    cluster.delete_drv_blob(&chain[1].digest).await;
    assert_eq!(cluster.drv_blob_count().await, 2);
    cluster.drv_put_calls.store(0, Ordering::SeqCst);

    let mut warm = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut warm, script(), &["a"]).await?;
    assert!(
        matches!(summary.outcomes[0].state, OutcomeState::Completed { .. }),
        "recovery must converge: {:?}",
        summary.outcomes[0].state
    );
    {
        let st = cluster.sched.state.lock().unwrap();
        assert_eq!(st.rejects, 1, "exactly one FAILED_PRECONDITION cycle");
        assert_eq!(st.accepted.len(), 2, "cold accept + recovered accept");
    }
    assert!(
        cluster.drv_put_calls.load(Ordering::SeqCst) >= 1,
        "recovery must re-upload the missing blob"
    );
    assert_eq!(cluster.drv_blob_count().await, 3, "blob restored");
    Ok(())
}

/// A second reject is a hard error, not a loop: `force_missing` makes
/// the scheduler name a digest missing forever (it IS present, so the
/// recovery's re-Has finds it and uploads nothing) — the client must
/// give up after the single permitted recovery cycle.
// r[verify bc.submit.stale-ack-once]
#[tokio::test]
async fn stale_ack_second_reject_is_hard_error() -> TestResult {
    let cluster = TestCluster::new().await?;
    let chain = drvgen::chain(&["hard-leaf", "hard-root"]);
    let root = &chain[1];
    let script = || {
        HashMap::from([(
            "a".to_string(),
            vec![single_frame("a", &[&chain[0], root], vec![], root)],
        )])
    };

    let mut cold = cluster.coordinator(|_| {});
    cluster.run(&mut cold, script(), &["a"]).await?;

    cluster
        .sched
        .state
        .lock()
        .unwrap()
        .force_missing
        .insert(chain[0].digest.to_vec());

    let mut warm = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut warm, script(), &["a"]).await?;
    let OutcomeState::Failed { message } = &summary.outcomes[0].state else {
        panic!("second reject must fail: {:?}", summary.outcomes[0].state);
    };
    assert!(
        message.contains("rejected twice"),
        "hard error names the contract: {message}"
    );
    assert_eq!(cluster.sched.state.lock().unwrap().rejects, 2);
    Ok(())
}

/// Chunk stale-ack recovery (the production GC-grace-vs-ack-TTL hole):
/// a chunk the client's ack table remembers loses its S3 object while
/// its presence row keeps claiming durable. A later upload that dedups
/// against it is rejected UNAVAILABLE naming the digest; the client
/// evicts the chunk ack, re-HasChunks-es (the store demoted the lying
/// row, so the probe now answers absent), re-streams the chunk, and the
/// retried upload — and the whole build — completes.
// r[verify bc.upload.stale-ack-once]
#[tokio::test]
async fn stale_chunk_ack_recovery_reuploads_and_completes() -> TestResult {
    // The chunk-backend fault injection goes through the trait surface.
    use rio_store::backend::ChunkBackend as _;

    let cluster = TestCluster::new().await?;

    // Run 1: a source tree whose payload spans several FastCDC chunks.
    let payload = rio_test_support::fixtures::pseudo_random_bytes(0xC4A5, 1 << 20);
    let (_, payload_chunks) = chunk_bytes(&payload);
    assert!(
        payload_chunks.len() >= 2,
        "fixture must span multiple chunks (got {})",
        payload_chunks.len()
    );
    let dir1 = tempfile::tempdir()?;
    std::fs::create_dir_all(dir1.path().join("src"))?;
    std::fs::write(dir1.path().join("src/blob.bin"), &payload)?;
    let src1 = source_root_for(dir1.path(), "chunkstale-src-v1");

    let chain1 = drvgen::chain(&["chunkstale-leaf", "chunkstale-root"]);
    let root1 = &chain1[1];
    let script1 = HashMap::from([(
        "a".to_string(),
        vec![single_frame(
            "a",
            &[&chain1[0], root1],
            vec![src1.clone()],
            root1,
        )],
    )]);
    let mut cold = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut cold, script1, &["a"]).await?;
    assert!(matches!(
        summary.outcomes[0].state,
        OutcomeState::Completed { .. }
    ));

    // The hole: the first chunk's S3 object disappears while its
    // `chunks` row keeps claiming durable presence and the client's ack
    // table keeps remembering the ack.
    let victim = payload_chunks[0].digest;
    cluster
        .backend
        .delete_by_key(&cluster.backend.key_for(&victim))
        .await?;

    // Run 2: a tree whose payload shares the victim chunk (identical
    // prefix, so FastCDC reproduces the same first cut) but ends in new
    // content — its whole-file digest has no committed binding, so the
    // store must fetch the victim back to verify it and discovers the
    // miss.
    let prefix_len = (payload_chunks[0].offset + u64::from(payload_chunks[0].len)) as usize;
    let mut payload2 = payload[..prefix_len].to_vec();
    payload2.extend(rio_test_support::fixtures::pseudo_random_bytes(
        0xC4A6,
        512 * 1024,
    ));
    let dir2 = tempfile::tempdir()?;
    std::fs::create_dir_all(dir2.path().join("src"))?;
    std::fs::write(dir2.path().join("src/blob.bin"), &payload2)?;
    let src2 = source_root_for(dir2.path(), "chunkstale-src-v2");

    let chain2 = drvgen::chain(&["chunkstale2-leaf", "chunkstale2-root"]);
    let root2 = &chain2[1];
    let script2 = HashMap::from([(
        "b".to_string(),
        vec![single_frame(
            "b",
            &[&chain2[0], root2],
            vec![src2.clone()],
            root2,
        )],
    )]);
    let mut warm = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut warm, script2, &["b"]).await?;
    assert!(
        matches!(summary.outcomes[0].state, OutcomeState::Completed { .. }),
        "chunk stale-ack recovery must converge: {:?}",
        summary.outcomes[0].state
    );

    // Recovery re-streamed the victim chunk: the object is back in the
    // backend, presence is honest again, and the v2 source committed.
    assert!(
        cluster.backend.exists_batch(&[victim]).await?[0],
        "recovery must re-upload the missing chunk object"
    );
    let durable: bool =
        sqlx::query_scalar("SELECT durable AND NOT deleted FROM chunks WHERE blake3_hash = $1")
            .bind(victim.as_slice())
            .fetch_one(&cluster.db.pool)
            .await?;
    assert!(durable, "the re-uploaded chunk answers HasChunks again");
    assert_eq!(narinfo_rows(&cluster, &src2.store_path).await, 1);
    Ok(())
}

/// Two attrs sharing a leaf: the second root's submission excludes the
/// node the first already submitted this session (the scheduler
/// resolves the cross-submission digest from the store).
// r[verify bc.submit.exclude-submitted]
// r[verify bc.fold.dedup-by-digest]
#[tokio::test]
async fn multi_root_excludes_already_submitted_nodes() -> TestResult {
    let cluster = TestCluster::new().await?;
    let leaf = drvgen::make_drv("shared-leaf", &[], &[]);
    let root_a = drvgen::make_drv("root-a", &[&leaf], &[]);
    let root_b = drvgen::make_drv("root-b", &[&leaf], &[]);

    let script = HashMap::from([
        (
            "a".to_string(),
            vec![single_frame("a", &[&leaf, &root_a], vec![], &root_a)],
        ),
        (
            "b".to_string(),
            // The worker for b reports the shared leaf too — the
            // coordinator's fold dedups it by digest.
            vec![single_frame("b", &[&leaf, &root_b], vec![], &root_b)],
        ),
    ]);

    let mut coordinator = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut coordinator, script, &["a", "b"]).await?;
    assert_eq!(summary.outcomes.len(), 2);
    assert!(
        summary
            .outcomes
            .iter()
            .all(|o| matches!(o.state, OutcomeState::Completed { .. })),
        "{:?}",
        summary
            .outcomes
            .iter()
            .map(|o| &o.state)
            .collect::<Vec<_>>()
    );

    let st = cluster.sched.state.lock().unwrap();
    assert_eq!(st.accepted.len(), 2);
    let total: usize = st.accepted.iter().map(|s| s.nodes.len()).sum();
    assert_eq!(total, 3, "shared leaf must ship exactly once");
    // Whichever submission went second carries only its root.
    let second = &st.accepted[1];
    assert_eq!(second.nodes.len(), 1);
    assert!(
        second.nodes[0].input_drv_digests[0] == leaf.digest.to_vec(),
        "the excluded leaf is still referenced by digest"
    );
    Ok(())
}

/// `.#checks`-style attrset installable: the worker answers with an
/// `AttrsetExpansion`; the coordinator queues each child as its own
/// build root named by its full attr path, dedups a child that was
/// also requested explicitly, and the run completes with one outcome
/// per derivation child.
// r[verify bc.eval.attrset-expansion]
#[tokio::test]
async fn attrset_expansion_builds_each_child_once() -> TestResult {
    let cluster = TestCluster::new().await?;
    let alpha = drvgen::make_drv("exp-alpha", &[], &[]);
    let beta = drvgen::make_drv("exp-beta", &[], &[]);
    let alpha_attr = "checks.x86_64-linux.alpha";
    let beta_attr = "checks.x86_64-linux.beta";

    let script = HashMap::from([
        (
            alpha_attr.to_string(),
            vec![single_frame(alpha_attr, &[&alpha], vec![], &alpha)],
        ),
        (
            beta_attr.to_string(),
            vec![single_frame(beta_attr, &[&beta], vec![], &beta)],
        ),
    ]);
    let expansions = HashMap::from([(
        "checks".to_string(),
        rio_proto::evaljob::AttrsetExpansion {
            attr: "checks".into(),
            children: vec![alpha_attr.to_string(), beta_attr.to_string()],
            skipped: vec!["checks.x86_64-linux.not-a-check".to_string()],
        },
    )]);

    // The attrset AND one of its children are requested explicitly —
    // the expanded child must not become a second root.
    let mut coordinator = cluster.coordinator(|_| {});
    let (summary, parent) = cluster
        .run_expanding(
            &mut coordinator,
            script,
            expansions,
            &["checks", alpha_attr],
        )
        .await?;

    assert_eq!(summary.outcomes.len(), 2, "{:?}", summary.outcomes);
    let mut attrs: Vec<&str> = summary.outcomes.iter().map(|o| o.attr.as_str()).collect();
    attrs.sort_unstable();
    assert_eq!(
        attrs,
        vec![alpha_attr, beta_attr],
        "roots are named by the worker-reported child attr paths"
    );
    assert!(
        summary
            .outcomes
            .iter()
            .all(|o| matches!(o.state, OutcomeState::Completed { .. })),
        "{:?}",
        summary.outcomes
    );

    // One submission per child root, none for the expanded attr itself.
    assert_eq!(cluster.sched.state.lock().unwrap().accepted.len(), 2);
    assert!(parent.seen.lock().unwrap().shutdown);
    Ok(())
}

/// >50k nodes paginate: non-final pages stage under one submission_id
/// and ack with an empty close; the final page carries the remainder
/// and the build options; the scheduler assembles and verifies the
/// whole set.
// r[verify bc.submit.paginate]
#[tokio::test]
async fn pagination_above_node_threshold() -> TestResult {
    let cluster = TestCluster::new().await?;

    const LEAVES: usize = 50_500;
    let mut fixtures: Vec<drvgen::DrvFixture> = Vec::with_capacity(LEAVES + 1);
    for i in 0..LEAVES {
        fixtures.push(drvgen::make_drv(&format!("wide-{i}"), &[], &[]));
    }
    let leaf_refs: Vec<&drvgen::DrvFixture> = fixtures.iter().collect();
    let root = drvgen::make_drv("wide-root", &leaf_refs, &[]);
    fixtures.push(root.clone());

    // Stream the skeleton in batches like a real worker (also keeps
    // each frame far below the framing cap).
    let mut frames: Vec<ResultFrame> = Vec::new();
    for chunk in fixtures.chunks(10_000) {
        frames.push(ResultFrame {
            attr: "wide".into(),
            nodes: chunk.iter().map(|f| f.node.clone()).collect(),
            drv_blobs: chunk.iter().map(|f| f.blob.clone()).collect(),
            source_roots: vec![],
            root_drv_digest: vec![],
        });
    }
    frames
        .last_mut()
        .expect("at least one frame")
        .root_drv_digest = root.digest.to_vec();
    let script = HashMap::from([("wide".to_string(), frames)]);

    let mut coordinator = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut coordinator, script, &["wide"]).await?;
    assert!(matches!(
        summary.outcomes[0].state,
        OutcomeState::Completed { .. }
    ));

    let st = cluster.sched.state.lock().unwrap();
    assert_eq!(st.accepted.len(), 1);
    assert_eq!(st.accepted[0].nodes.len(), LEAVES + 1, "assembled whole");
    // Page shape: one staged page at the 50k threshold + the final
    // remainder, sharing a submission_id.
    assert_eq!(st.pages.len(), 2, "pages: {:?}", st.pages);
    let (id0, final0, n0) = &st.pages[0];
    let (id1, final1, n1) = &st.pages[1];
    assert_eq!(id0, id1, "pages share the client-chosen submission_id");
    assert!(!id0.is_empty());
    assert!(!final0 && *final1);
    assert_eq!(*n0, 50_000);
    assert_eq!(*n1, LEAVES + 1 - 50_000);
    Ok(())
}

/// A source tree mutated between eval and upload must fail the root
/// loudly — the upload-time re-read recomputes the NAR hash and root
/// digest and refuses to ship content the skeleton never referenced.
// r[verify bc.upload.origin-reread+2]
#[tokio::test]
async fn mutated_origin_fails_upload_loudly() -> TestResult {
    let cluster = TestCluster::new().await?;
    let src_dir = tempfile::tempdir()?;
    write_source_tree(src_dir.path());
    let src = source_root_for(src_dir.path(), "mutating-src");
    // Mutate AFTER eval reported the digests.
    std::fs::write(src_dir.path().join("src/util.c"), b"int y; /* changed */\n")?;

    let root = drvgen::make_drv("mut-root", &[], &[]);
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&root], vec![src], &root)],
    )]);
    let mut coordinator = cluster.coordinator(|_| {});
    let Err(err) = cluster.run(&mut coordinator, script, &["a"]).await else {
        panic!("mutated origin must fail the run");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("changed since eval") || msg.contains("folds to root digest"),
        "error must name the mutation: {msg}"
    );
    Ok(())
}

/// Single-file and symlink source roots upload via `PutPathChunked`
/// with inline castore root nodes — no Directory DAG, no
/// `HasDirectories` probe — and commit real narinfo rows.
// r[verify bc.upload.source-root-kinds]
#[tokio::test]
async fn file_and_symlink_source_roots_upload() -> TestResult {
    let cluster = TestCluster::new().await?;
    let dir = tempfile::tempdir()?;
    let patch = dir.path().join("fix.patch");
    std::fs::write(&patch, b"--- a/x\n+++ b/x\n@@ patch body @@\n")?;
    let link = dir.path().join("link");
    std::os::unix::fs::symlink("fix.patch", &link)?;
    let src_file = leaf_source_root(&patch, "fix-patch");
    let src_link = leaf_source_root(&link, "fix-link");

    let root = drvgen::make_drv("leafsrc-root", &[], &[]);
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame(
            "a",
            &[&root],
            vec![src_file.clone(), src_link.clone()],
            &root,
        )],
    )]);
    let mut coordinator = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut coordinator, script, &["a"]).await?;
    assert!(
        matches!(summary.outcomes[0].state, OutcomeState::Completed { .. }),
        "{:?}",
        summary.outcomes[0].state
    );
    assert_eq!(narinfo_rows(&cluster, &src_file.store_path).await, 1);
    assert_eq!(narinfo_rows(&cluster, &src_link.store_path).await, 1);
    Ok(())
}

/// Streamed source roots (empty origin — fetched flake inputs, toFile
/// text) are served from the client CAS: a single file and a directory
/// tree with no origin path on disk both upload and commit narinfo
/// rows.
// r[verify bc.upload.cas-read]
#[tokio::test]
async fn streamed_source_roots_upload_from_client_cas() -> TestResult {
    use rio_nix::nar::{NarEntry, NarNode};

    let cluster = TestCluster::new().await?;
    let src_file = streamed_source_root(
        cluster.cas.path(),
        "fetched-patch",
        &NarNode::Regular {
            executable: false,
            contents: b"+ patched line out of a fetched input\n".to_vec(),
        },
    );
    let src_dir = streamed_source_root(
        cluster.cas.path(),
        "fetched-input",
        &NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "data.txt".into(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"streamed dir data\n".to_vec(),
                    },
                },
                NarEntry {
                    name: "docs".into(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "run.sh".into(),
                            node: NarNode::Regular {
                                executable: true,
                                contents: b"#!/bin/sh\necho hi\n".to_vec(),
                            },
                        }],
                    },
                },
                NarEntry {
                    name: "link".into(),
                    node: NarNode::Symlink {
                        target: "data.txt".into(),
                    },
                },
            ],
        },
    );

    let root = drvgen::make_drv("cas-src-root", &[], &[]);
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame(
            "a",
            &[&root],
            vec![src_file.clone(), src_dir.clone()],
            &root,
        )],
    )]);
    let mut coordinator = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut coordinator, script, &["a"]).await?;
    assert!(
        matches!(summary.outcomes[0].state, OutcomeState::Completed { .. }),
        "{:?}",
        summary.outcomes[0].state
    );
    assert_eq!(narinfo_rows(&cluster, &src_file.store_path).await, 1);
    assert_eq!(narinfo_rows(&cluster, &src_dir.store_path).await, 1);
    Ok(())
}

/// Warm second run with sources: every source ack is in the persistent
/// table, so the coordinator never re-reads an origin tree and never
/// re-reads the client CAS — proven structurally by deleting the
/// origins AND the CAS pack records before the warm run (a re-read
/// would hard-fail, an ack hit cannot).
// r[verify bc.negotiate.ack-short-circuit]
#[tokio::test]
async fn warm_source_acks_skip_origin_and_cas_reads() -> TestResult {
    let cluster = TestCluster::new().await?;
    let dir = tempfile::tempdir()?;
    let tree = dir.path().join("tree");
    write_source_tree(&tree);
    let patch = dir.path().join("warm.patch");
    std::fs::write(&patch, b"warm patch body\n")?;

    let src_tree = source_root_for(&tree, "warm-tree");
    let src_file = leaf_source_root(&patch, "warm-patch");
    let src_streamed = streamed_source_root(
        cluster.cas.path(),
        "warm-fetched",
        &rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: b"warm streamed body\n".to_vec(),
        },
    );

    let chain = drvgen::chain(&["warmsrc-leaf", "warmsrc-root"]);
    let root = &chain[1];
    let script = || {
        HashMap::from([(
            "a".to_string(),
            vec![single_frame(
                "a",
                &[&chain[0], root],
                vec![src_tree.clone(), src_file.clone(), src_streamed.clone()],
                root,
            )],
        )])
    };

    let mut cold = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut cold, script(), &["a"]).await?;
    assert!(matches!(
        summary.outcomes[0].state,
        OutcomeState::Completed { .. }
    ));

    // No origin, no CAS records: only the ack table can satisfy the
    // warm run's source handling.
    std::fs::remove_dir_all(&tree)?;
    std::fs::remove_file(&patch)?;
    std::fs::remove_dir_all(cluster.cas.path().join("packs"))?;

    let mut warm = cluster.coordinator(|_| {});
    let (summary, _) = cluster.run(&mut warm, script(), &["a"]).await?;
    assert!(
        matches!(summary.outcomes[0].state, OutcomeState::Completed { .. }),
        "warm run must complete on ack hits alone: {:?}",
        summary.outcomes[0].state
    );
    Ok(())
}

/// A single-file origin mutated between eval and upload fails that
/// root loudly (the upload-time re-read recomputes the NAR hash),
/// mirroring the directory-tree case above.
// r[verify bc.upload.origin-reread+2]
#[tokio::test]
async fn mutated_file_origin_fails_upload_loudly() -> TestResult {
    let cluster = TestCluster::new().await?;
    let dir = tempfile::tempdir()?;
    let patch = dir.path().join("mut.patch");
    std::fs::write(&patch, b"original body\n")?;
    let src = leaf_source_root(&patch, "mutating-patch");
    // Mutate AFTER eval reported the digests.
    std::fs::write(&patch, b"tampered body\n")?;

    let root = drvgen::make_drv("mut-file-root", &[], &[]);
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&root], vec![src], &root)],
    )]);
    let mut coordinator = cluster.coordinator(|_| {});
    let Err(err) = cluster.run(&mut coordinator, script, &["a"]).await else {
        panic!("mutated single-file origin must fail the run");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("changed since eval"),
        "error must name the mutation: {msg}"
    );
    Ok(())
}

/// `--detach` + interrupt: nothing is cancelled, the build outlives
/// the client (the stream is dropped on detach), and `--attach`
/// re-streams the full event log — including events emitted while
/// nobody watched — to the terminal.
// r[verify bc.interrupt.detach-flag]
#[tokio::test]
async fn detach_then_attach_resumes_stream() -> TestResult {
    let cluster = TestCluster::new().await?;
    cluster.sched.state.lock().unwrap().hold_open = true;

    let chain = drvgen::chain(&["det-leaf", "det-root"]);
    let root = &chain[1];
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&chain[0], root], vec![], root)],
    )]);

    let mut coordinator = cluster.coordinator(|opts| opts.detach_on_interrupt = true);
    let summary = cluster
        .run_interrupted(&mut coordinator, script, &["a"], 1, 1)
        .await?;
    assert!(summary.interrupted);
    assert!(summary.detached);
    let detached: Vec<_> = summary
        .outcomes
        .iter()
        .filter(|o| o.state == OutcomeState::Detached)
        .collect();
    assert_eq!(detached.len(), 1, "{:?}", summary.outcomes);
    let build_id = detached[0].build_id.clone();
    assert!(!build_id.is_empty());
    assert!(
        cluster.sched.state.lock().unwrap().cancel_calls.is_empty(),
        "--detach must not send CancelBuild"
    );

    // The build finishes while no client is attached.
    let log = cluster.sched.log(&build_id);
    log.append(
        &build_id,
        rio_proto::types::build_event::Event::Derivation(rio_proto::types::DerivationEvent {
            derivation_path: root.drv_path.clone(),
            kind: rio_proto::types::DerivationEventKind::Completed as i32,
            output_paths: vec![root.out_path.clone()],
            ..Default::default()
        }),
    );
    log.append(
        &build_id,
        rio_proto::types::build_event::Event::Completed(rio_proto::types::BuildCompleted {
            output_paths: vec![root.out_path.clone()],
        }),
    );

    // --attach re-streams from sequence 0 and reaches the terminal.
    let mut clients = cluster.clients.clone();
    let outcome = rio_build_cli::coordinator::attach_build(
        &mut clients,
        &build_id,
        0,
        rio_build_cli::render::RenderHandle::null(),
        rio_build_cli::coordinator::FailureLogOpts::default(),
    )
    .await?;
    assert_eq!(outcome.build_id, build_id);
    assert_eq!(
        outcome.state,
        OutcomeState::Completed {
            output_paths: vec![root.out_path.clone()]
        }
    );
    assert!(
        outcome.drv_events.iter().any(|(p, _)| p == &root.drv_path),
        "replay must include events emitted while detached"
    );
    Ok(())
}

/// Default interrupt: every build this invocation submitted is
/// cancelled via the CancelBuild RPC, the outcome is Cancelled, and
/// the summary marks the run interrupted (non-zero exit).
// r[verify bc.interrupt.cancel-default]
#[tokio::test]
async fn interrupt_cancels_submitted_builds() -> TestResult {
    let cluster = TestCluster::new().await?;
    cluster.sched.state.lock().unwrap().hold_open = true;

    let chain = drvgen::chain(&["int-leaf", "int-root"]);
    let root = &chain[1];
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&chain[0], root], vec![], root)],
    )]);

    let mut coordinator = cluster.coordinator(|_| {});
    let summary = cluster
        .run_interrupted(&mut coordinator, script, &["a"], 1, 1)
        .await?;
    assert!(summary.interrupted);
    assert!(!summary.detached);
    let cancelled: Vec<_> = summary
        .outcomes
        .iter()
        .filter(|o| matches!(o.state, OutcomeState::Cancelled { .. }))
        .collect();
    assert_eq!(cancelled.len(), 1, "{:?}", summary.outcomes);
    let build_id = cancelled[0].build_id.clone();
    assert!(!build_id.is_empty());

    let st = cluster.sched.state.lock().unwrap();
    assert_eq!(
        st.cancel_calls.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        vec![&build_id],
        "the interrupt must cancel exactly this invocation's build"
    );
    Ok(())
}

/// Interrupt cancellation is scoped to this invocation: a build left
/// running by an earlier (detached) invocation is never cancelled by a
/// later invocation's Ctrl-C — only the later invocation's own build
/// is.
// r[verify bc.interrupt.scope]
#[tokio::test]
async fn interrupt_never_cancels_foreign_builds() -> TestResult {
    let cluster = TestCluster::new().await?;
    cluster.sched.state.lock().unwrap().hold_open = true;

    // Invocation 1 (--detach): leaves its build running cluster-side.
    let chain_a = drvgen::chain(&["scope-a-leaf", "scope-a-root"]);
    let root_a = &chain_a[1];
    let script_a = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&chain_a[0], root_a], vec![], root_a)],
    )]);
    let mut detached_run = cluster.coordinator(|opts| opts.detach_on_interrupt = true);
    let summary_a = cluster
        .run_interrupted(&mut detached_run, script_a, &["a"], 1, 1)
        .await?;
    let foreign_id = summary_a.outcomes[0].build_id.clone();
    assert!(!foreign_id.is_empty());

    // Invocation 2 (default interrupt): cancels its own build only.
    let chain_b = drvgen::chain(&["scope-b-leaf", "scope-b-root"]);
    let root_b = &chain_b[1];
    let script_b = HashMap::from([(
        "b".to_string(),
        vec![single_frame("b", &[&chain_b[0], root_b], vec![], root_b)],
    )]);
    let mut cancelling_run = cluster.coordinator(|_| {});
    let summary_b = cluster
        .run_interrupted(&mut cancelling_run, script_b, &["b"], 2, 1)
        .await?;
    assert!(summary_b.interrupted);
    let own_id = summary_b.outcomes[0].build_id.clone();
    assert_ne!(own_id, foreign_id);

    let st = cluster.sched.state.lock().unwrap();
    assert_eq!(
        st.cancel_calls.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        vec![&own_id],
        "only the second invocation's own build may be cancelled"
    );
    Ok(())
}

/// A second interrupt while cancellation is in flight stops waiting
/// for cancel acknowledgements: no CancelBuild ack is awaited, the
/// build id is reported with its reattach hint (Detached outcome), and
/// the run still counts as interrupted (non-zero exit). Both signals
/// are queued up front so the path is deterministic — no real
/// signal-timing games.
// r[verify bc.interrupt.cancel-default]
#[tokio::test]
async fn second_interrupt_skips_cancel_wait() -> TestResult {
    let cluster = TestCluster::new().await?;
    cluster.sched.state.lock().unwrap().hold_open = true;

    let chain = drvgen::chain(&["dbl-leaf", "dbl-root"]);
    let root = &chain[1];
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&chain[0], root], vec![], root)],
    )]);

    let mut coordinator = cluster.coordinator(|_| {});
    let summary = cluster
        .run_interrupted(&mut coordinator, script, &["a"], 1, 2)
        .await?;
    assert!(summary.interrupted);
    assert!(!summary.detached);
    // The second (already-queued) interrupt wins the cancel race: the
    // build is reported as still running, with its id intact.
    let outcomes: Vec<_> = summary
        .outcomes
        .iter()
        .filter(|o| o.state == OutcomeState::Detached)
        .collect();
    assert_eq!(outcomes.len(), 1, "{:?}", summary.outcomes);
    assert!(!outcomes[0].build_id.is_empty());
    assert!(
        cluster.sched.state.lock().unwrap().cancel_calls.is_empty(),
        "no cancel ack may be awaited after the second interrupt"
    );
    Ok(())
}

/// `--fetch`: the completed output materializes through GetPath into
/// the client CAS, narHash-verified, and `--out-link` points at it.
// r[verify bc.fetch.narhash-verify+2]
#[tokio::test]
async fn fetch_materializes_output_with_narhash_verify() -> TestResult {
    let cluster = TestCluster::new().await?;

    // A real store path for the build's output: upload NAR content the
    // production way so GetPath serves verified bytes.
    let payload = b"hello from the cluster store".to_vec();
    let (nar, nar_hash) = rio_test_support::fixtures::make_nar(&payload);
    let out_path = drvgen::fake_out_path("fetch-out");
    let info = rio_test_support::fixtures::make_path_info(&out_path, &nar, nar_hash);
    assert!(cluster.put_path_as_builder(info, nar).await?);

    // The fixture drv's declared output IS that path, so the stub's
    // BuildCompleted carries it.
    let mut root = drvgen::make_drv("fetch-root", &[], &[]);
    root.node.expected_output_paths = vec![out_path.clone()];
    let script = HashMap::from([(
        "a".to_string(),
        vec![single_frame("a", &[&root], vec![], &root)],
    )]);

    let link = cluster.cas.path().join("result");
    let mut coordinator = cluster.coordinator(|opts| {
        opts.fetch = true;
        opts.out_link = Some(link.clone());
    });
    let (summary, _) = cluster.run(&mut coordinator, script, &["a"]).await?;

    let o = &summary.outcomes[0];
    assert!(matches!(o.state, OutcomeState::Completed { .. }), "{o:?}");
    assert_eq!(o.fetched.len(), 1);
    let dest = &o.fetched[0];
    assert!(dest.starts_with(cluster.cas.path()), "fetched into the CAS");
    // The NAR root is a single file — restored verbatim.
    assert_eq!(std::fs::read(dest)?, payload);
    // Out-link points at the materialization.
    assert_eq!(&std::fs::read_link(&link)?, dest);
    Ok(())
}

/// Helpers for the local-store import tests: upload a (signed) output to
/// the in-process store, with optional references.
async fn seed_output(
    cluster: &common::TestCluster,
    name: &str,
    payload: &[u8],
    references: &[&str],
) -> anyhow::Result<(String, Vec<u8>)> {
    let (nar, nar_hash) = rio_test_support::fixtures::make_nar(payload);
    let path = drvgen::fake_out_path(name);
    let mut info = rio_test_support::fixtures::make_path_info(&path, &nar, nar_hash);
    info.references = references
        .iter()
        .map(|r| rio_nix::store_path::StorePath::parse(r))
        .collect::<Result<_, _>>()?;
    assert!(cluster.put_path_as_builder(info, nar.clone()).await?);
    Ok((path, nar))
}

/// Shared note sink for `OutputFetcher` (the single fallback note).
fn note_sink() -> (
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    impl Fn(String) + Send + Sync + 'static,
) {
    let notes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&notes);
    (notes, move |msg: String| {
        sink.lock().expect("notes lock").push(msg)
    })
}

/// Default fetch path: the output's closure imports into the local nix
/// store via the daemon — closure walked through QueryPathInfo, pruned
/// against the daemon, imported dependencies-first, with the cluster
/// signatures and references riding each AddToStoreNar, and the streamed
/// NAR bytes hash-verified on the way through.
// r[verify bc.fetch.store-import-default]
// r[verify bc.fetch.closure-topo]
// r[verify bc.fetch.narhash-verify+2]
#[tokio::test]
async fn default_fetch_imports_closure_into_local_store() -> TestResult {
    let cluster = TestCluster::new().await?;
    let (dep_path, dep_nar) = seed_output(&cluster, "import-dep", b"dep payload", &[]).await?;
    let (root_path, root_nar) = seed_output(
        &cluster,
        "import-root",
        b"root payload",
        &[dep_path.as_str()],
    )
    .await?;

    let daemon = fake_daemon::FakeDaemon::spawn().await?;
    let (notes, note) = note_sink();
    let mut fetcher = rio_build_cli::import::OutputFetcher::new(
        daemon.socket.clone(),
        cluster.cas.path().to_path_buf(),
        note,
    );
    let mut clients = cluster.clients.clone();
    let fetched = fetcher.fetch(&mut clients, &root_path).await?;
    assert_eq!(
        fetched,
        rio_build_cli::import::FetchedOutput::Store(std::path::PathBuf::from(&root_path))
    );

    let st = daemon.state.lock().expect("daemon state");
    assert_eq!(st.imported.len(), 2, "dep + root must both import");
    assert_eq!(
        st.imported[0].store_path, dep_path,
        "dependencies import before dependents"
    );
    assert_eq!(st.imported[1].store_path, root_path);
    assert_eq!(st.imported[0].nar, dep_nar);
    assert_eq!(st.imported[1].nar, root_nar);
    // Metadata rides the import: references and the cluster signature.
    assert_eq!(st.imported[1].info.references, vec![dep_path.clone()]);
    for imported in &st.imported {
        assert!(
            imported
                .info
                .signatures
                .iter()
                .any(|s| s.starts_with(&format!("{}:", common::SIGNING_KEY_NAME))),
            "cluster signature must ride AddToStoreNar: {:?}",
            imported.info.signatures
        );
    }
    assert!(
        notes.lock().expect("notes lock").is_empty(),
        "no fallback note on the daemon path"
    );
    Ok(())
}

/// Paths the daemon already considers valid are pruned — only the
/// missing part of the closure is imported.
// r[verify bc.fetch.closure-topo]
#[tokio::test]
async fn import_prunes_paths_already_valid_in_daemon() -> TestResult {
    let cluster = TestCluster::new().await?;
    let (dep_path, _) = seed_output(&cluster, "prune-dep", b"dep payload", &[]).await?;
    let (root_path, _) = seed_output(
        &cluster,
        "prune-root",
        b"root payload",
        &[dep_path.as_str()],
    )
    .await?;

    let daemon = fake_daemon::FakeDaemon::spawn().await?;
    daemon
        .state
        .lock()
        .expect("daemon state")
        .valid
        .insert(dep_path.clone());
    let (_notes, note) = note_sink();
    let mut fetcher = rio_build_cli::import::OutputFetcher::new(
        daemon.socket.clone(),
        cluster.cas.path().to_path_buf(),
        note,
    );
    let mut clients = cluster.clients.clone();
    fetcher.fetch(&mut clients, &root_path).await?;

    let st = daemon.state.lock().expect("daemon state");
    assert_eq!(st.imported.len(), 1, "already-valid dep must be pruned");
    assert_eq!(st.imported[0].store_path, root_path);
    Ok(())
}

/// A daemon signature-policy rejection maps to guidance naming the
/// signing key, the trusted-public-keys line, and --no-fetch.
// r[verify bc.fetch.sig-reject-ux]
#[tokio::test]
async fn daemon_sig_rejection_maps_to_trusted_key_guidance() -> TestResult {
    let cluster = TestCluster::new().await?;
    let (root_path, _) = seed_output(&cluster, "sigfail-root", b"payload", &[]).await?;

    let daemon = fake_daemon::FakeDaemon::spawn().await?;
    daemon.state.lock().expect("daemon state").reject_with =
        Some("cannot add path because it lacks a signature by a trusted key".to_string());
    let (_notes, note) = note_sink();
    let mut fetcher = rio_build_cli::import::OutputFetcher::new(
        daemon.socket.clone(),
        cluster.cas.path().to_path_buf(),
        note,
    );
    let mut clients = cluster.clients.clone();
    let err = fetcher
        .fetch(&mut clients, &root_path)
        .await
        .expect_err("rejected import must fail the fetch");
    let msg = format!("{err:#}");
    assert!(msg.contains("trusted-public-keys"), "{msg}");
    assert!(msg.contains(common::SIGNING_KEY_NAME), "{msg}");
    assert!(msg.contains("--no-fetch"), "{msg}");
    Ok(())
}

/// No daemon socket → the output materializes into the client CAS, with
/// exactly one stderr note naming the cause, and the bytes still verify.
// r[verify bc.fetch.daemonless-fallback]
#[tokio::test]
async fn no_daemon_falls_back_to_cas_materialization() -> TestResult {
    let cluster = TestCluster::new().await?;
    let payload = b"fallback payload".to_vec();
    let (root_path, _) = seed_output(&cluster, "fallback-root", &payload, &[]).await?;

    let missing_socket = cluster.cas.path().join("no-such-daemon.sock");
    let (notes, note) = note_sink();
    let mut fetcher = rio_build_cli::import::OutputFetcher::new(
        missing_socket,
        cluster.cas.path().to_path_buf(),
        note,
    );
    let mut clients = cluster.clients.clone();
    let first = fetcher.fetch(&mut clients, &root_path).await?;
    let rio_build_cli::import::FetchedOutput::Cas(dest) = first else {
        panic!("expected CAS fallback, got {first:?}");
    };
    assert!(dest.starts_with(cluster.cas.path()));
    // Single-file NAR → restored verbatim.
    assert_eq!(std::fs::read(&dest)?, payload);

    // A second output reuses the probe answer and must not repeat the note.
    let (second_path, _) = seed_output(&cluster, "fallback-second", b"more", &[]).await?;
    fetcher.fetch(&mut clients, &second_path).await?;
    let notes = notes.lock().expect("notes lock");
    assert_eq!(notes.len(), 1, "exactly one fallback note: {notes:?}");
    assert!(notes[0].contains("no local nix daemon"), "{notes:?}");
    Ok(())
}

/// Fail-fast failure replay: the client fetches the culprit's original
/// log via `GetDerivationLog` (server-side tail), re-prints it one note
/// per line with a header naming the culprit, honors `-L` for the full
/// log, and falls back to the persisted reason text when no log content
/// is available (cross-tenant culprit, expired log, no output).
// r[verify bc.render.failure-log-tail]
#[tokio::test]
async fn failure_replay_prints_culprit_log_tail() -> TestResult {
    use rio_build_cli::coordinator::{FailureLogOpts, replay_failure_log};
    use rio_build_cli::render::RenderHandle;

    let cluster = TestCluster::new().await?;
    let culprit = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-culprit.drv".to_string();
    cluster.sched.state.lock().unwrap().derivation_logs.insert(
        culprit.clone(),
        (0..30)
            .map(|i| format!("compile step {i}").into_bytes())
            .collect(),
    );

    let failed = rio_proto::types::BuildFailed {
        error_message: format!("derivation {culprit} failed in an earlier build: boom"),
        failed_derivation: culprit.clone(),
        culprit_derivation: culprit.clone(),
        culprit_exec_id: "0190f7a1-7c2e-7d10-b5c5-3be41b1c6f7e".into(),
        culprit_error_message: "boom".into(),
        ..Default::default()
    };
    let mut clients = cluster.clients.clone();
    let render = RenderHandle::null();

    // Default: header + 20-line tail, lines 10..=29.
    let notes = replay_failure_log(
        &mut clients,
        "build-0001",
        &failed,
        FailureLogOpts::default(),
        &render,
    )
    .await;
    assert_eq!(notes.len(), 21, "header + 20 tail lines: {notes:?}");
    assert!(
        notes[0].contains("failed previously") && notes[0].contains("last 20 line"),
        "{}",
        notes[0]
    );
    assert!(notes[1].ends_with("compile step 10"), "{}", notes[1]);
    assert!(notes[20].ends_with("compile step 29"), "{}", notes[20]);

    // -L / --print-build-logs: the full log.
    let notes = replay_failure_log(
        &mut clients,
        "build-0001",
        &failed,
        FailureLogOpts {
            print_build_logs: true,
            ..Default::default()
        },
        &render,
    )
    .await;
    assert_eq!(notes.len(), 31, "header + all 30 lines: {notes:?}");
    assert!(notes[1].ends_with("compile step 0"), "{}", notes[1]);

    // No log content for the culprit (the stub closes the stream empty,
    // the same shape as a cross-tenant execution): the persisted reason
    // text is printed instead.
    let mut no_log = failed.clone();
    no_log.culprit_derivation = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-other.drv".to_string();
    no_log.culprit_error_message = "builder exited 1: cc not found".into();
    let notes = replay_failure_log(
        &mut clients,
        "build-0001",
        &no_log,
        FailureLogOpts::default(),
        &render,
    )
    .await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("failed previously") && notes[0].contains("cc not found"),
        "{}",
        notes[0]
    );
    Ok(())
}
