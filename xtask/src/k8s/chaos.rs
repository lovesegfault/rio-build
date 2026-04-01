//! `xtask k8s stress chaos` — structured network fault injection.
//!
//! **Why this exists (I-048c).** The h2 keepalive path on the balanced
//! channel only fires when the peer's IP goes blackhole — packets
//! dropped, no FIN, no RST. Process death (even SIGKILL) doesn't test
//! it: the kernel reaps, closes FDs, sends FIN, the worker sees a
//! clean close in sub-ms. `kubectl delete pod --grace-period=0` still
//! SIGTERMs first. iptables on the host is blocked by EKS `brush`
//! allowed-programs. NetworkPolicy is disabled in this cluster.
//!
//! What does test it: a privileged hostNetwork pod, pinned to the
//! worker's node, that nsenters the host mount namespace and inserts
//! iptables DROP rules in a dedicated chain. Packets to/from the
//! target IP silently disappear. The h2 keepalive (30s interval, 10s
//! timeout) detects the dead connection at ~40s.
//!
//! **Self-cleaning.** The chaos pod's shell traps TERM/EXIT and
//! flushes its chain. After `<duration>` the sleep returns → trap
//! fires → rules gone. xtask deletes the pod on its way out (Ctrl-C
//! or normal completion).
//!
//! **SIGKILL-safe.** Same discipline as `stress run`: pod identity is
//! flushed to `<session>/chaos.json` BEFORE the iptables rules go in.
//! `stress cleanup` reads chaos.json, deletes any stale chaos pod,
//! then spawns a one-shot remediation pod on each affected node that
//! flushes/deletes the chain (idempotent — `iptables -F` on a missing
//! chain is a no-op with `2>/dev/null`).

use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use console::style;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::k8s::{NS, NS_BUILDERS, NS_FETCHERS, NS_STORE};
use crate::kube as k;

/// iptables chain name. Dedicated chain (not bare FORWARD inserts) so
/// cleanup is `-F <chain>; -D FORWARD -j <chain>; -X <chain>` —
/// idempotent and unambiguous. A bare `iptables -D FORWARD -s <ip> -j
/// DROP` on cleanup would need to know the exact rule it inserted; a
/// chain just gets flushed.
const CHAIN: &str = "RIO-CHAOS";

/// Namespace for chaos pods. `rio-system` is PSA `baseline`, but
/// `hostNetwork: true` + `privileged: true` needs `privileged`. The
/// builders/fetchers namespaces already are. Use builders — chaos pods
/// don't need fetcher's egress allowance (they nsenter to the host
/// anyway, NetworkPolicy doesn't apply to hostNetwork).
const CHAOS_NS: &str = NS_BUILDERS;

/// Same digest-pinned busybox the seccomp-installer uses (see
/// `infra/helm/rio-build/values.yaml`). Busybox has nsenter built in
/// — no need for a separate iptables image; we nsenter into PID 1's
/// mount namespace and use the host's `/usr/sbin/iptables`.
const BUSYBOX_IMAGE: &str = "public.ecr.aws/docker/library/busybox:1.36.1@sha256:b9598f8c98e24d0ad42c1742c32516772c3aa2151011ebaf639089bd18c605b8";

// ─── CLI types ──────────────────────────────────────────────────────

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChaosKind {
    /// iptables DROP on src+dst — pod IP becomes unroutable from the
    /// victim node. No FIN, no RST; the only signal is keepalive
    /// timeout.
    Blackhole,
    // Future: Latency (tc netem), Partition (multi-target), Flap
    // (drop/restore cycles). Only Blackhole is wired.
}

/// What to blackhole. `--target scheduler-leader` resolves the lease;
/// `builder-<N>` / `fetcher-<N>` index into the role-labeled pod set
/// (sorted by name, so `-0` is the lowest StatefulSet ordinal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChaosTarget {
    SchedulerLeader,
    Store,
    Builder(u32),
    Fetcher(u32),
}

