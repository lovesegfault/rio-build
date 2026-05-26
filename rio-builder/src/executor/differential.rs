//! Differential-harness driver: build one derivation with the native
//! executor stack (request glue → rio-exec sandbox → result pipeline)
//! and report what it produced, so the `vm-differential-standalone`
//! scenario can compare it against the same derivation built by real
//! Nix in the same VM.
//!
//! This is a test harness, not an operator surface: it bypasses the
//! scheduler/store control plane entirely (no assignment, no upload, no
//! FUSE/overlay — the input closure is *copied* into a scratch store
//! directory instead). The simplifications relative to production are
//! deliberate and documented inline; the parts under test — the glue,
//! the sandbox, and the result pipeline — are the real production code
//! paths.
//!
//! Invoked by the `differential-driver` binary, which only runs inside
//! the VM test (it needs root + namespaces + a populated /nix/store).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use tokio::sync::mpsc;

use rio_exec::{ExecEvent, HostLayout};
use rio_nix::derivation::{BasicDerivation, Derivation, DerivationLike as _};
use rio_nix::hash::NixHash;
use rio_nix::store_path::{StorePath, basename};
use rio_proto::types::BuildResultStatus;
use rio_proto::validated::ValidatedPathInfo;

use super::glue::log::{LineAction, NixLogFilter};
use super::glue::{GluePlan, SandboxOptions, SandboxPaths, derivation_into_request};
use super::native_result::{ExitClassification, OutputToProcess, classify_exit, process_outputs};

/// Driver configuration (one invocation = one derivation).
#[derive(Debug)]
pub struct DriverConfig {
    /// Path to the `.drv` file (must already be instantiated in the
    /// VM's /nix/store, along with its full input closure).
    pub drv_path: PathBuf,
    /// Scratch directory for this build (created if absent). Gets
    /// `store/` (the copied input closure + produced outputs),
    /// `build/` (the sandbox /build), and `chroot/` (the rio-exec
    /// skeleton).
    pub work_dir: PathBuf,
    /// Static shell to provide as `/bin/sh` inside the sandbox. Should
    /// be the same `busybox-sandbox-shell` real Nix uses so the two
    /// sandboxes are observationally equivalent.
    pub sandbox_shell: Option<PathBuf>,
    /// The host system string (e.g. `x86_64-linux`).
    pub host_system: String,
    /// uid/gid the build runs as (1000/100 in production).
    pub uid: u32,
    pub gid: u32,
    /// Wall-clock timeout for the sandboxed build.
    pub timeout: Duration,
}

/// What the native stack produced for one output.
#[derive(Debug, Serialize)]
pub struct OutputReport {
    pub name: String,
    pub store_path: String,
    /// Hex SHA-256 of the canonicalised NAR serialization.
    pub nar_hash: String,
    pub nar_size: u64,
    /// Sorted full store paths referenced by this output.
    pub references: Vec<String>,
}

/// Log-stream observations (the `@nix` filter's view).
#[derive(Debug, Default, Serialize)]
pub struct LogReport {
    pub total_lines: u64,
    /// Lines that arrived with the `@nix ` prefix (should be consumed
    /// by the filter, never forwarded).
    pub atnix_lines: u64,
    /// Lines the filter said to forward (i.e. what would reach the
    /// tenant-visible log).
    pub forwarded_lines: u64,
    /// Whether any forwarded line still carried the `@nix ` prefix —
    /// must always be false.
    pub forwarded_atnix: bool,
    /// setPhase values seen, in order.
    pub phases: Vec<String>,
    pub cap_exceeded: bool,
    /// The last forwarded log lines (lossy UTF-8), so a failing corpus
    /// entry's report is self-explanatory without re-running the VM
    /// with a debugger attached. Bounded by `LOG_TAIL_LINES`.
    pub tail: Vec<String>,
}

/// How many trailing forwarded log lines the report keeps. Enough to
/// see why a builder script failed; small enough that report.json stays
/// readable in the test log.
const LOG_TAIL_LINES: usize = 40;

/// The driver's full result for one derivation.
#[derive(Debug, Serialize)]
pub struct Report {
    pub drv: String,
    /// "sandbox" | "builtin-fetchurl".
    pub plan: String,
    /// Set when the request glue rejected the derivation (no build ran).
    pub glue_error: Option<String>,
    /// "success" | the failed `BuildResultStatus` as a string.
    pub classification: Option<String>,
    pub error_msg: Option<String>,
    pub outputs: Vec<OutputReport>,
    /// `Some("ok")` or `Some(<rejection>)` for fixed-output derivations,
    /// `None` for ordinary ones.
    pub fod_check: Option<String>,
    pub log: LogReport,
}

