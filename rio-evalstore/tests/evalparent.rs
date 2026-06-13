//! Eval-parent tests (ADR-024): skeleton assembly, the IFD relay,
//! and the fork-worker orchestration loop — all against the REAL
//! `run_eval_parent` with a stub eval callback, real `fork(2)`, real
//! socketpairs.
//!
//! The fork-based tests fork the TEST process: safe under nextest
//! (process-per-test, no sibling test threads) — the project gate runs
//! nextest. The forked child never returns into the harness
//! (`libc::_exit`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use rio_evalstore::EvalStore;
use rio_evalstore::evaljob::framing::{self, FdIo};
use rio_evalstore::evaljob::{EvalParentOpts, run_eval_parent};
use rio_nix::derivation::{Derivation, DerivationOutput};
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use rio_proto::evaljob::{
    CoordinatorFrame, IfdCompletion, ResultFrame, Shutdown, WorkItem, WorkerFrame,
    coordinator_frame, worker_frame,
};
use sha2::{Digest as _, Sha256};

// ---------------------------------------------------------------------------
// drv fixtures: generated through rio-nix so the ATerm round-trip and
// the text-path cross-check in write_derivation hold by construction.
// ---------------------------------------------------------------------------

fn fake_out_path(tag: char) -> String {
    format!("/nix/store/{}-out", tag.to_string().repeat(32))
}

/// Build a drv, compute its drv path the way nix would, and return
/// (full drv path, aterm).
fn mk_drv(
    name: &str,
    out_tag: char,
    input_drv_paths: &[&str],
    input_srcs: &[&str],
) -> (String, String) {
    let outputs = vec![DerivationOutput::new("out", fake_out_path(out_tag), "", "").unwrap()];
    let input_drvs: BTreeMap<String, std::collections::BTreeSet<String>> = input_drv_paths
        .iter()
        .map(|p| (p.to_string(), ["out".to_string()].into()))
        .collect();
    let drv = Derivation::new(
        outputs,
        input_drvs,
        input_srcs.iter().map(|s| s.to_string()).collect(),
        "x86_64-linux".into(),
        "/bin/sh".into(),
        vec!["-c".into(), "echo hi".into()],
        [("name".to_string(), name.to_string())].into(),
    )
    .unwrap();
    let aterm = drv.to_aterm();
    let refs: Vec<StorePath> = input_srcs
        .iter()
        .chain(input_drv_paths.iter())
        .map(|p| StorePath::parse(p).unwrap())
        .collect();
    let path = StorePath::make_text(
        &format!("{name}.drv"),
        &NixHash::new(HashAlgo::SHA256, Sha256::digest(aterm.as_bytes()).to_vec()).unwrap(),
        &refs,
    )
    .unwrap();
    (path.to_string(), aterm)
}

fn open_store(dir: &std::path::Path) -> EvalStore {
    EvalStore::open(Some(dir.to_str().unwrap())).expect("open store")
}

// ---------------------------------------------------------------------------
// skeleton assembly
// ---------------------------------------------------------------------------