impl FromStr for ChaosTarget {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "scheduler-leader" => Ok(Self::SchedulerLeader),
            "store" => Ok(Self::Store),
            _ => parse_ordinal(s, "builder-")
                .map(Self::Builder)
                .or_else(|| parse_ordinal(s, "fetcher-").map(Self::Fetcher))
                .with_context(|| {
                    format!(
                        "invalid --target {s:?} \
                         (expected: scheduler-leader, store, builder-<N>, fetcher-<N>)"
                    )
                }),
        }
    }
}

impl fmt::Display for ChaosTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchedulerLeader => f.write_str("scheduler-leader"),
            Self::Store => f.write_str("store"),
            Self::Builder(n) => write!(f, "builder-{n}"),
            Self::Fetcher(n) => write!(f, "fetcher-{n}"),
        }
    }
}

/// Which workers lose connectivity. Resolves to a set of node names —
/// the chaos pod runs hostNetwork on each, so the iptables rules
/// affect all pod-to-pod traffic transiting that node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChaosFrom {
    /// Every node hosting a builder OR fetcher pod (deduped).
    AllWorkers,
    Builder(u32),
    Fetcher(u32),
}

impl FromStr for ChaosFrom {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "all-workers" => Ok(Self::AllWorkers),
            _ => parse_ordinal(s, "builder-")
                .map(Self::Builder)
                .or_else(|| parse_ordinal(s, "fetcher-").map(Self::Fetcher))
                .with_context(|| {
                    format!(
                        "invalid --from {s:?} \
                         (expected: all-workers, builder-<N>, fetcher-<N>)"
                    )
                }),
        }
    }
}

impl fmt::Display for ChaosFrom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllWorkers => f.write_str("all-workers"),
            Self::Builder(n) => write!(f, "builder-{n}"),
            Self::Fetcher(n) => write!(f, "fetcher-{n}"),
        }
    }
}

/// Parse `<prefix><N>` → `Some(N)`. None on no-match (caller chains
/// with `or_else`).
fn parse_ordinal(s: &str, prefix: &str) -> Option<u32> {
    s.strip_prefix(prefix)?.parse().ok()
}

/// `60s` / `60` → 60 seconds. Tiny parser — no humantime dep just for
/// one suffix.
pub fn parse_duration_secs(s: &str) -> Result<Duration> {
    let s = s.strip_suffix('s').unwrap_or(s);
    let secs: u64 = s
        .parse()
        .with_context(|| format!("invalid duration {s:?} (expected <N> or <N>s)"))?;
    Ok(Duration::from_secs(secs))
}

// ─── state file ─────────────────────────────────────────────────────