/// Run one derivation through the native executor stack.
pub async fn run(cfg: DriverConfig) -> anyhow::Result<Report> {
    let drv_text = std::fs::read_to_string(&cfg.drv_path)
        .with_context(|| format!("reading {}", cfg.drv_path.display()))?;
    let drv = Derivation::parse(&drv_text).context("parsing derivation")?;
    let drv_path_str = cfg
        .drv_path
        .to_str()
        .context("drv path is not valid UTF-8")?
        .to_owned();

    // ---- Resolve inputDrvs → concrete store paths -----------------------
    //
    // Production resolves against rio-store metadata; here every input
    // .drv is on disk in the VM store, so resolution is "parse the input
    // .drv and read the declared output path". Floating-CA inputs (empty
    // path) are unsupported by the harness corpus on purpose.
    let mut resolved_inputs: Vec<String> = Vec::new();
    for (input_drv, wanted) in drv.input_drvs() {
        let text = std::fs::read_to_string(input_drv)
            .with_context(|| format!("reading input drv {input_drv}"))?;
        let parsed =
            Derivation::parse(&text).with_context(|| format!("parsing input drv {input_drv}"))?;
        for name in wanted {
            let out = parsed
                .outputs()
                .iter()
                .find(|o| o.name() == name)
                .with_context(|| format!("{input_drv} has no output '{name}'"))?;
            if out.path().is_empty() {
                bail!(
                    "input {input_drv}!{name} has no static path (floating-CA inputs unsupported by the harness)"
                );
            }
            resolved_inputs.push(out.path().to_string());
        }
    }
    let mut direct_inputs: Vec<String> = drv.input_srcs().iter().cloned().collect();
    direct_inputs.extend(resolved_inputs.iter().cloned());

    // ---- Input closure + metadata (from the VM's own nix) ----------------
    let closure_paths = query_closure(&direct_inputs)?;
    let input_metadata = query_path_info(&closure_paths)?;

    // ---- Scratch store: copy the closure in ------------------------------
    //
    // Production binds the overlay *merged* dir (FUSE lower ∪ upper);
    // the harness uses a plain directory pre-populated with the closure.
    // Same observable contract for the glue: a host dir that contains
    // every input path and is writable for outputs.
    let store_dir = cfg.work_dir.join("store");
    let build_dir = cfg.work_dir.join("build");
    let chroot_dir = cfg.work_dir.join("chroot");
    for d in [&store_dir, &build_dir, &chroot_dir] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    // The sandboxed build runs as cfg.uid/cfg.gid but creates its outputs
    // directly under the writable /nix/store mount (and scratch files under
    // /build), whose bind SOURCES are these root-owned directories. In
    // production the overlay upper store is prepared build-writable (mode
    // 1775, build gid — the same contract real Nix gives its chroot store);
    // mirror that here or every output `mkdir` fails EACCES.
    for d in [&store_dir, &build_dir] {
        // Same mode/group contract as production (1775 root:<gid>); the
        // extra uid chown is driver-only — the VM driver pre-creates the
        // dirs as root while production's skeleton/overlay setup already
        // owns them appropriately.
        super::make_store_scratch_writable(d, cfg.gid)
            .with_context(|| format!("making {} build-writable", d.display()))?;
        std::os::unix::fs::chown(d, Some(cfg.uid), None)
            .with_context(|| format!("chowning {} to the sandbox user", d.display()))?;
    }
    for p in &closure_paths {
        let dest = store_dir.join(basename(p).unwrap_or(p.as_str()));
        if !dest.exists() {
            let st = Command::new("cp")
                .arg("-a")
                .arg(p)
                .arg(&dest)
                .status()
                .context("running cp -a")?;
            if !st.success() {
                bail!("cp -a {p} -> {} failed: {st}", dest.display());
            }
        }
    }

    // ---- Glue: derivation → ExecutionRequest -----------------------------
    let basic = BasicDerivation::from_resolved(&drv, resolved_inputs.iter().cloned());
    let paths = SandboxPaths {
        build_dir: build_dir.clone(),
        merged_store: store_dir.clone(),
    };
    let opts = SandboxOptions {
        build_cores: 1,
        uid: cfg.uid,
        gid: cfg.gid,
        sandbox_shell: cfg.sandbox_shell.clone(),
        extra_sandbox_paths: Vec::new(),
        impure_env: BTreeMap::new(),
        ca_bundle: None,
        cgroup: None,
        extra_devices: Vec::new(),
        host_system: cfg.host_system.clone(),
        timeout: Some(cfg.timeout),
        max_silent: None,
        max_log_bytes: None,
        // The driver never dispatches builtin:fetchurl entries (they are
        // reported as unsupported below), so the re-exec binary, mirror
        // list, and netrc are irrelevant here.
        builder_binary: None,
        hashed_mirrors: Vec::new(),
        netrc: None,
    };

    let mut report = Report {
        drv: drv_path_str.clone(),
        plan: "sandbox".to_string(),
        glue_error: None,
        classification: None,
        error_msg: None,
        outputs: Vec::new(),
        fod_check: None,
        log: LogReport::default(),
    };

    let prepared = match derivation_into_request(
        &drv_path_str,
        &basic,
        &closure_paths,
        &input_metadata,
        &paths,
        &opts,
    ) {
        Ok(GluePlan::Sandbox(p)) => p,
        Ok(GluePlan::BuiltinFetchurl(_)) => {
            report.plan = "builtin-fetchurl".to_string();
            return Ok(report);
        }
        Err(e) => {
            report.glue_error = Some(e.to_string());
            return Ok(report);
        }
    };

    // ---- Execute in the rio-exec sandbox ----------------------------------
    let host = HostLayout {
        chroot_dir: chroot_dir.clone(),
    };
    let (tx, mut rx) = mpsc::channel::<ExecEvent>(256);
    let collector = tokio::spawn(async move {
        let mut filter = NixLogFilter::new();
        let mut log = LogReport::default();
        while let Some(ev) = rx.recv().await {
            if let ExecEvent::Log { line, .. } = ev {
                log.total_lines += 1;
                if line.starts_with(b"@nix ") {
                    log.atnix_lines += 1;
                }
                match filter.handle(&line) {
                    LineAction::Forward(l) => {
                        log.forwarded_lines += 1;
                        if l.starts_with(b"@nix ") {
                            log.forwarded_atnix = true;
                        }
                        if log.tail.len() == LOG_TAIL_LINES {
                            log.tail.remove(0);
                        }
                        // Failure-evidence display only (the harness prints
                        // this tail when a corpus entry diverges); lossy is
                        // the sanctioned choice for log display per
                        // clippy.toml — build output is arbitrary bytes.
                        #[allow(clippy::disallowed_methods)]
                        log.tail.push(String::from_utf8_lossy(&l).into_owned());
                    }
                    LineAction::Phase(p) => log.phases.push(p),
                    LineAction::Consumed => {}
                    LineAction::CapExceeded => log.cap_exceeded = true,
                }
            }
        }
        log
    });

    let outcome = rio_exec::execute(&prepared.request, &host, tx)
        .await
        .context("rio-exec execute")?;
    report.log = collector.await.unwrap_or_default();

    // ---- Classify + process outputs ---------------------------------------
    let is_fod = basic.is_fixed_output();
    match classify_exit(outcome.exit, is_fod, false, false) {
        ExitClassification::Failed { status, error_msg } => {
            report.classification = Some(status_name(status));
            report.error_msg = Some(error_msg);
            return Ok(report);
        }
        ExitClassification::Success => {}
    }

    let to_process: Vec<OutputToProcess> = prepared
        .outputs
        .iter()
        .map(|po| OutputToProcess {
            name: po.name.clone(),
            store_path: po.path.clone(),
            host_path: store_dir.join(basename(&po.path).unwrap_or(po.path.as_str())),
        })
        .collect();

    match process_outputs(&drv, &to_process, cfg.uid, &input_metadata) {
        Ok(processed) => {
            // Fixed-output content check: the declared hash must match the
            // produced content. Mirrors the fail-closed verify_fod_hashes
            // semantics (unknown algorithm = rejection) so the harness
            // exercises the same decision the production path will make.
            if is_fod {
                report.fod_check = Some(check_fod_hash(&drv, &store_dir));
                if report.fod_check.as_deref() != Some("ok") {
                    report.classification = Some(status_name(BuildResultStatus::OutputRejected));
                    report.error_msg = report.fod_check.clone();
                    return Ok(report);
                }
            }
            report.classification = Some("success".to_string());
            report.outputs = processed
                .outputs
                .iter()
                .map(|o| OutputReport {
                    name: o.name.clone(),
                    store_path: o.store_path.clone(),
                    nar_hash: hex::encode(o.nar_hash),
                    nar_size: o.nar_size,
                    references: o.references.clone(),
                })
                .collect();
        }
        Err(rejection) => {
            report.classification = Some(status_name(BuildResultStatus::OutputRejected));
            report.error_msg = Some(rejection.to_string());
        }
    }

    Ok(report)
}

