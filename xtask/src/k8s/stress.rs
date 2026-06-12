//! `qa --load` implementation: one Indexed Job in ns `rio-load` whose
//! pods run `nix build --store ssh-ng://…` against the gateway
//! ClusterIP, plus a coordinator-side watch loop. Per-client logs are
//! collected into `.stress-test/{ts}/` when the Job finishes.
//!
//! Why in-cluster only: load clients used to run locally through SSM
//! port-forward tunnels, but those tunnels ride the EKS apiserver — a
//! Karpenter node surge (which heavy load always triggers) rolls
//! apiserver instances and severs ALL client tunnels simultaneously,
//! killing every client with "Nix daemon disconnected". Job pods dial
//! the gateway ClusterIP directly and survived the 2026-06-11
//! campaign's 512× wave with zero transport failures.
//!
//! The bench target is resolved to a `.drv` path ONCE on the
//! coordinator and every pod builds `<drv>^*` directly. The campaign
//! showed that per-client cold flake eval (each client fetching
//! nix-bench's ~8 flake inputs from github + cache.nixos.org)
//! dominates above ~128 clients — the ladder measured internet
//! egress, not rio. Drv-closure distribution: the coordinator `nix
//! copy`s the closure into the rio store through ONE tunnel, and each
//! pod pulls it back with `nix copy --from` before building — rio
//! itself is the distribution channel, no shared volume or extra
//! substituter; pods never evaluate.
//!
//! Pods authenticate with the operator key (Secret `rio-load-ssh`,
//! written by the coordinator) and PIN the gateway host key: the
//! public key is derived from the `rio-gateway-host-key` Secret and
//! shipped as a known_hosts file, so `StrictHostKeyChecking=yes`
//! holds even on the empty `nixos/nix` image (no
//! StrictHostKeyChecking=no in cluster manifests).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use console::style;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::ResourceExt;
use kube::api::{Api, DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use serde_json::json;
use tracing::{info, warn};

use super::provider::Provider;
use crate::config::XtaskConfig;
use crate::sh::repo_root;

/// Knobs for the load stage, owned by `qa::QaOpts`.
pub(super) struct LoadOpts {
    /// `packages.x86_64-linux` attribute of the bench flake.
    pub target: String,
    pub parallel: u16,
    /// Bench flake checkout; default `~/src/nix-bench/main`.
    pub bench_flake: Option<PathBuf>,
    /// Spread client starts evenly over this window instead of
    /// all-at-once. 512 simultaneous starts destabilized the EKS
    /// control plane in the 2026-06-11 campaign (~5min apiserver
    /// blackout, 7 NotReady nodes from Karpenter churn).
    pub stagger: Duration,
}

/// Namespace + Secret the load Job uses. The Secret carries the
/// operator ssh key AND the pinned gateway known_hosts.
const LOAD_NS: &str = "rio-load";
const LOAD_SECRET: &str = "rio-load-ssh";
/// Gateway ClusterIP DNS name as the pods see it — both the ssh-ng
/// store host and the known_hosts pin entry.
const GATEWAY_SSH_HOST: &str = "rio-gateway.rio-system.svc";
/// Digest-pinned client image (validated by the 2026-06-11 campaign).
const NIX_IMAGE: &str =
    "nixos/nix@sha256:e623d73af9cac82d1b50784c83e0cf2a4b83bfd2cfe8d5b67809a2fc94e043ac";

/// Start offset for client `i` of `n` when ramping over `window`:
/// evenly spaced, client 0 at t=0, client n-1 at `window * (n-1)/n`.
fn stagger_offset(i: u16, n: u16, window: Duration) -> Duration {
    if n == 0 || window.is_zero() {
        return Duration::ZERO;
    }
    let nanos = window.as_nanos() * u128::from(i) / u128::from(n);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// How many of `n` clients should have started by `elapsed`. The Job
/// ramp patches `spec.parallelism` to this value, so pod creation
/// (and the Karpenter provisioning it triggers) follows the schedule.
fn stagger_allowed(n: u16, window: Duration, elapsed: Duration) -> u16 {
    u16::try_from(
        (0..n)
            .take_while(|&i| stagger_offset(i, n, window) <= elapsed)
            .count(),
    )
    .expect("count of 0..n fits u16")
}

pub(super) async fn cmd_run(p: &dyn Provider, cfg: &XtaskConfig, opts: &LoadOpts) -> Result<()> {
    let bench = resolve_bench_flake(opts.bench_flake.clone())?;
    let installable = format!("{}#{}", bench.display(), opts.target);

    // Pre-resolve the target to a .drv ONCE — kills the cold-eval
    // thundering herd (module doc). Hard error: every client depends
    // on the resolved path.
    info!("resolving {installable} to a .drv (cold eval can take ~2min)");
    let drv = resolve_drv(&installable)?;
    info!("target resolved: {drv}");

    run_load_job(p, cfg, &drv, opts).await
}

/// Resolve `installable` to its `.drv` store path. Instantiates the
/// derivation closure into the local store as a side effect — that
/// closure is what the coordinator seeds into the rio store.
fn resolve_drv(installable: &str) -> Result<String> {
    let out = std::process::Command::new("nix")
        .args(["path-info", "--derivation", "--impure", installable])
        .output()
        .context("spawn nix path-info for drv pre-resolve")?;
    anyhow::ensure!(
        out.status.success(),
        "nix path-info --derivation {installable} failed: {}",
        std::str::from_utf8(&out.stderr)
            .unwrap_or("<non-utf8 stderr>")
            .trim()
    );
    parse_drv_output(std::str::from_utf8(&out.stdout).context("nix path-info stdout not utf-8")?)
}

/// Pure parse of `nix path-info --derivation` stdout (one store path).
fn parse_drv_output(stdout: &str) -> Result<String> {
    let path = stdout.trim();
    anyhow::ensure!(
        path.starts_with("/nix/store/") && path.ends_with(".drv") && path.lines().count() == 1,
        "expected a single .drv store path from nix path-info, got: {path:?}"
    );
    Ok(path.to_string())
}

/// Run the clients as one Indexed Job in `rio-load`. See the module
/// doc for the drv-distribution + host-key-pin design.
#[allow(clippy::print_stderr)]
async fn run_load_job(
    p: &dyn Provider,
    cfg: &XtaskConfig,
    drv: &str,
    opts: &LoadOpts,
) -> Result<()> {
    let n = opts.parallel;
    let key = crate::ssh::privkey_path(cfg)?;
    let client = crate::k8s::client::client().await?;

    // Host-key pin: derive the gateway's public host key from its
    // in-cluster Secret so pods verify server identity instead of
    // StrictHostKeyChecking=no (a MITM on the pod network could
    // otherwise impersonate the gateway and feed fabricated build
    // results / harvest the handshake).
    let host_key = crate::k8s::client::get_secret_key(
        &client,
        "rio-system",
        "rio-gateway-host-key",
        "host_key",
    )
    .await?
    .context(
        "Secret rio-system/rio-gateway-host-key not found — qa --load needs the \
         persistent gateway host key to pin known_hosts (EKS deploys set \
         gateway.ssh.hostKeySecret; emptyDir host keys can't be pinned)",
    )?;
    let known_hosts = known_hosts_line(&host_key)?;

    crate::k8s::client::ensure_namespace(&client, LOAD_NS, false).await?;
    let privkey = fs::read_to_string(&key)
        .with_context(|| format!("read ssh private key {}", key.display()))?;
    crate::k8s::client::apply_secret(
        &client,
        LOAD_NS,
        LOAD_SECRET,
        BTreeMap::from([
            ("id_ed25519".to_string(), privkey),
            ("known_hosts".to_string(), known_hosts),
        ]),
    )
    .await?;

    // Seed the drv closure into the rio store through ONE tunnel so
    // the pods (empty local stores) can pull it back without any eval.
    info!("seeding drv closure into the rio store");
    {
        let (port, _tunnel) = p.tunnel(0).await?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );
        let out = tokio::process::Command::new("nix")
            .args(["copy", "--to", &store, drv])
            .env("NIX_SSHOPTS", super::shared::NIX_SSHOPTS_BASE)
            .output()
            .await
            .context("spawn nix copy (drv closure seed)")?;
        anyhow::ensure!(
            out.status.success(),
            "nix copy --to {store} {drv} failed: {}",
            std::str::from_utf8(&out.stderr)
                .unwrap_or("<non-utf8 stderr>")
                .trim()
        );
    }

    let ts = jiff::Timestamp::now().as_second();
    let dir = repo_root().join(".stress-test").join(ts.to_string());
    fs::create_dir_all(&dir)?;
    let name = format!("rio-load-{ts}");
    let initial = stagger_allowed(n, opts.stagger, Duration::ZERO);
    let api: Api<Job> = Api::namespaced(client.clone(), LOAD_NS);
    api.create(&PostParams::default(), &wave_job(&name, drv, n, initial)?)
        .await
        .context("create load Job")?;
    eprintln!(
        "{} Job {LOAD_NS}/{name}: {n} client(s), parallelism ramp {initial}→{n} — Ctrl-C to abort",
        style("▸").blue(),
    );

    // `signal()`, NOT `ctrl_c()`: registers the sigaction synchronously
    // at call time, so a Ctrl-C before the first select! poll still
    // reaches the cleanup arm instead of the default disposition.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let start = std::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut parallelism = initial;
    let status = loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => {
                eprintln!("{} interrupted — deleting Job {LOAD_NS}/{name}", style("✗").red());
                // Foreground propagation: background-delete on a Job
                // races Job-Complete and orphans the job-tracking
                // finalizer (see ci-failure-patterns.md).
                let _ = api.delete(&name, &DeleteParams::foreground()).await;
                anyhow::bail!("interrupted");
            }
            _ = tick.tick() => {}
        }

        // --load-stagger: ramp spec.parallelism along the schedule. The
        // job controller creates pods only as parallelism allows, so
        // pod-creation (and Karpenter) load follows the ramp. 10s tick
        // granularity is plenty — the herd was 512-at-once, not 512-in-
        // 10s-buckets.
        let allowed = stagger_allowed(n, opts.stagger, start.elapsed());
        if allowed > parallelism {
            api.patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({"spec": {"parallelism": allowed}})),
            )
            .await
            .context("ramp Job parallelism")?;
            parallelism = allowed;
        }

        let st = api.get(&name).await?.status.unwrap_or_default();
        let (active, ok, failed) = (
            st.active.unwrap_or(0),
            st.succeeded.unwrap_or(0),
            st.failed.unwrap_or(0),
        );
        eprintln!(
            "{} parallelism={parallelism}/{n} active={active} ok={ok} failed={failed}",
            style("·").dim()
        );
        let done = st.conditions.as_ref().is_some_and(|cs| {
            cs.iter()
                .any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
        });
        if done {
            break st;
        }
    };

    // Collect per-client logs BEFORE reporting: ttlSecondsAfterFinished
    // reaps the pods (and their logs) 30min after the Job finishes.
    match collect_pod_logs(&client, &name, &dir).await {
        Ok(got) => eprintln!(
            "{} logs: {} ({got} pod(s))",
            style("·").dim(),
            dir.display()
        ),
        Err(e) => warn!("pod log collection failed: {e:#}"),
    }

    let ok = status.succeeded.unwrap_or(0);
    eprintln!();
    eprintln!("{} {ok}/{n} succeeded", style("✓").green());
    if ok < i32::from(n) {
        anyhow::bail!(
            "{} client(s) failed — see {}",
            i32::from(n) - ok,
            dir.display()
        );
    }
    Ok(())
}

