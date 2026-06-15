//! Coordinator-side test harness for the rio-eval parent. Speaks the
//! REAL worker channel (spawn with fd 3, send
//! WorkItems, fold frames) without needing a cluster — the
//! `rio-eval-smoke` nix check drives the actual C++ binary through it
//! in the build sandbox, the same tier as `evalstore-parity`.
//!
//! Validates the worker contract while folding:
//!   - every DrvBlob digest is blake3(body) and the body is canonical
//!     (decode + canonical re-encode is byte-identical);
//!   - every final frame's root digest resolves to a folded node;
//!   - nodes always ride with their bodies on first sight.
//!
//! Prints a JSON summary to stdout; any contract violation exits 1.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use prost::Message as _;
use rio_build_cli::evalchan;
use rio_proto::evaljob::{Shutdown, WorkItem, coordinator_frame, worker_frame};

#[derive(Parser)]
#[command(
    name = "eval-harness",
    about = "drive a rio-eval parent over the worker channel"
)]
struct Args {
    /// Path to the rio-eval binary.
    #[arg(long)]
    eval_parent: PathBuf,
    /// Client CAS directory (passed through as --cas).
    #[arg(long)]
    cas: PathBuf,
    /// Fixture nix file (passed through as --file).
    #[arg(long, conflicts_with = "flake", required_unless_present = "flake")]
    file: Option<PathBuf>,
    /// Fixture flake ref (passed through as --flake).
    #[arg(long)]
    flake: Option<String>,
    /// Comma-separated attrs to evaluate. Empty in file mode means the
    /// file's top-level value (the empty attr path), like the
    /// coordinator's zero-installable default.
    #[arg(long, value_delimiter = ',')]
    attrs: Vec<String>,
    /// `--arg NAME EXPR` pair forwarded to the eval parent (file mode).
    #[arg(long, num_args = 2, value_names = ["NAME", "EXPR"])]
    arg: Vec<String>,
    /// `--argstr NAME VALUE` pair forwarded to the eval parent (file mode).
    #[arg(long, num_args = 2, value_names = ["NAME", "VALUE"])]
    argstr: Vec<String>,
    /// `-I` lookup-path entry forwarded to the eval parent (file mode).
    #[arg(short = 'I', long)]
    include: Vec<String>,
    /// Pass --recycle-attrs to the eval parent.
    #[arg(long)]
    recycle_attrs: Option<u32>,
    /// Pass --workers to the eval parent.
    #[arg(long)]
    workers: Option<u32>,
    /// Crash injection: kill -9 one fork worker this many ms after the
    /// work items go out.
    #[arg(long)]
    kill_worker_after_ms: Option<u64>,
}

#[derive(serde::Serialize)]
struct ResultSummary {
    attr: String,
    root_drv_path: String,
    root_digest_hex: String,
    /// Nodes folded for this attr's frames (delta — shared closures
    /// arrive once per worker).
    new_nodes: usize,
    source_roots: usize,
}

#[derive(serde::Serialize)]
struct Summary {
    results: Vec<ResultSummary>,
    /// Named attr failures (WorkerError with attr).
    eval_errors: Vec<(String, String)>,
    /// Attr-less faults (crash visibility reports).
    faults: Vec<String>,
    /// Attrset expansions: (attr, child attr paths).
    expansions: Vec<(String, Vec<String>)>,
    /// Children skipped by expansions (not derivations, not recursable).
    skipped: Vec<String>,
    /// Pre-fork warmup progress notes (libnix fetch-activity start
    /// lines forwarded as `Note` frames).
    notes: Vec<String>,
    recycles: usize,
    total_nodes: usize,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    // Mirror the coordinator's nix-build default: in file mode, no
    // attrs means the file's top-level value (the empty attr path).
    if args.attrs.is_empty() && args.file.is_some() {
        args.attrs = vec![String::new()];
    }
    let mut parent_args = vec!["--cas".to_string(), args.cas.display().to_string()];
    match (&args.file, &args.flake) {
        (Some(f), None) => parent_args.extend(["--file".into(), f.display().to_string()]),
        (None, Some(r)) => parent_args.extend(["--flake".into(), r.clone()]),
        _ => unreachable!("clap enforces exactly one of --file/--flake"),
    }
    for pair in args.arg.chunks(2) {
        parent_args.push("--arg".into());
        parent_args.extend(pair.iter().cloned());
    }
    for pair in args.argstr.chunks(2) {
        parent_args.push("--argstr".into());
        parent_args.extend(pair.iter().cloned());
    }
    for inc in &args.include {
        parent_args.extend(["-I".into(), inc.clone()]);
    }
    if let Some(n) = args.recycle_attrs {
        parent_args.extend(["--recycle-attrs".into(), n.to_string()]);
    }
    if let Some(n) = args.workers {
        parent_args.extend(["--workers".into(), n.to_string()]);
    }

    let (chan, mut child) =
        evalchan::spawn_eval_parent(&args.eval_parent, &parent_args, false, None)
            .context("spawning eval parent")?;
    let evalchan::EvalChannel {
        mut reader,
        mut writer,
    } = chan;

    for attr in &args.attrs {
        writer
            .send(coordinator_frame::Msg::Work(WorkItem {
                attr: attr.clone(),
            }))
            .await?;
    }