/// One chaos pod's identity, written to `<session>/chaos.json` BEFORE
/// the pod's iptables rules go in. `stress cleanup` reads this to
/// remediate even if xtask was SIGKILLed mid-run.
#[derive(Serialize, Deserialize, Clone)]
pub struct ChaosEntry {
    pub node: String,
    pub pod_name: String,
    pub target_ip: String,
    pub chain: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct ChaosState {
    pub entries: Vec<ChaosEntry>,
}

/// Same write-then-rename atomicity as `stress::write_pids`.
pub fn write_chaos(dir: &Path, st: &ChaosState) -> Result<()> {
    let tmp = dir.join("chaos.json.tmp");
    let dst = dir.join("chaos.json");
    fs::write(&tmp, serde_json::to_string_pretty(st)?)?;
    fs::rename(tmp, dst)?;
    Ok(())
}

pub fn read_chaos(dir: &Path) -> Result<ChaosState> {
    let p = dir.join("chaos.json");
    if !p.exists() {
        return Ok(ChaosState::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(p)?)?)
}

// ─── target / from resolution ───────────────────────────────────────

/// Resolve a target to its pod IP.
async fn resolve_target_ip(client: &k::Client, target: &ChaosTarget) -> Result<String> {
    let (ns, pod_name) = match target {
        ChaosTarget::SchedulerLeader => (NS, k::scheduler_leader(client, NS).await?),
        ChaosTarget::Store => {
            let name = first_pod_by_label(
                client,
                NS_STORE,
                "app.kubernetes.io/name=rio-store",
                "store",
            )
            .await?;
            (NS_STORE, name)
        }
        ChaosTarget::Builder(n) => (NS_BUILDERS, nth_worker(client, "builder", *n).await?),
        ChaosTarget::Fetcher(n) => (NS_FETCHERS, nth_worker(client, "fetcher", *n).await?),
    };
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let pod = pods.get(&pod_name).await?;
    pod.status
        .and_then(|s| s.pod_ip)
        .with_context(|| format!("pod {ns}/{pod_name} has no podIP (not running?)"))
}

/// Resolve `--from` to a set of (description, node_name) pairs. The
/// description is for human output; node_name is what pins the chaos
/// pod via `spec.nodeName`.
async fn resolve_from_nodes(client: &k::Client, from: &ChaosFrom) -> Result<Vec<(String, String)>> {
    match from {
        ChaosFrom::AllWorkers => {
            let mut nodes = vec![];
            for (ns, role) in [(NS_BUILDERS, "builder"), (NS_FETCHERS, "fetcher")] {
                let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
                for p in pods
                    .list(&ListParams::default().labels(&format!("rio.build/role={role}")))
                    .await?
                {
                    if let Some(node) = p.spec.and_then(|s| s.node_name) {
                        let desc = p.metadata.name.unwrap_or_default();
                        nodes.push((desc, node));
                    }
                }
            }
            // Dedup by node (multiple workers can share a node).
            // Stable sort first so the kept description is deterministic.
            nodes.sort_by(|a, b| a.1.cmp(&b.1));
            nodes.dedup_by(|a, b| a.1 == b.1);
            anyhow::ensure!(!nodes.is_empty(), "no worker pods found");
            Ok(nodes)
        }
        ChaosFrom::Builder(n) => {
            let name = nth_worker(client, "builder", *n).await?;
            let node = node_of(client, NS_BUILDERS, &name).await?;
            Ok(vec![(name, node)])
        }
        ChaosFrom::Fetcher(n) => {
            let name = nth_worker(client, "fetcher", *n).await?;
            let node = node_of(client, NS_FETCHERS, &name).await?;
            Ok(vec![(name, node)])
        }
    }
}

/// Nth pod with `rio.build/role=<role>`, sorted by name. StatefulSet
/// pod names end in `-<ordinal>`, so sorted order = ordinal order.
async fn nth_worker(client: &k::Client, role: &str, n: u32) -> Result<String> {
    let ns = match role {
        "builder" => NS_BUILDERS,
        "fetcher" => NS_FETCHERS,
        _ => unreachable!(),
    };
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let mut names: Vec<String> = pods
        .list(&ListParams::default().labels(&format!("rio.build/role={role}")))
        .await?
        .into_iter()
        .filter_map(|p| p.metadata.name)
        .collect();
    names.sort();
    names.into_iter().nth(n as usize).with_context(|| {
        format!(
            "no {role} pod at ordinal {n} (have: {} pods)",
            names_len(ns)
        )
    })
}

// Helper for the error message above (can't borrow `names` after
// into_iter consumes it).
fn names_len(_ns: &str) -> &'static str {
    "see `kubectl get pods -l rio.build/role=...`"
}

async fn first_pod_by_label(
    client: &k::Client,
    ns: &str,
    selector: &str,
    desc: &str,
) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    pods.list(&ListParams::default().labels(selector))
        .await?
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter_map(|p| p.metadata.name)
        .next()
        .with_context(|| format!("no {desc} pod found (selector {selector:?} in ns {ns})"))
}

async fn node_of(client: &k::Client, ns: &str, pod_name: &str) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    pods.get(pod_name)
        .await?
        .spec
        .and_then(|s| s.node_name)
        .with_context(|| format!("pod {ns}/{pod_name} has no nodeName (not scheduled?)"))
}

// ─── chaos pod spec ─────────────────────────────────────────────────