#[test]
fn assemble_subgraph_ships_closure_with_digests() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(dir.path());

    let (leaf_path, leaf_aterm) = mk_drv("leaf", 'a', &[], &[]);
    store.write_derivation("leaf.drv", leaf_aterm.as_bytes(), &leaf_path)?;
    let (root_path, root_aterm) = mk_drv("root", 'b', &[leaf_path.as_str()], &[]);
    store.write_derivation("root.drv", root_aterm.as_bytes(), &root_path)?;

    let frame = store.assemble_subgraph(&root_path)?;
    assert_eq!(frame.nodes.len(), 2);
    assert_eq!(frame.drv_blobs.len(), 2);
    assert_eq!(frame.root_drv_digest.len(), 32);

    // Digest discipline: every blob digest is blake3(body); node and
    // blob digests pair up; the root node's input_drv_digests name the
    // leaf's digest.
    for blob in &frame.drv_blobs {
        assert_eq!(
            blob.digest.as_slice(),
            blake3::hash(&blob.body).as_bytes(),
            "negotiation digest must be blake3(canonical bytes)"
        );
    }
    let root_node = frame
        .nodes
        .iter()
        .find(|n| n.drv_path == root_path)
        .expect("root node present");
    assert_eq!(root_node.drv_digest, frame.root_drv_digest);
    let leaf_node = frame
        .nodes
        .iter()
        .find(|n| n.drv_path == leaf_path)
        .expect("leaf node present");
    assert_eq!(
        root_node.input_drv_digests,
        vec![leaf_node.drv_digest.clone()]
    );
    assert!(leaf_node.input_drv_digests.is_empty());
    assert_eq!(root_node.system, "x86_64-linux");
    assert_eq!(root_node.output_names, vec!["out".to_string()]);
    assert_eq!(root_node.expected_output_paths, vec![fake_out_path('b')]);
    assert!(!root_node.is_fixed_output);

    // The canonical body re-digests through the shared verifier path:
    // decode + canonical re-encode is byte-identical.
    let root_blob = frame
        .drv_blobs
        .iter()
        .find(|b| b.drv_path == root_path)
        .unwrap();
    let decoded =
        <rio_proto::drv::Derivation as prost::Message>::decode(root_blob.body.as_slice())?;
    assert_eq!(
        rio_proto::derivation_util::canonical_encode(&decoded),
        root_blob.body,
        "assembled bodies must already be canonical"
    );

    // Per-worker delta: a second assembly of the same root ships no
    // repeat nodes but still names the root digest (final frame).
    let again = store.assemble_subgraph(&root_path)?;
    assert!(again.nodes.is_empty());
    assert!(again.drv_blobs.is_empty());
    assert_eq!(again.root_drv_digest, frame.root_drv_digest);

    // ifd_materials ignores the reported set (the request must carry
    // its root unconditionally).
    let (node, blob) = store.ifd_materials(&root_path)?;
    assert_eq!(node.drv_digest, frame.root_drv_digest);
    assert_eq!(blob.body, root_blob.body);
    Ok(())
}