    if let Some(ms) = args.kill_worker_after_ms {
        let parent_pid = child.id().context("eval parent already exited")?;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            kill_one_fork_worker(parent_pid);
        });
    }

    let mut pending: std::collections::HashSet<String> = args.attrs.iter().cloned().collect();
    let mut summary = Summary {
        results: vec![],
        eval_errors: vec![],
        faults: vec![],
        expansions: vec![],
        skipped: vec![],
        notes: vec![],
        recycles: 0,
        total_nodes: 0,
    };
    // digest → drv_path, folded across all frames (the coordinator's
    // dedup view).
    let mut nodes: HashMap<Vec<u8>, String> = HashMap::new();
    let mut shutdown_sent = false;

    loop {
        if pending.is_empty() && !shutdown_sent {
            writer
                .send(coordinator_frame::Msg::Shutdown(Shutdown {}))
                .await?;
            shutdown_sent = true;
        }
        let msg = tokio::time::timeout(Duration::from_secs(600), reader.recv())
            .await
            .context("timed out waiting for worker frames")??;
        let Some(msg) = msg else {
            break; // clean EOF
        };
        match msg {
            worker_frame::Msg::Result(frame) => {
                let mut bodies: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
                for blob in &frame.drv_blobs {
                    // Contract: digest = blake3(body), body canonical.
                    if blob.digest.as_slice() != blake3::hash(&blob.body).as_bytes() {
                        bail!("blob digest != blake3(body) for {}", blob.drv_path);
                    }
                    let decoded = rio_proto::drv::Derivation::decode(blob.body.as_slice())
                        .with_context(|| format!("blob body for {} undecodable", blob.drv_path))?;
                    if rio_proto::derivation_util::canonical_encode(&decoded) != blob.body {
                        bail!("blob body for {} is not canonical", blob.drv_path);
                    }
                    bodies.insert(blob.digest.clone(), blob.body.clone());
                }
                for node in &frame.nodes {
                    if node.drv_digest.len() != 32 {
                        bail!("node {} lacks a 32-byte drv_digest", node.drv_path);
                    }
                    // First sight must carry the body (worker contract).
                    if !nodes.contains_key(&node.drv_digest)
                        && !bodies.contains_key(&node.drv_digest)
                    {
                        bail!("node {} arrived without its body", node.drv_path);
                    }
                    nodes.insert(node.drv_digest.clone(), node.drv_path.clone());
                }
                summary.total_nodes = nodes.len();
                if !frame.root_drv_digest.is_empty() {
                    let root_path = nodes
                        .get(&frame.root_drv_digest)
                        .with_context(|| {
                            format!("root digest of '{}' resolves to no folded node", frame.attr)
                        })?
                        .clone();
                    pending.remove(&frame.attr);
                    summary.results.push(ResultSummary {
                        attr: frame.attr.clone(),
                        root_drv_path: root_path,
                        root_digest_hex: hex::encode(&frame.root_drv_digest),
                        new_nodes: frame.nodes.len(),
                        source_roots: frame.source_roots.len(),
                    });
                }
            }
            worker_frame::Msg::IfdRequest(req) => {
                // No cluster behind the harness: refuse like the
                // coordinator's --local-ifd path does.
                let drv_path = req.node.map(|n| n.drv_path).unwrap_or_default();
                writer
                    .send(coordinator_frame::Msg::IfdCompletion(
                        rio_proto::evaljob::IfdCompletion {
                            drv_path,
                            output_paths: vec![],
                            error: "eval-harness has no cluster for IFD".into(),
                        },
                    ))
                    .await?;
            }
            worker_frame::Msg::Expansion(exp) => {
                // Mirror the coordinator: the attrset attr resolves into
                // one WorkItem per derivation child.
                pending.remove(&exp.attr);
                summary.skipped.extend(exp.skipped);
                for child in &exp.children {
                    if pending.insert(child.clone()) {
                        writer
                            .send(coordinator_frame::Msg::Work(WorkItem {
                                attr: child.clone(),
                            }))
                            .await?;
                    }
                }
                summary.expansions.push((exp.attr, exp.children));
            }
            worker_frame::Msg::Recycle(_) => summary.recycles += 1,
            worker_frame::Msg::Note(n) => summary.notes.push(n.text),
            worker_frame::Msg::Error(e) => {
                if e.fatal {
                    bail!("eval parent fatal: {}", e.message);
                }
                // Mirror the coordinator: the empty attr is a real WorkItem
                // (zero-attr file mode), so an Error naming it while pending
                // is its eval failure, not an attr-less fault.
                if e.attr.is_empty() && !pending.contains("") {
                    summary.faults.push(e.message);
                } else {
                    pending.remove(&e.attr);
                    summary.eval_errors.push((e.attr, e.message));
                }
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        bail!("eval parent exited with {status}");
    }
    if !pending.is_empty() {
        bail!("attrs never resolved: {pending:?}");
    }
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// Kill one fork worker of the eval parent (crash injection). Workers
/// are direct children of the parent pid.
fn kill_one_fork_worker(parent_pid: u32) {
    for _ in 0..100 {
        let children =
            std::fs::read_to_string(format!("/proc/{parent_pid}/task/{parent_pid}/children"))
                .unwrap_or_default();
        if let Some(pid) = children.split_whitespace().next()
            && let Ok(pid) = pid.parse::<i32>()
        {
            eprintln!("eval-harness: killing fork worker {pid}");
            // SAFETY: SIGKILL to the located worker pid.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("eval-harness: no fork worker appeared to kill");
}