/// Shell script the chaos pod runs. Busybox sh — no bashisms.
///
/// The script nsenters PID 1's mount+net namespace (host), so
/// `iptables` is the host binary and the rules land in the host
/// netfilter tables. `hostNetwork: true` means we're already IN the
/// host net namespace, but `-n` is harmless and explicit.
///
/// Chain protocol:
///   1. `-N` create (or `-F` flush if it exists from a prior crashed run)
///   2. `-C FORWARD -j CHAIN || -I FORWARD -j CHAIN` — link the chain
///      into FORWARD only if not already linked (idempotent)
///   3. `-A` append the DROP rules (both directions)
///   4. trap: flush, unlink, delete chain. `2>/dev/null` makes each
///      step a no-op if a prior step already cleaned (e.g., concurrent
///      remediation pod from `stress cleanup`).
///
/// `echo ACTIVE > /proc/1/root/tmp/rio-chaos-state` writes a marker
/// to the HOST /tmp (we're in PID 1's mount ns), so a parallel `kubectl
/// debug node` can confirm the rules are live.
fn chaos_script(target_ip: &str, dur_secs: u64) -> String {
    // Single-quote-safe: target_ip is a podIP (digits + dots), CHAIN
    // is a const literal, dur_secs is a u64. No injection surface.
    format!(
        r#"set -eu
cleanup() {{
  nsenter -t 1 -m -n -- sh -c '
    iptables -F {CHAIN} 2>/dev/null
    iptables -D FORWARD -j {CHAIN} 2>/dev/null
    iptables -X {CHAIN} 2>/dev/null
    echo CLEANED > /tmp/rio-chaos-state
  '
}}
trap cleanup TERM EXIT
nsenter -t 1 -m -n -- sh -c '
  iptables -N {CHAIN} 2>/dev/null || iptables -F {CHAIN}
  iptables -C FORWARD -j {CHAIN} 2>/dev/null || iptables -I FORWARD -j {CHAIN}
  iptables -A {CHAIN} -s {target_ip} -j DROP
  iptables -A {CHAIN} -d {target_ip} -j DROP
  echo ACTIVE > /tmp/rio-chaos-state
'
echo "blackhole active: {target_ip} via chain {CHAIN}"
sleep {dur_secs}
echo "duration elapsed, exiting (trap will clean)"
"#
    )
}

/// One-shot remediation script — runs without the trap dance (it's
/// the cleanup, not the chaos). Idempotent: every step `2>/dev/null`s
/// so a missing chain is a no-op.
fn remediation_script() -> String {
    format!(
        r#"nsenter -t 1 -m -n -- sh -c '
  iptables -F {CHAIN} 2>/dev/null
  iptables -D FORWARD -j {CHAIN} 2>/dev/null
  iptables -X {CHAIN} 2>/dev/null
  rm -f /tmp/rio-chaos-state
  echo REMEDIATED
'
"#
    )
}

/// Build the Pod spec. Privileged + hostNetwork + hostPID, pinned to
/// `node`. Tolerates both `rio.build/builder` and `rio.build/fetcher`
/// taints — worker nodes carry one or the other depending on which
/// pool Karpenter provisioned them for.
///
/// `restartPolicy: Never` — the pod runs once. If it crashes mid-run
/// (it won't; it's `sleep`), restart wouldn't help anyway: the chain
/// is already in place, restart would re-insert it (which `-C ... ||
/// -I` makes idempotent), but the `sleep` timer would reset. Never is
/// the honest contract.
fn chaos_pod_spec(name: &str, node: &str, script: &str) -> Pod {
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": CHAOS_NS,
            "labels": {
                "app.kubernetes.io/name": "rio-chaos",
                "app.kubernetes.io/part-of": "rio-build",
                "app.kubernetes.io/managed-by": "xtask",
            },
        },
        "spec": {
            "nodeName": node,
            "hostNetwork": true,
            "hostPID": true,
            "restartPolicy": "Never",
            "tolerations": [
                {"key": "rio.build/builder", "operator": "Exists", "effect": "NoSchedule"},
                {"key": "rio.build/fetcher", "operator": "Exists", "effect": "NoSchedule"},
            ],
            "containers": [{
                "name": "chaos",
                "image": BUSYBOX_IMAGE,
                "command": ["sh", "-c"],
                "args": [script],
                "securityContext": {
                    "privileged": true,
                },
            }],
        },
    }))
    // The json! literal is fixed-shape; deserialization can't fail
    // unless the schema is wrong, which is a compile-time bug class.
    .expect("chaos pod spec is well-formed JSON")
}