#[test]
fn assemble_emits_local_dir_source_roots_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(dir.path());

    // A real local tree, ingested the way eval does.
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("sub"))?;
    std::fs::write(tree.join("sub/data.txt"), b"hello source\n")?;
    let added = store.add_source_tree(tree.to_str().unwrap(), "tree", &[], &mut |h| {
        let hash = NixHash::new(HashAlgo::SHA256, hex::decode(&h.nar_sha256).unwrap()).unwrap();
        Ok(StorePath::make_fixed_output("tree", &hash, true, &[])
            .unwrap()
            .to_string())
    })?;

    // A streamed path (no origin): toFile-shaped content.
    let text = b"builder text".to_vec();
    let streamed_path = {
        let hash = NixHash::new(HashAlgo::SHA256, Sha256::digest(&text).to_vec()).unwrap();
        StorePath::make_text("builder.sh", &hash, &[]).unwrap()
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(
        &mut nar,
        &rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: text,
        },
    )?;
    store.add_nar(
        &rio_evalstore::store::ProvidedInfo {
            path: streamed_path.to_string(),
            nar_hash: hex::encode(Sha256::digest(&nar)),
            nar_size: nar.len() as u64,
            references: vec![],
            ca: None,
        },
        &mut nar.as_slice(),
    )?;

    let (drv_path, aterm) = mk_drv(
        "withsrc",
        'd',
        &[],
        &[added.path.as_str(), streamed_path.as_str()],
    );
    store.write_derivation("withsrc.drv", aterm.as_bytes(), &drv_path)?;

    let frame = store.assemble_subgraph(&drv_path)?;
    // Only the origin-backed directory tree becomes a SourceRoot; the
    // streamed text path is skipped (counted) — the coordinator's
    // upload path can only re-read origins.
    assert_eq!(frame.source_roots.len(), 1);
    let sr = &frame.source_roots[0];
    assert_eq!(sr.store_path, added.path);
    assert_eq!(sr.origin, tree.to_str().unwrap());
    assert_eq!(sr.nar_hash, hex::decode(&added.nar_sha256)?);
    assert_eq!(sr.dir_digest.len(), 32);
    assert!(store.stats().count("source_root_skipped") >= 1);

    // Dedup: re-assembling a sibling drv referencing the same tree
    // doesn't resend the source root.
    let (drv2, aterm2) = mk_drv("withsrc2", 'e', &[], &[added.path.as_str()]);
    store.write_derivation("withsrc2.drv", aterm2.as_bytes(), &drv2)?;
    let frame2 = store.assemble_subgraph(&drv2)?;
    assert!(frame2.source_roots.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// IFD relay
// ---------------------------------------------------------------------------

/// Worker side blocks on its socketpair until IfdCompletion; success
/// imports the coordinator-materialized outputs into the eval store.
// r[verify bc.evalparent.ifd-relay]
#[test]
fn ifd_relay_blocks_then_imports_outputs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(dir.path());

    let (leaf_path, leaf_aterm) = mk_drv("ifd-leaf", 'f', &[], &[]);
    store.write_derivation("ifd-leaf.drv", leaf_aterm.as_bytes(), &leaf_path)?;
    let (ifd_path, ifd_aterm) = mk_drv("ifd-drv", 'g', &[leaf_path.as_str()], &[]);
    store.write_derivation("ifd-drv.drv", ifd_aterm.as_bytes(), &ifd_path)?;

    // Materialize the output the way the coordinator's fetch would.
    let out_path = fake_out_path('g');
    let out_basename = out_path.rsplit('/').next().unwrap();
    let fetched = store.cas_root().join("fetched").join(out_basename);
    std::fs::create_dir_all(&fetched)?;
    std::fs::write(fetched.join("result.txt"), b"ifd output\n")?;

    let (worker_end, parent_end) = UnixStream::pair()?;
    let ifd_path_clone = ifd_path.clone();
    let out_path_clone = out_path.clone();
    let relay = std::thread::spawn(move || -> anyhow::Result<(usize, String)> {
        let mut io = &parent_end;
        // 1. intermediate closure frame under the mini-submission attr
        let f1: WorkerFrame = framing::read_frame(&mut io)?.expect("closure frame");
        let Some(worker_frame::Msg::Result(pre)) = f1.msg else {
            anyhow::bail!("expected Result frame first, got {f1:?}");
        };
        assert_eq!(pre.attr, format!("ifd:{ifd_path_clone}"));
        assert!(pre.root_drv_digest.is_empty(), "intermediate batch");
        // 2. the request
        let f2: WorkerFrame = framing::read_frame(&mut io)?.expect("ifd request");
        let Some(worker_frame::Msg::IfdRequest(req)) = f2.msg else {
            anyhow::bail!("expected IfdRequest, got {f2:?}");
        };
        let node = req.node.expect("node");
        assert_eq!(node.drv_path, ifd_path_clone);
        assert!(req.blob.is_some());
        // 3. completion
        framing::write_frame(
            &mut io,
            &CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::IfdCompletion(IfdCompletion {
                    drv_path: ifd_path_clone,
                    output_paths: vec![out_path_clone],
                    error: String::new(),
                })),
            },
        )?;
        Ok((pre.nodes.len(), node.drv_path.clone()))
    });

    let outputs = rio_evalstore::evaljob::ifd::ifd_request_blocking(
        &store,
        worker_end.as_raw_fd(),
        &ifd_path,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    assert_eq!(outputs, vec![out_path.clone()]);
    let (pre_nodes, _) = relay.join().unwrap()?;
    assert_eq!(
        pre_nodes, 2,
        "IFD closure (leaf + ifd drv) precedes the request"
    );

    // Imported: the eval store now serves the output.
    assert!(store.is_valid_path(out_basename));
    let mut content = Vec::new();
    store.read_file(out_basename, "result.txt", &mut content)?;
    assert_eq!(content, b"ifd output\n");
    Ok(())
}