/// Write each Job pod's log to `dir/<pod>.log` (the pod name carries
/// the completion index: `<job>-<index>-<suffix>`). Per-pod failures
/// are warnings, not errors — a partially collected run dir beats
/// losing everything to one unreadable pod.
async fn collect_pod_logs(client: &kube::Client, job: &str, dir: &Path) -> Result<usize> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), LOAD_NS);
    let list = pods
        .list(&ListParams::default().labels(&format!("job-name={job}")))
        .await
        .context("list Job pods")?;
    let mut got = 0usize;
    for pod in &list.items {
        let pod_name = pod.name_any();
        match pods.logs(&pod_name, &LogParams::default()).await {
            Ok(log) => {
                fs::write(dir.join(format!("{pod_name}.log")), log)?;
                got += 1;
            }
            Err(e) => warn!("logs for {pod_name}: {e:#}"),
        }
    }
    Ok(got)
}

/// known_hosts pin for the gateway: public key derived from the
/// private host key stored in `rio-system/rio-gateway-host-key`.
fn known_hosts_line(host_privkey: &str) -> Result<String> {
    let key = ssh_key::PrivateKey::from_openssh(host_privkey).context("parse gateway host key")?;
    let pubkey = key.public_key().to_openssh()?;
    Ok(format!("{GATEWAY_SSH_HOST} {pubkey}\n"))
}