// ─── run ────────────────────────────────────────────────────────────

#[allow(clippy::print_stderr)] // summary block, no progress bars active
pub async fn run(
    session_dir: &Path,
    kind: ChaosKind,
    target: ChaosTarget,
    from: ChaosFrom,
    duration: Duration,
) -> Result<()> {
    // Only blackhole is implemented; clap's ValueEnum already gates
    // this, but be explicit for when more variants land.
    let ChaosKind::Blackhole = kind;

    let client = k::client().await?;

    info!("resolving --target {target} ...");
    let target_ip = resolve_target_ip(&client, &target).await?;
    info!("target {target} = {target_ip}");

    info!("resolving --from {from} ...");
    let nodes = resolve_from_nodes(&client, &from).await?;
    for (desc, node) in &nodes {
        info!("from: {desc} on node {node}");
    }

    // Unique pod-name suffix per session (unix-ts dirname). One chaos
    // pod per node — name is `rio-chaos-<ts>-<idx>`.
    let session_ts = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("0");

    let dur_secs = duration.as_secs();
    let script = chaos_script(&target_ip, dur_secs);
    let pods: Api<Pod> = Api::namespaced(client.clone(), CHAOS_NS);

    // Track chaos pod identities BEFORE creating them. Same discipline
    // as `stress run`: if xtask dies between create and the chaos.json
    // write, cleanup can't find the pod. So write first (with names we
    // pre-compute), then create. If create fails, cleanup tries to
    // delete a nonexistent pod — that's a 404, harmless.
    let mut state = ChaosState::default();
    for (idx, (_desc, node)) in nodes.iter().enumerate() {
        let pod_name = format!("rio-chaos-{session_ts}-{idx}");
        state.entries.push(ChaosEntry {
            node: node.clone(),
            pod_name,
            target_ip: target_ip.clone(),
            chain: CHAIN.to_string(),
        });
    }
    write_chaos(session_dir, &state)?;
    info!("wrote {} chaos entries to chaos.json", state.entries.len());

    // Now create the pods. If any create fails, we still try to clean
    // up the ones that succeeded (the `?` would skip that, so collect
    // results instead).
    let mut created = vec![];
    for entry in &state.entries {
        let spec = chaos_pod_spec(&entry.pod_name, &entry.node, &script);
        match pods.create(&PostParams::default(), &spec).await {
            Ok(_) => {
                info!("created chaos pod {} on {}", entry.pod_name, entry.node);
                created.push(entry.pod_name.clone());
            }
            Err(e) => {
                warn!("create {} failed: {e:#}", entry.pod_name);
                // Don't bail yet — clean up what we did create.
            }
        }
    }
    if created.len() < state.entries.len() {
        warn!("partial create: cleaning up {} pods", created.len());
        for name in &created {
            let _ = pods.delete(name, &DeleteParams::default()).await;
        }
        bail!(
            "failed to create all chaos pods ({}/{} succeeded)",
            created.len(),
            state.entries.len()
        );
    }

    // Wait for each pod to actually log ACTIVE. The pod's `echo
    // ACTIVE` runs after the iptables rules are in. We poll phase ==
    // Running as a proxy — the script reaches `sleep` immediately
    // after ACTIVE, so Running = rules in place.
    for entry in &state.entries {
        let p = pods.clone();
        let name = entry.pod_name.clone();
        crate::ui::poll(
            &format!("chaos pod {name} active"),
            Duration::from_secs(1),
            30,
            move || {
                let p = p.clone();
                let name = name.clone();
                async move {
                    let pod = p.get(&name).await?;
                    let phase = pod
                        .status
                        .as_ref()
                        .and_then(|s| s.phase.as_deref())
                        .unwrap_or("");
                    Ok((phase == "Running").then_some(()))
                }
            },
        )
        .await?;
    }

    eprintln!();
    eprintln!(
        "{} blackhole active: {} → {} ({} node(s), {} chain)",
        style("⏺").red().bold(),
        style(&from.to_string()).cyan(),
        style(&target_ip).bold(),
        nodes.len(),
        CHAIN,
    );
    eprintln!(
        "  holding for {} ... (Ctrl-C to lift early)",
        style(format!("{dur_secs}s")).yellow()
    );
    eprintln!();

    // Block. Either the timer fires (normal completion) or Ctrl-C
    // (early lift). Both paths fall through to cleanup below.
    let lift_reason = tokio::select! {
        _ = tokio::time::sleep(duration) => "duration elapsed",
        _ = tokio::signal::ctrl_c() => "Ctrl-C",
    };
    info!("lifting blackhole ({lift_reason})");

    // Delete the chaos pods. Their TERM trap fires → iptables cleaned.
    // We DON'T spawn a remediation pod here — the trap is reliable for
    // graceful delete. Remediation is for the SIGKILL-recovery path
    // (`stress cleanup`).
    let mut all_clean = true;
    for entry in &state.entries {
        match pods.delete(&entry.pod_name, &DeleteParams::default()).await {
            Ok(_) => info!("deleted {} (trap will flush {CHAIN})", entry.pod_name),
            Err(e) => {
                warn!("delete {} failed: {e:#}", entry.pod_name);
                all_clean = false;
            }
        }
    }

    // Poll for actual deletion — kubelet needs a moment to send TERM,
    // run the trap, and reap. We don't want to claim "lifted" while
    // the rules are still in place.
    for entry in &state.entries {
        let p = pods.clone();
        let name = entry.pod_name.clone();
        let res = crate::ui::poll(
            &format!("chaos pod {name} gone"),
            Duration::from_secs(1),
            30,
            move || {
                let p = p.clone();
                let name = name.clone();
                async move { Ok(p.get_opt(&name).await?.is_none().then_some(())) }
            },
        )
        .await;
        if let Err(e) = res {
            warn!("pod {} delete didn't complete: {e:#}", entry.pod_name);
            all_clean = false;
        }
    }

    if all_clean {
        // Clear chaos.json — nothing left to remediate.
        write_chaos(session_dir, &ChaosState::default())?;
        eprintln!();
        eprintln!(
            "{} blackhole lifted, chain {CHAIN} flushed",
            style("✓").green()
        );
    } else {
        eprintln!();
        eprintln!(
            "{} cleanup incomplete — run `stress cleanup` to remediate via one-shot pod",
            style("!").yellow()
        );
    }

    Ok(())
}