// r[verify bc.evalparent.ifd-relay]
#[test]
fn ifd_relay_error_completion_fails_the_import() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(dir.path());
    let (drv_path, aterm) = mk_drv("ifd-err", 'h', &[], &[]);
    store.write_derivation("ifd-err.drv", aterm.as_bytes(), &drv_path)?;

    let (worker_end, parent_end) = UnixStream::pair()?;
    let drv_clone = drv_path.clone();
    let relay = std::thread::spawn(move || {
        let mut io = &parent_end;
        let _: Option<WorkerFrame> = framing::read_frame(&mut io).unwrap();
        let _: Option<WorkerFrame> = framing::read_frame(&mut io).unwrap();
        framing::write_frame(
            &mut io,
            &CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::IfdCompletion(IfdCompletion {
                    drv_path: drv_clone,
                    output_paths: vec![],
                    error: "--local-ifd: local IFD fallback is not wired yet".into(),
                })),
            },
        )
        .unwrap();
    });
    let err = rio_evalstore::evaljob::ifd::ifd_request_blocking(
        &store,
        worker_end.as_raw_fd(),
        &drv_path,
    )
    .unwrap_err();
    assert!(
        err.contains("--local-ifd"),
        "coordinator message surfaces verbatim: {err}"
    );
    relay.join().unwrap();
    Ok(())
}

// ---------------------------------------------------------------------------
// the fork-worker orchestration loop
// ---------------------------------------------------------------------------

/// A stub final ResultFrame: digest derived from the attr, worker pid
/// recorded in `pname` so tests can assert which process evaluated.
fn stub_result(attr: &str) -> WorkerFrame {
    let digest = blake3::hash(attr.as_bytes());
    let node = rio_proto::types::DerivationNode {
        drv_path: format!("/nix/store/stub-{attr}.drv"),
        pname: std::process::id().to_string(),
        drv_digest: digest.as_bytes().to_vec(),
        ..Default::default()
    };
    WorkerFrame {
        msg: Some(worker_frame::Msg::Result(ResultFrame {
            attr: attr.to_string(),
            nodes: vec![node],
            drv_blobs: vec![],
            source_roots: vec![],
            root_drv_digest: digest.as_bytes().to_vec(),
        })),
    }
}

/// Fork the eval parent with a stub eval callback; drive it from the
/// test (acting as the coordinator). Returns every upstream frame
/// observed until clean EOF, plus the parent's exit status.
fn drive_parent(
    opts: EvalParentOpts,
    attrs: &[&str],
    marker_dir: &std::path::Path,
    expected_results: usize,
) -> anyhow::Result<(Vec<WorkerFrame>, i32)> {
    let (coord_end, parent_end) = UnixStream::pair()?;
    let marker = marker_dir.to_path_buf();
    // SAFETY: nextest runs one test per process; no sibling threads.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // ----- eval-parent process -----
        drop(coord_end);
        let exit_code = {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = open_store(dir.path());
            let mut eval = |attr: &str, fd: std::os::fd::RawFd| -> Result<(), String> {
                if attr == "boom" {
                    return Err("eval exploded".to_string());
                }
                if let Some(rest) = attr.strip_prefix("crash:") {
                    let m = marker.join(rest);
                    if !m.exists() {
                        std::fs::write(&m, b"x").expect("marker");
                        // SIGKILL mid-eval — the crash-injection test.
                        // SAFETY: kills only this worker process.
                        unsafe { libc::raise(libc::SIGKILL) };
                    }
                }
                framing::write_frame(&mut FdIo(fd), &stub_result(attr)).map_err(|e| e.to_string())
            };
            match run_eval_parent(&store, parent_end.as_raw_fd(), &opts, &mut eval) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        };
        // Never return into the test harness; skip Drop of the
        // test-owned tempdirs duplicated by fork.
        std::io::stdout().flush().ok();
        // SAFETY: terminating the forked child.
        unsafe { libc::_exit(exit_code) };
    }
    // ----- coordinator (the test) -----
    drop(parent_end);
    let mut io = &coord_end;
    for attr in attrs {
        framing::write_frame(
            &mut io,
            &CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::Work(WorkItem {
                    attr: attr.to_string(),
                })),
            },
        )?;
    }
    let mut frames: Vec<WorkerFrame> = Vec::new();
    let mut results = 0usize;
    let mut shutdown_sent = false;
    loop {
        if results >= expected_results && !shutdown_sent {
            framing::write_frame(
                &mut io,
                &CoordinatorFrame {
                    msg: Some(coordinator_frame::Msg::Shutdown(Shutdown {})),
                },
            )?;
            shutdown_sent = true;
        }
        match framing::read_frame::<_, WorkerFrame>(&mut io)? {
            None => break, // parent exited → clean EOF
            Some(f) => {
                match &f.msg {
                    Some(worker_frame::Msg::Result(r)) if !r.root_drv_digest.is_empty() => {
                        results += 1;
                    }
                    // A named, non-fatal WorkerError resolves an attr
                    // too (lost-attr path).
                    Some(worker_frame::Msg::Error(e)) if !e.attr.is_empty() => {
                        results += 1;
                    }
                    _ => {}
                }
                frames.push(f);
            }
        }
    }
    let mut status: libc::c_int = 0;
    // SAFETY: waiting on our forked child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -libc::WTERMSIG(status)
    };
    Ok((frames, code))
}