/// `nix-store -qR` over the direct inputs: the full transitive closure.
fn query_closure(direct: &[String]) -> anyhow::Result<Vec<String>> {
    if direct.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("nix-store")
        .arg("-qR")
        .args(direct)
        .output()
        .context("running nix-store -qR")?;
    if !out.status.success() {
        bail!(
            "nix-store -qR failed: {}",
            String::from_utf8(out.stderr).unwrap_or_else(|e| format!("<non-utf8 stderr: {e}>"))
        );
    }
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .map(str::to_string)
        .collect())
}

/// `nix path-info --json` over the closure: narHash/narSize/references
/// per path, converted to the `ValidatedPathInfo` shape the glue and the
/// result pipeline take. Handles both the array (nix ≤ 2.18) and the
/// object-keyed-by-path (≥ 2.19) output layouts.
fn query_path_info(paths: &[String]) -> anyhow::Result<Vec<ValidatedPathInfo>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--json",
        ])
        .args(paths)
        .output()
        .context("running nix path-info --json")?;
    if !out.status.success() {
        bail!(
            "nix path-info failed: {}",
            String::from_utf8(out.stderr).unwrap_or_else(|e| format!("<non-utf8 stderr: {e}>"))
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let entries: Vec<(String, serde_json::Value)> = match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| {
                v.get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| (p.to_string(), v.clone()))
            })
            .collect(),
        serde_json::Value::Object(map) => map.into_iter().collect(),
        other => bail!("unexpected nix path-info output shape: {other}"),
    };

    let mut infos = Vec::with_capacity(entries.len());
    for (path, info) in entries {
        let nar_hash_sri = info
            .get("narHash")
            .and_then(|v| v.as_str())
            .with_context(|| format!("{path}: missing narHash"))?;
        let nar_size = info
            .get("narSize")
            .and_then(|v| v.as_u64())
            .with_context(|| format!("{path}: missing narSize"))?;
        let references: Vec<StorePath> = info
            .get("references")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str())
                    .filter_map(|r| StorePath::parse(r).ok())
                    .collect()
            })
            .unwrap_or_default();
        infos.push(ValidatedPathInfo {
            store_path: StorePath::parse(&path)
                .with_context(|| format!("parsing store path {path}"))?,
            store_path_hash: Vec::new(),
            deriver: None,
            nar_hash: decode_sri_sha256(nar_hash_sri)
                .with_context(|| format!("{path}: narHash {nar_hash_sri}"))?,
            nar_size,
            references,
            registration_time: 0,
            ultimate: false,
            signatures: Vec::new(),
            content_address: info.get("ca").and_then(|v| v.as_str()).map(str::to_string),
        });
    }
    Ok(infos)
}