// ─── cleanup integration ────────────────────────────────────────────

/// Remediate any chaos entries in `<session>/chaos.json`. Called from
/// `stress cleanup`. For each entry: delete the chaos pod (404 is
/// fine), then spawn a one-shot remediation pod on its node that
/// flushes the chain. Idempotent — if the chain doesn't exist (the
/// chaos pod's trap already cleaned), the remediation script is a
/// no-op.
///
/// Returns `(remediated_count, had_entries)`. `had_entries` lets the
/// caller decide whether to print a chaos-cleanup summary line.
#[allow(clippy::print_stderr)]
pub async fn remediate(session_dir: &Path) -> Result<(usize, bool)> {
    let state = read_chaos(session_dir)?;
    if state.entries.is_empty() {
        return Ok((0, false));
    }

    let client = k::client().await?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), CHAOS_NS);

    let session_ts = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("0");
    let script = remediation_script();
    let mut remediated = 0;

    for (idx, entry) in state.entries.iter().enumerate() {
        // Delete the original chaos pod first (if it's still there).
        // Best-effort — 404 means it already exited or was deleted.
        if let Err(e) = pods.delete(&entry.pod_name, &DeleteParams::default()).await
            && !is_not_found(&e)
        {
            warn!("delete chaos pod {}: {e:#}", entry.pod_name);
        }

        // One-shot remediation pod. Same spec shape, different script.
        let rem_name = format!("rio-chaos-remediate-{session_ts}-{idx}");
        let spec = chaos_pod_spec(&rem_name, &entry.node, &script);
        match pods.create(&PostParams::default(), &spec).await {
            Ok(_) => {
                eprintln!(
                    "    {} remediation pod {} on {} (flush {})",
                    style("▸").cyan(),
                    rem_name,
                    entry.node,
                    entry.chain,
                );
                // Wait for it to complete (Succeeded), then delete it.
                // 30s is generous for `iptables -F; -D; -X`.
                let p = pods.clone();
                let n = rem_name.clone();
                let done = crate::ui::poll(
                    &format!("remediation {rem_name} done"),
                    Duration::from_secs(1),
                    30,
                    move || {
                        let p = p.clone();
                        let n = n.clone();
                        async move {
                            let phase = p
                                .get(&n)
                                .await?
                                .status
                                .and_then(|s| s.phase)
                                .unwrap_or_default();
                            // Succeeded = script exited 0. Failed =
                            // nonzero (still means chain is gone or
                            // never existed; the script `2>/dev/null`s
                            // everything). Either is "done".
                            Ok((phase == "Succeeded" || phase == "Failed").then_some(()))
                        }
                    },
                )
                .await;
                if done.is_ok() {
                    remediated += 1;
                }
                let _ = pods.delete(&rem_name, &DeleteParams::default()).await;
            }
            Err(e) => warn!("create remediation pod {rem_name}: {e:#}"),
        }
    }

    // Clear chaos.json — we've done what we can.
    write_chaos(session_dir, &ChaosState::default())?;
    Ok((remediated, true))
}

