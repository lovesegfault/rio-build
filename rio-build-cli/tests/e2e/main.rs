//! End-to-end coordinator pipeline tests (ADR-024 P3a): a scripted
//! eval-parent stub feeds real canonical drv closures over the
//! `rio.evaljob` channel; the coordinator negotiates, uploads, and
//! submits against REAL in-process rio-store services (ephemeral
//! postgres) and a purpose-built scheduler stub that runs the actual
//! digest bulk-verify against the store's `drv_blobs` table.

mod common;
mod drvgen;
mod stub_parent;
mod stub_scheduler;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;

use common::{TestCluster, single_frame};
use rio_build_cli::coordinator::OutcomeState;
use rio_evalstore::dirblob::{BuiltDir, BuiltEntry};
use rio_evalstore::ingest::{IngestConfig, IngestNode, ingest_tree};
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
    }
}

fn write_source_tree(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("default.nix"), b"{ }: null\n").unwrap();
    std::fs::write(dir.join("src/main.c"), vec![0x42; 100_000]).unwrap();
    std::fs::write(dir.join("src/util.c"), b"static int x;\n").unwrap();
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
// r[verify bc.upload.origin-reread]
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
// r[verify bc.fetch.narhash-verify]
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