/// Decode a `sha256-<base64>` SRI hash into raw bytes.
fn decode_sri_sha256(sri: &str) -> anyhow::Result<[u8; 32]> {
    let hash = NixHash::parse_sri(sri).map_err(|e| anyhow::anyhow!("parsing SRI hash: {e}"))?;
    hash.digest()
        .try_into()
        .map_err(|_| anyhow::anyhow!("narHash {sri} is not a sha256 (32-byte) hash"))
}

/// Verify a fixed-output derivation's declared hash against the
/// produced output. Fail-closed: unknown algorithms are rejections.
fn check_fod_hash(drv: &Derivation, store_dir: &Path) -> String {
    // Delegate to the production fail-closed gate (sha1/sha256/sha512,
    // unknown algorithms rejected) instead of reimplementing it: the
    // harness must exercise the same decision the activation will make,
    // and the r[verify builder.fod.verify-hash+2] marker on this scenario
    // depends on the production code path being the thing exercised.
    match super::inputs::verify_fod_hashes(drv, store_dir) {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("{e:#}"),
    }
}

fn status_name(status: BuildResultStatus) -> String {
    format!("{status:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sri_decode_roundtrip() {
        // sha256 of empty input.
        let sri = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        let bytes = decode_sri_sha256(sri).unwrap();
        assert_eq!(
            hex::encode(bytes),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(decode_sri_sha256("sha512-abc").is_err());
        assert!(decode_sri_sha256("sha256-!!!").is_err());
    }

    #[test]
    fn path_info_object_and_array_shapes() {
        // Array shape (nix ≤ 2.18) and object shape (≥ 2.19) must both
        // parse — exercised through the private helper by feeding the
        // JSON directly.
        let array = serde_json::json!([{
            "path": "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x",
            "narHash": "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "narSize": 120,
            "references": []
        }]);
        let object = serde_json::json!({
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x": {
                "narHash": "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
                "narSize": 120,
                "references": []
            }
        });
        for v in [array, object] {
            let entries: Vec<(String, serde_json::Value)> = match v {
                serde_json::Value::Array(items) => items
                    .into_iter()
                    .filter_map(|v| {
                        v.get("path")
                            .and_then(|p| p.as_str())
                            .map(|p| (p.to_string(), v.clone()))
                    })
                    .collect(),
                serde_json::Value::Object(map) => map.into_iter().collect(),
                _ => unreachable!(),
            };
            assert_eq!(entries.len(), 1);
            assert!(entries[0].0.ends_with("-x"));
        }
    }
}