fn is_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(r) if r.code == 404)
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parse() {
        assert_eq!(
            "scheduler-leader".parse::<ChaosTarget>().unwrap(),
            ChaosTarget::SchedulerLeader
        );
        assert_eq!("store".parse::<ChaosTarget>().unwrap(), ChaosTarget::Store);
        assert_eq!(
            "builder-0".parse::<ChaosTarget>().unwrap(),
            ChaosTarget::Builder(0)
        );
        assert_eq!(
            "fetcher-7".parse::<ChaosTarget>().unwrap(),
            ChaosTarget::Fetcher(7)
        );
        assert!("scheduler".parse::<ChaosTarget>().is_err());
        assert!("builder".parse::<ChaosTarget>().is_err());
        assert!("builder-".parse::<ChaosTarget>().is_err());
        assert!("builder-x".parse::<ChaosTarget>().is_err());
    }

    #[test]
    fn target_display_roundtrip() {
        for s in ["scheduler-leader", "store", "builder-3", "fetcher-0"] {
            let t: ChaosTarget = s.parse().unwrap();
            assert_eq!(t.to_string(), s);
        }
    }

    #[test]
    fn from_parse() {
        assert_eq!(
            "all-workers".parse::<ChaosFrom>().unwrap(),
            ChaosFrom::AllWorkers
        );
        assert_eq!(
            "fetcher-0".parse::<ChaosFrom>().unwrap(),
            ChaosFrom::Fetcher(0)
        );
        assert_eq!(
            "builder-2".parse::<ChaosFrom>().unwrap(),
            ChaosFrom::Builder(2)
        );
        // scheduler-leader is a valid TARGET but not a valid FROM —
        // you blackhole the scheduler FROM a worker, not the reverse.
        assert!("scheduler-leader".parse::<ChaosFrom>().is_err());
        assert!("all".parse::<ChaosFrom>().is_err());
    }

    #[test]
    fn duration_parse() {
        assert_eq!(parse_duration_secs("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration_secs("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration_secs("0s").unwrap(), Duration::from_secs(0));
        assert!(parse_duration_secs("60m").is_err());
        assert!(parse_duration_secs("").is_err());
        assert!(parse_duration_secs("abc").is_err());
    }

    #[test]
    fn chaos_state_roundtrip_via_disk() {
        // Same disk-roundtrip discipline as stress::pids_roundtrip.
        let dir = tempfile::tempdir().unwrap();
        let st = ChaosState {
            entries: vec![
                ChaosEntry {
                    node: "ip-10-42-1-219.us-east-2.compute.internal".into(),
                    pod_name: "rio-chaos-1700000000-0".into(),
                    target_ip: "10.42.1.99".into(),
                    chain: "RIO-CHAOS".into(),
                },
                ChaosEntry {
                    node: "ip-10-42-2-88.us-east-2.compute.internal".into(),
                    pod_name: "rio-chaos-1700000000-1".into(),
                    target_ip: "10.42.1.99".into(),
                    chain: "RIO-CHAOS".into(),
                },
            ],
        };
        write_chaos(dir.path(), &st).unwrap();
        assert!(!dir.path().join("chaos.json.tmp").exists());
        let r = read_chaos(dir.path()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].node, st.entries[0].node);
        assert_eq!(r.entries[1].pod_name, "rio-chaos-1700000000-1");
        assert_eq!(r.entries[0].chain, CHAIN);
    }

    #[test]
    fn read_chaos_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let st = read_chaos(dir.path()).unwrap();
        assert!(st.entries.is_empty());
    }

    #[test]
    fn chaos_script_shape() {
        let s = chaos_script("10.42.1.99", 60);
        // Chain create/flush idempotency.
        assert!(s.contains("iptables -N RIO-CHAOS 2>/dev/null || iptables -F RIO-CHAOS"));
        // Link-if-not-linked.
        assert!(s.contains("iptables -C FORWARD -j RIO-CHAOS 2>/dev/null || iptables -I FORWARD"));
        // Both directions dropped.
        assert!(s.contains("-s 10.42.1.99 -j DROP"));
        assert!(s.contains("-d 10.42.1.99 -j DROP"));
        // Trap cleanup.
        assert!(s.contains("trap cleanup TERM EXIT"));
        assert!(s.contains("iptables -X RIO-CHAOS"));
        // Duration interpolated.
        assert!(s.contains("sleep 60"));
        // nsenter into host.
        assert!(s.contains("nsenter -t 1 -m -n"));
    }

    #[test]
    fn remediation_script_idempotent() {
        let s = remediation_script();
        // Every iptables call must be 2>/dev/null'd — missing chain
        // is the expected case (chaos pod's trap already cleaned).
        assert!(s.contains("iptables -F RIO-CHAOS 2>/dev/null"));
        assert!(s.contains("iptables -D FORWARD -j RIO-CHAOS 2>/dev/null"));
        assert!(s.contains("iptables -X RIO-CHAOS 2>/dev/null"));
        // No trap, no sleep — one-shot.
        assert!(!s.contains("trap"));
        assert!(!s.contains("sleep"));
    }

    #[test]
    fn chaos_pod_spec_shape() {
        let pod = chaos_pod_spec("rio-chaos-test", "ip-10-42-1-1", "echo hi");
        let spec = pod.spec.unwrap();
        assert_eq!(spec.node_name.as_deref(), Some("ip-10-42-1-1"));
        assert_eq!(spec.host_network, Some(true));
        assert_eq!(spec.host_pid, Some(true));
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        // Tolerates both worker taint keys.
        let tol: Vec<_> = spec
            .tolerations
            .unwrap()
            .into_iter()
            .filter_map(|t| t.key)
            .collect();
        assert!(tol.contains(&"rio.build/builder".to_string()));
        assert!(tol.contains(&"rio.build/fetcher".to_string()));
        // Privileged container.
        let c = &spec.containers[0];
        assert_eq!(c.security_context.as_ref().unwrap().privileged, Some(true));
        assert_eq!(c.image.as_deref(), Some(BUSYBOX_IMAGE));
        // Namespace + labels.
        let meta = pod.metadata;
        assert_eq!(meta.namespace.as_deref(), Some(CHAOS_NS));
        assert_eq!(
            meta.labels.unwrap().get("app.kubernetes.io/name"),
            Some(&"rio-chaos".to_string())
        );
    }
}