fn result_attrs(frames: &[WorkerFrame]) -> Vec<(String, String)> {
    frames
        .iter()
        .filter_map(|f| match &f.msg {
            Some(worker_frame::Msg::Result(r)) if !r.root_drv_digest.is_empty() => {
                Some((r.attr.clone(), r.nodes[0].pname.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn parent_completes_attrs_and_drains_cleanly() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (frames, code) = drive_parent(
        EvalParentOpts {
            max_workers: 2,
            recycle_attrs: 0,
            recycle_rss_mb: 0,
            attr_retries: 1,
        },
        &["alpha", "beta", "gamma"],
        dir.path(),
        3,
    )?;
    assert_eq!(code, 0, "parent must exit cleanly after Shutdown drain");
    let mut attrs: Vec<String> = result_attrs(&frames).into_iter().map(|(a, _)| a).collect();
    attrs.sort();
    assert_eq!(attrs, vec!["alpha", "beta", "gamma"]);
    Ok(())
}

/// Eval failures are per-attr: a WorkerError names the attr, siblings
/// complete, the parent drains cleanly.
#[test]
fn eval_failure_is_nonfatal_worker_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (frames, code) = drive_parent(
        EvalParentOpts {
            max_workers: 1,
            recycle_attrs: 0,
            recycle_rss_mb: 0,
            attr_retries: 1,
        },
        &["boom", "ok"],
        dir.path(),
        2,
    )?;
    assert_eq!(code, 0);
    let err = frames
        .iter()
        .find_map(|f| match &f.msg {
            Some(worker_frame::Msg::Error(e)) => Some(e.clone()),
            _ => None,
        })
        .expect("WorkerError for boom");
    assert_eq!(err.attr, "boom");
    assert!(!err.fatal);
    assert!(err.message.contains("eval exploded"));
    assert_eq!(result_attrs(&frames).len(), 1, "ok still completes");
    Ok(())
}

/// kill -9 a worker mid-eval: the parent re-queues the in-flight attr
/// to a fresh fork and the run completes; the crash is surfaced as a
/// non-fatal, attr-less WorkerError (visibility without failing the
/// attr).
// r[verify bc.evalparent.crash-requeue]
#[test]
fn worker_crash_requeues_and_completes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (frames, code) = drive_parent(
        EvalParentOpts {
            max_workers: 2,
            recycle_attrs: 0,
            recycle_rss_mb: 0,
            attr_retries: 1,
        },
        &["crash:one", "steady"],
        dir.path(),
        2,
    )?;
    assert_eq!(code, 0, "parent must survive worker death");
    let results = result_attrs(&frames);
    let mut attrs: Vec<&str> = results.iter().map(|(a, _)| a.as_str()).collect();
    attrs.sort();
    assert_eq!(
        attrs,
        vec!["crash:one", "steady"],
        "crashed attr completes on retry"
    );
    // Visibility: an attr-less, non-fatal crash report went upstream.
    let crash = frames
        .iter()
        .find_map(|f| match &f.msg {
            Some(worker_frame::Msg::Error(e)) if e.attr.is_empty() => Some(e.clone()),
            _ => None,
        })
        .expect("crash WorkerError");
    assert!(!crash.fatal);
    assert!(
        crash.message.contains("re-queueing attr 'crash:one'"),
        "{}",
        crash.message
    );
    Ok(())
}

/// Retries exhausted: the attr is reported lost (named WorkerError),
/// siblings still complete, the parent never dies.
// r[verify bc.evalparent.crash-requeue]
#[test]
fn crash_retries_exhausted_loses_only_that_attr() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // attr_retries = 0: the first crash is final.
    let (frames, code) = drive_parent(
        EvalParentOpts {
            max_workers: 1,
            recycle_attrs: 0,
            recycle_rss_mb: 0,
            attr_retries: 0,
        },
        &["crash:fatal", "steady"],
        dir.path(),
        2,
    )?;
    assert_eq!(code, 0);
    let named = frames
        .iter()
        .find_map(|f| match &f.msg {
            Some(worker_frame::Msg::Error(e)) if e.attr == "crash:fatal" => Some(e.clone()),
            _ => None,
        })
        .expect("named WorkerError after exhausted retries");
    assert!(!named.fatal);
    assert!(named.message.contains("giving up"));
    let results = result_attrs(&frames);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "steady");
    Ok(())
}

/// recycle_attrs = 1: every attr gets a fresh worker (distinct pids in
/// the stub frames), results identical, RecycleNotices surface.
// r[verify bc.evalparent.recycle]
#[test]
fn recycle_after_each_attr_uses_fresh_workers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (frames, code) = drive_parent(
        EvalParentOpts {
            max_workers: 1,
            recycle_attrs: 1,
            recycle_rss_mb: 0,
            attr_retries: 1,
        },
        &["r1", "r2", "r3"],
        dir.path(),
        3,
    )?;
    assert_eq!(code, 0);
    let results = result_attrs(&frames);
    let mut attrs: Vec<&str> = results.iter().map(|(a, _)| a.as_str()).collect();
    attrs.sort();
    assert_eq!(attrs, vec!["r1", "r2", "r3"]);
    let pids: std::collections::HashSet<&str> =
        results.iter().map(|(_, pid)| pid.as_str()).collect();
    assert_eq!(
        pids.len(),
        3,
        "every attr must run in a fresh fork: {results:?}"
    );
    let recycles = frames
        .iter()
        .filter(|f| matches!(&f.msg, Some(worker_frame::Msg::Recycle(_))))
        .count();
    assert!(
        recycles >= 2,
        "recycle notices surface upstream (got {recycles})"
    );
    Ok(())
}