/// Indexed-Job manifest for one load wave. Pure — unit-tested offline.
fn wave_job(name: &str, drv: &str, completions: u16, parallelism: u16) -> Result<Job> {
    // Pod flow: wait for the gateway to accept the key (3 consecutive
    // `nix store info` over ~5min budget — auth propagation), pull the
    // drv closure from the rio store (a plain .drv installable means
    // the derivation itself since Nix 2.13; closure copy includes all
    // input drvs/sources), then build with all jobs remote.
    let store =
        format!("ssh-ng://rio@{GATEWAY_SSH_HOST}?compress=true&ssh-key=/etc/rio-load/id_ed25519");
    let script = format!(
        r#"set -eu
store='{store}'
drv='{drv}'
okrun=0
for n in $(seq 1 60); do
  if nix store info --store "$store" >/dev/null 2>&1; then okrun=$((okrun+1)); else okrun=0; fi
  if [ "$okrun" -ge 3 ]; then echo "AUTH_OK idx=${{JOB_COMPLETION_INDEX:-?}} n=$n"; break; fi
  echo "waiting for gateway ($n/60 okrun=$okrun)"; sleep 5
done
nix copy --no-check-sigs --from "$store" "$drv"
echo "BUILD_START idx=${{JOB_COMPLETION_INDEX:-?}} $(date -u +%FT%TZ)"
nix build --store "$store" --eval-store auto --no-link -L --max-jobs 0 "$drv^*"
echo "BUILD_DONE idx=${{JOB_COMPLETION_INDEX:-?}} $(date -u +%FT%TZ)"
"#
    );
    // nixos/nix ships with experimental features OFF and an empty
    // known_hosts — without NIX_CONFIG + the pinned known_hosts no
    // client authenticates at all (campaign finding #2).
    let nix_config = "experimental-features = nix-command flakes";
    let sshopts = "-o UserKnownHostsFile=/etc/rio-load/known_hosts \
                   -o GlobalKnownHostsFile=/dev/null -o StrictHostKeyChecking=yes";
    serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": name,
            "namespace": LOAD_NS,
            "labels": {"app": "rio-load", "role": "wave"},
        },
        "spec": {
            "completions": completions,
            "parallelism": parallelism,
            "completionMode": "Indexed",
            "backoffLimit": 0,
            // 2h: connect+pull is minutes; the rest is remote build
            // wall. The prior 3h deadline only masked the eval herd.
            "activeDeadlineSeconds": 7200,
            // 30min: generous window for the coordinator to collect
            // pod logs (it does so immediately on Job completion).
            "ttlSecondsAfterFinished": 1800,
            "template": {
                "metadata": {"labels": {"app": "rio-load", "role": "wave"}},
                "spec": {
                    "restartPolicy": "Never",
                    "volumes": [
                        {"name": "ssh", "secret": {"secretName": LOAD_SECRET, "defaultMode": 0o400}},
                    ],
                    "containers": [{
                        "name": "client",
                        "image": NIX_IMAGE,
                        "env": [
                            {"name": "NIX_CONFIG", "value": nix_config},
                            {"name": "NIX_SSHOPTS", "value": sshopts},
                        ],
                        "volumeMounts": [
                            {"name": "ssh", "mountPath": "/etc/rio-load", "readOnly": true},
                        ],
                        "command": ["/bin/sh", "-c"],
                        "args": [script],
                    }],
                },
            },
        },
    }))
    .context("wave Job manifest")
}