/// Two workers blocked on the SAME IFD drv simultaneously: the parent
/// must route one completion to each requester (request order), not
/// overwrite the route — a strand leaves a worker blocked forever and
/// the run never drains.
// r[verify bc.evalparent.ifd-relay]
#[test]
fn concurrent_same_drv_ifd_resumes_both_workers() -> anyhow::Result<()> {
    let (coord_end, parent_end) = UnixStream::pair()?;
    // SAFETY: nextest runs one test per process; no sibling threads.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // ----- eval-parent process -----
        drop(coord_end);
        let exit_code = {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = open_store(dir.path());
            let mut eval = |_attr: &str, fd: std::os::fd::RawFd| -> Result<(), String> {
                // Every worker hits IFD on the SAME drv. The test
                // coordinator answers each request with an error
                // completion, which must resume THIS worker.
                let (drv_path, aterm) = mk_drv("shared-ifd", 'z', &[], &[]);
                store
                    .write_derivation("shared-ifd.drv", aterm.as_bytes(), &drv_path)
                    .map_err(|e| e.to_string())?;
                let err = rio_evalstore::evaljob::ifd::ifd_request_blocking(&store, fd, &drv_path)
                    .expect_err("error completion fails the import");
                Err(err)
            };
            let opts = EvalParentOpts {
                max_workers: 2,
                recycle_attrs: 0,
                recycle_rss_mb: 0,
                attr_retries: 0,
            };
            match run_eval_parent(&store, parent_end.as_raw_fd(), &opts, &mut eval) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        };
        // SAFETY: terminating the forked child without unwinding into
        // the test harness.
        unsafe { libc::_exit(exit_code) };
    }
    // ----- coordinator (the test) -----
    drop(parent_end);
    let mut io = &coord_end;
    for attr in ["i1", "i2"] {
        framing::write_frame(
            &mut io,
            &CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::Work(WorkItem { attr: attr.into() })),
            },
        )?;
    }
    let mut requests: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut completions_sent = false;
    let mut shutdown_sent = false;
    loop {
        if errors.len() >= 2 && !shutdown_sent {
            framing::write_frame(
                &mut io,
                &CoordinatorFrame {
                    msg: Some(coordinator_frame::Msg::Shutdown(Shutdown {})),
                },
            )?;
            shutdown_sent = true;
        }
        let Some(frame) = framing::read_frame::<_, WorkerFrame>(&mut io)? else {
            break;
        };
        match frame.msg {
            Some(worker_frame::Msg::IfdRequest(req)) => {
                requests.push(req.node.expect("node").drv_path);
                // Only answer once BOTH workers are blocked on the
                // same drv — this is the routing collision under test.
                if requests.len() == 2 && !completions_sent {
                    assert_eq!(requests[0], requests[1], "both workers on one drv");
                    for _ in 0..2 {
                        framing::write_frame(
                            &mut io,
                            &CoordinatorFrame {
                                msg: Some(coordinator_frame::Msg::IfdCompletion(IfdCompletion {
                                    drv_path: requests[0].clone(),
                                    output_paths: vec![],
                                    error: "test coordinator has no cluster".into(),
                                })),
                            },
                        )?;
                    }
                    completions_sent = true;
                }
            }
            Some(worker_frame::Msg::Error(e)) if !e.attr.is_empty() => {
                assert!(e.message.contains("no cluster"), "{}", e.message);
                errors.push(e.attr);
            }
            _ => {}
        }
    }
    let mut status: libc::c_int = 0;
    // SAFETY: waiting on our forked child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    errors.sort();
    assert_eq!(errors, vec!["i1", "i2"], "BOTH blocked workers resumed");
    Ok(())
}