fn resolve_bench_flake(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let p = explicit.unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("src/nix-bench/main")
    });
    anyhow::ensure!(
        p.join("flake.nix").exists(),
        "nix-bench flake not found at {}\n\
         (pass --bench-flake /path/to/nix-bench/main)",
        p.display()
    );
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drv_output_accepts_single_drv_path() {
        let p = "/nix/store/abc123-small-mixed-4x.drv\n";
        assert_eq!(
            parse_drv_output(p).unwrap(),
            "/nix/store/abc123-small-mixed-4x.drv"
        );
    }

    #[test]
    fn parse_drv_output_rejects_garbage() {
        // Empty (eval produced nothing), an output path (forgot
        // --derivation), and multi-line (multiple installables) would
        // each silently break every client — fail at the coordinator.
        for bad in [
            "",
            "/nix/store/abc123-small-mixed-4x",
            "error: attribute missing",
            "/nix/store/a.drv\n/nix/store/b.drv\n",
        ] {
            assert!(parse_drv_output(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn stagger_zero_window_starts_everyone_immediately() {
        for i in 0..8 {
            assert_eq!(stagger_offset(i, 8, Duration::ZERO), Duration::ZERO);
        }
        assert_eq!(stagger_allowed(8, Duration::ZERO, Duration::ZERO), 8);
    }

    #[test]
    fn stagger_offsets_evenly_spaced_inside_window() {
        let w = Duration::from_secs(64);
        let offsets: Vec<_> = (0..8).map(|i| stagger_offset(i, 8, w)).collect();
        assert_eq!(offsets[0], Duration::ZERO);
        assert_eq!(offsets[1], Duration::from_secs(8));
        assert_eq!(offsets[7], Duration::from_secs(56));
        assert!(offsets.windows(2).all(|p| p[0] < p[1]), "not monotonic");
        assert!(*offsets.last().unwrap() < w, "last start must be < window");
    }

    #[test]
    fn stagger_allowed_matches_offsets() {
        let (n, w) = (512u16, Duration::from_secs(300));
        // One client at t=0, all by the end of the window, and at any
        // instant `allowed` equals the number of due offsets.
        assert_eq!(stagger_allowed(n, w, Duration::ZERO), 1);
        assert_eq!(stagger_allowed(n, w, w), n);
        for elapsed in [1, 75, 150, 299].map(Duration::from_secs) {
            let due = (0..n)
                .filter(|&i| stagger_offset(i, n, w) <= elapsed)
                .count();
            assert_eq!(usize::from(stagger_allowed(n, w, elapsed)), due);
        }
    }

    #[test]
    fn known_hosts_pins_gateway_pubkey() {
        let (privkey, pubkey) = crate::ssh::generate("host").unwrap();
        let line = known_hosts_line(&privkey).unwrap();
        // `<host> <type> <base64> [comment]` — the exact pubkey of the
        // gateway's persistent host key, pinned to the ClusterIP name
        // the pods dial.
        let b64 = pubkey.split_whitespace().nth(1).unwrap();
        assert!(line.starts_with("rio-gateway.rio-system.svc ssh-ed25519 "));
        assert!(line.contains(b64), "pubkey not pinned: {line}");
    }

    #[test]
    fn wave_job_manifest_shape() {
        let job = wave_job("rio-load-test", "/nix/store/abc-x.drv", 512, 4).unwrap();
        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.completions, Some(512));
        assert_eq!(spec.parallelism, Some(4));
        assert_eq!(spec.completion_mode.as_deref(), Some("Indexed"));
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(spec.active_deadline_seconds, Some(7200));
        assert_eq!(spec.ttl_seconds_after_finished, Some(1800));

        let pod = spec.template.spec.as_ref().unwrap();
        let c = &pod.containers[0];
        assert_eq!(c.image.as_deref(), Some(NIX_IMAGE));

        let env = |k: &str| {
            c.env
                .as_ref()
                .unwrap()
                .iter()
                .find(|e| e.name == k)
                .and_then(|e| e.value.clone())
                .unwrap()
        };
        // Campaign finding #2: without these two, no client ever
        // authenticates (experimental features off, empty known_hosts).
        assert_eq!(
            env("NIX_CONFIG"),
            "experimental-features = nix-command flakes"
        );
        let sshopts = env("NIX_SSHOPTS");
        assert!(sshopts.contains("StrictHostKeyChecking=yes"));
        assert!(sshopts.contains("UserKnownHostsFile=/etc/rio-load/known_hosts"));
        // The host-key BYPASS must never ship in a cluster manifest.
        assert!(!sshopts.contains("StrictHostKeyChecking=no"));

        let script = &c.args.as_ref().unwrap()[0];
        // No flake ref anywhere: clients pull the pre-seeded drv
        // closure from rio and build it — zero client-side eval.
        assert!(script.contains(r#"nix copy --no-check-sigs --from "$store" "$drv""#));
        assert!(script.contains("/nix/store/abc-x.drv"));
        assert!(script.contains(r#""$drv^*""#));
        assert!(!script.contains("github:"));

        let vol = &pod.volumes.as_ref().unwrap()[0];
        let secret = vol.secret.as_ref().unwrap();
        assert_eq!(secret.secret_name.as_deref(), Some(LOAD_SECRET));
        assert_eq!(secret.default_mode, Some(0o400));
    }
}