/// Fork workers must not append through the pack segment inherited
/// from the eval parent (shared O_APPEND file description + stale
/// offset bookkeeping = interleaved records). Each worker allocates
/// its own per-pid segment; everything ingested by parent and workers
/// reads back from a fresh store handle.
#[test]
fn fork_workers_use_their_own_pack_segments() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cas = dir.path().join("cas");
    std::fs::create_dir_all(&cas)?;
    // Source trees on disk before the fork, one per attr plus the
    // parent's pre-fork ingest (the flake-input-fetch stand-in).
    for tag in ["pre", "s1", "s2"] {
        let tree = dir.path().join(format!("tree-{tag}"));
        std::fs::create_dir_all(&tree)?;
        std::fs::write(tree.join("data.txt"), format!("payload {tag}\n"))?;
    }
    let ingest =
        |store: &EvalStore, tree: &std::path::Path, name: &str| -> Result<String, String> {
            store
                .add_source_tree(tree.to_str().unwrap(), name, &[], &mut |h| {
                    let hash = NixHash::new(HashAlgo::SHA256, hex::decode(&h.nar_sha256).unwrap())
                        .unwrap();
                    Ok(StorePath::make_fixed_output(name, &hash, true, &[])
                        .unwrap()
                        .to_string())
                })
                .map(|r| r.path)
                .map_err(|e| e.to_string())
        };

    let (coord_end, parent_end) = UnixStream::pair()?;
    let base = dir.path().to_path_buf();
    // SAFETY: nextest runs one test per process; no sibling threads.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // ----- eval-parent process -----
        drop(coord_end);
        let exit_code = {
            let store = open_store(&cas);
            // Pre-fork ingest: creates the parent's writer segment,
            // which every worker then inherits.
            ingest(&store, &base.join("tree-pre"), "tree-pre").expect("pre-fork ingest");
            let mut eval = |attr: &str, fd: std::os::fd::RawFd| -> Result<(), String> {
                let path = ingest(&store, &base.join(format!("tree-{attr}")), attr)?;
                // Report the ingested store path via the stub frame's
                // pname so the test can read it back.
                let digest = blake3::hash(attr.as_bytes());
                let node = rio_proto::types::DerivationNode {
                    drv_path: format!("/nix/store/stub-{attr}.drv"),
                    pname: path,
                    drv_digest: digest.as_bytes().to_vec(),
                    ..Default::default()
                };
                framing::write_frame(
                    &mut FdIo(fd),
                    &WorkerFrame {
                        msg: Some(worker_frame::Msg::Result(ResultFrame {
                            attr: attr.to_string(),
                            nodes: vec![node],
                            drv_blobs: vec![],
                            source_roots: vec![],
                            root_drv_digest: digest.as_bytes().to_vec(),
                        })),
                    },
                )
                .map_err(|e| e.to_string())
            };
            let opts = EvalParentOpts {
                max_workers: 2,
                recycle_attrs: 0,
                recycle_rss_mb: 0,
                attr_retries: 0,
            };
            let rc = match run_eval_parent(&store, parent_end.as_raw_fd(), &opts, &mut eval) {
                Ok(()) => 0,
                Err(_) => 1,
            };
            store.flush().expect("parent flush");
            rc
        };
        // SAFETY: terminating the forked child without unwinding into
        // the test harness.
        unsafe { libc::_exit(exit_code) };
    }
    // ----- coordinator (the test) -----
    drop(parent_end);
    let mut io = &coord_end;
    for attr in ["s1", "s2"] {
        framing::write_frame(
            &mut io,
            &CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::Work(WorkItem { attr: attr.into() })),
            },
        )?;
    }
    let mut ingested: Vec<(String, String)> = Vec::new(); // (attr, store path)
    let mut shutdown_sent = false;
    loop {
        if ingested.len() >= 2 && !shutdown_sent {
            framing::write_frame(
                &mut io,
                &CoordinatorFrame {
                    msg: Some(coordinator_frame::Msg::Shutdown(Shutdown {})),
                },
            )?;
            shutdown_sent = true;
        }
        let Some(frame) = framing::read_frame::<_, WorkerFrame>(&mut io)? else {
            break;
        };
        if let Some(worker_frame::Msg::Result(r)) = frame.msg
            && !r.root_drv_digest.is_empty()
        {
            ingested.push((r.attr.clone(), r.nodes[0].pname.clone()));
        }
    }
    let mut status: libc::c_int = 0;
    // SAFETY: waiting on our forked child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(ingested.len(), 2);

    // Structural: one segment per writer process — the parent's
    // pre-fork segment plus one per fork worker, distinct pids in the
    // names (seg-<nanos>-<pid>-<attempt>.pack).
    let seg_pids: std::collections::HashSet<String> = std::fs::read_dir(cas.join("packs"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let pid = name.strip_prefix("seg-")?.split('-').nth(1)?.to_string();
            Some(pid)
        })
        .collect();
    assert!(
        seg_pids.len() >= 3,
        "parent + each fork worker write their own segment, got pids {seg_pids:?}"
    );

    // Content: a fresh handle serves every ingested tree (worker
    // flushes merged the index; records intact).
    let reopened = open_store(&cas);
    for (attr, path) in &ingested {
        let basename = path.rsplit('/').next().unwrap();
        assert!(reopened.is_valid_path(basename), "{path} missing");
        let mut content = Vec::new();
        reopened.read_file(basename, "data.txt", &mut content)?;
        assert_eq!(content, format!("payload {attr}\n").as_bytes());
    }
    Ok(())
}
