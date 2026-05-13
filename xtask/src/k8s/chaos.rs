//! `xtask k8s qa --fault` — structured network fault injection.
//!
//! **Why this exists (I-048c).** The h2 keepalive path on the balanced
//! channel only fires when the peer goes blackhole — packets dropped,
//! no FIN, no RST. Process death (even SIGKILL) doesn't test it: the
//! kernel reaps, closes FDs, sends FIN, the worker sees a clean close
//! in sub-ms. `kubectl delete pod --grace-period=0` still SIGTERMs
//! first.
//!
//! **Why not iptables (the I-048c fix to the I-048c verifier).** The
//! original implementation spawned a privileged hostNetwork pod on
//! each worker node and inserted `ip6tables -I FORWARD -j RIO-CHAOS`
//! DROP rules matching the target pod IP. That worked pre-Cilium
//! (kube-proxy + AWS VPC CNI; pod-to-pod traffic transited the host
//! IP layer). Under Cilium with `bpf.masquerade=true` and a kernel
//! ≥5.10, `bpf.hostLegacyRouting` defaults to **false**: pod-to-pod
//! cross-node traffic is `bpf_redirect()`ed at the lxc TC ingress
//! straight to `cilium_geneve`/`cilium_wg0`, never entering the kernel
//! IP routing layer. Netfilter (FORWARD, raw/PREROUTING, mangle) and
//! legacy `tc filter`s (Cilium 1.16+ attaches via tcx, which runs
//! *before* legacy filters and short-circuits on redirect) never see
//! those packets. Verified live 2026-05-13: `ip6tables -L FORWARD -v`
//! on every node shows `0 packets, 0 bytes`. The k3s VM tests pin
//! `bpf.hostLegacyRouting=true` for an unrelated socketLB→apiserver
//! reason (`nix/cilium-render.nix`), so this datapath gap is
//! EKS-only — invisible to CI.
//!
//! **What works: a CiliumClusterwideNetworkPolicy `egressDeny`.**
//! Apply a deny rule at the same eBPF policy map the traffic actually
//! traverses. Verified against the running 1.19.3 source (`bpf_lxc.c`):
//! `policy_can_egress*()` is called for *both* `CT_NEW` and
//! `CT_ESTABLISHED`, so an established gRPC stream's packets are
//! dropped per-packet the moment the deny lands. L3/L4 deny is silent
//! drop (no RST, no ICMP) — the exact blackhole semantics the keepalive
//! check needs. Live spike 2026-05-13: builder hit
//! `BrokenPipe … stream closed because of a broken pipe` (the
//! h2-keepalive teardown signature, identical to the 2026-04-01
//! pre-Cilium iptables verification) and
//! `rio_scheduler_worker_disconnects_total` incremented in ~30s.
//!
//! **Self-cleaning.** The CCNP is a single cluster-scoped k8s object —
//! `delete ccnp rio-chaos-blackhole` is idempotent. No remediation
//! pod, no chain flush. SIGKILL recovery is a delete-by-fixed-name on
//! the next invocation, gated on `chaos.json` so it doesn't pay an API
//! round-trip when there's nothing to clean.
//!
//! **Precondition: the `from` endpoints must already be policy-enforced.**
//! Cilium switches an endpoint to default-deny in the deny direction
//! the moment *any* policy (including a deny-only one) selects it.
//! Builders/fetchers carry that policy from `builder-egress` /
//! `fetcher-egress` (`infra/helm/rio-build/templates/networkpolicy.
//! yaml`). If `networkPolicy.enabled=false`, applying a deny-only CCNP
//! would airgap the workers entirely. [`run`] asserts the required
//! CCNPs exist and bails with a fix-naming error otherwise.

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use console::style;
use kube::api::{Api, DeleteParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use rio_crds::KubeErrorExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::k8s::client as k;
use crate::k8s::{NS, NS_STORE};

/// Fixed name. ONE blackhole at a time — chaos compositions (e.g.
/// scheduler + store simultaneously) are a future kind, and a fixed
/// name keeps SIGKILL remediation a name-lookup, not a label-scan.
const CCNP_NAME: &str = "rio-chaos-blackhole";

/// CiliumClusterwideNetworkPolicy GVK. Cluster-scoped (not the
/// namespaced `CiliumNetworkPolicy`) — `ChaosFrom::AllWorkers` spans
/// `rio-builders` AND `rio-fetchers`, and the existing `builder-egress`
/// / `fetcher-egress` policies are already CCNPs, so this stays in the
/// same shape an operator already knows to look for.
fn ccnp_gvk() -> GroupVersionKind {
    GroupVersionKind::gvk("cilium.io", "v2", "CiliumClusterwideNetworkPolicy")
}

fn ccnp_api(client: &k::Client) -> Api<DynamicObject> {
    Api::all_with(client.clone(), &ApiResource::from_gvk(&ccnp_gvk()))
}

// ─── CLI types ──────────────────────────────────────────────────────

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChaosKind {
    /// CCNP `egressDeny` from the `--from` workers to the `--target`.
    /// Silent drop at the eBPF policy map — no FIN, no RST; the only
    /// signal is keepalive timeout. The faithful Cilium analog of an
    /// iptables blackhole.
    Blackhole,
    // Future: Latency / Flap need `tc netem` and would re-introduce a
    // hostNetwork chaos pod (CNP can't shape traffic). Only Blackhole
    // is wired.
}

/// What to deny egress to. Resolved to a `toEndpoints` label
/// selector — NOT a pod IP. CIDR-based deny rules don't match
/// in-cluster identities (Cilium classifies pod IPs by *identity*, not
/// CIDR; the `toCIDR` deny rule's CIDR-identity never overlaps a pod's
/// pod-identity), and Deployment pods carry no leader-distinguishing
/// label, so `Scheduler` denies *all* `rio-scheduler` pods. Functionally
/// identical to leader-only for the i048c assertion: workers only hold
/// a stream to the leader; the standby has no stream to drop.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChaosTarget {
    Scheduler,
    Store,
    Builder,
    Fetcher,
}

impl fmt::Display for ChaosTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.to_possible_value().unwrap().get_name())
    }
}

/// Which workers lose connectivity. Becomes the CCNP `endpointSelector`.
/// Pod-scoped (not node-scoped like the iptables predecessor): the
/// deny lands at the worker pod's eBPF egress, not the node's FORWARD
/// chain. Same observable effect for the keepalive test; a hair more
/// surgical (a non-rio pod on the same node keeps its scheduler
/// connectivity).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChaosFrom {
    /// Every builder AND fetcher endpoint.
    AllWorkers,
    Builder,
    Fetcher,
}

impl fmt::Display for ChaosFrom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.to_possible_value().unwrap().get_name())
    }
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

/// SIGKILL-recovery marker. Written BEFORE the CCNP is created (if
/// xtask dies between create and write, [`remediate`] can't find it —
/// so write first; a leftover marker pointing at a CCNP that never got
/// created is a harmless 404 on cleanup). Cleared after the CCNP is
/// deleted.
#[derive(Serialize, Deserialize, Default)]
pub struct ChaosState {
    /// Name of the CCNP that may still be applied.
    pub ccnp: Option<String>,
}

/// Same write-then-rename atomicity as `stress::write_pids`.
pub fn write_chaos(dir: &Path, st: &ChaosState) -> Result<()> {
    let tmp = dir.join("chaos.json.tmp");
    let dst = dir.join("chaos.json");
    fs::write(&tmp, serde_json::to_string_pretty(st)?)?;
    fs::rename(tmp, dst)?;
    Ok(())
}

/// Read the marker. A missing file OR an unparseable one (e.g. the
/// pre-CNP `{"entries":[…]}` schema from a SIGKILLed run before this
/// migration) is `default()` — there's nothing the new code could
/// remediate from old state anyway (the iptables chaos pods self-clean
/// via their EXIT trap), and a hard error here would block the next QA
/// run indefinitely.
pub fn read_chaos(dir: &Path) -> Result<ChaosState> {
    let p = dir.join("chaos.json");
    if !p.exists() {
        return Ok(ChaosState::default());
    }
    let raw = fs::read_to_string(&p)?;
    Ok(serde_json::from_str(&raw).unwrap_or_else(|e| {
        warn!("chaos.json unparseable ({e}); treating as clean");
        ChaosState::default()
    }))
}

// ─── CCNP construction ──────────────────────────────────────────────

/// Helm-managed CCNPs that must exist for a deny-only chaos CCNP to be
/// safe on the `from` endpoints. Cilium docs: "Pods will enter
/// default-deny mode as soon a single policy selects it" — that's
/// *any* policy including a deny-only one. If `networkPolicy.enabled=
/// false`, applying [`chaos_ccnp`] would be the FIRST policy on the
/// workers and would airgap their entire egress (DNS, store, *and*
/// scheduler) instead of just the chaos target.
fn precondition_ccnps(from: &ChaosFrom) -> &'static [&'static str] {
    match from {
        ChaosFrom::AllWorkers => &["builder-egress", "fetcher-egress"],
        ChaosFrom::Builder => &["builder-egress"],
        ChaosFrom::Fetcher => &["fetcher-egress"],
    }
}

/// `endpointSelector` for the `from` workers.
///
/// Selectors mirror the helm `networkpolicy.yaml` shapes so an
/// operator reading `kubectl get ccnp -o yaml` next to the helm-owned
/// policies sees the same vocabulary. The component label is stamped
/// by the controller's `pool/pod.rs` `executor_labels` regardless of
/// which namespace a Pool lands in — same reason the helm CCNPs select
/// on it instead of namespace.
fn from_endpoint_selector(from: &ChaosFrom) -> Value {
    match from {
        ChaosFrom::AllWorkers => json!({
            "matchExpressions": [{
                "key": "app.kubernetes.io/component",
                "operator": "In",
                "values": ["rio-builder", "rio-fetcher"],
            }]
        }),
        ChaosFrom::Builder => {
            json!({"matchLabels": {"app.kubernetes.io/component": "rio-builder"}})
        }
        ChaosFrom::Fetcher => {
            json!({"matchLabels": {"app.kubernetes.io/component": "rio-fetcher"}})
        }
    }
}

/// `egressDeny.toEndpoints` selector for the chaos `target`.
///
/// Cross-namespace `toEndpoints` need the `k8s:` prefix (Cilium's
/// auto-injected source labels — `endpointSelector` doesn't, but
/// `toEndpoints`/`fromEndpoints` do). Scheduler/store match by
/// `name`+`namespace` (cluster-singleton Deployments); builder/fetcher
/// by `component` only (namespace-agnostic, same rationale as
/// [`from_endpoint_selector`]).
fn target_endpoints(target: &ChaosTarget) -> Value {
    match target {
        ChaosTarget::Scheduler => json!([{
            "matchLabels": {
                "k8s:io.kubernetes.pod.namespace": NS,
                "k8s:app.kubernetes.io/name": "rio-scheduler",
            }
        }]),
        ChaosTarget::Store => json!([{
            "matchLabels": {
                "k8s:io.kubernetes.pod.namespace": NS_STORE,
                "k8s:app.kubernetes.io/name": "rio-store",
            }
        }]),
        ChaosTarget::Builder => json!([{
            "matchLabels": {"k8s:app.kubernetes.io/component": "rio-builder"}
        }]),
        ChaosTarget::Fetcher => json!([{
            "matchLabels": {"k8s:app.kubernetes.io/component": "rio-fetcher"}
        }]),
    }
}

/// Build the CCNP. `egressDeny` only — no `egress`, no `ingress*`.
/// Deny precedence over allow is what makes this composable with the
/// helm-managed `builder-egress`/`fetcher-egress` policies (which
/// allow scheduler:9001 and store:9002): the deny shadows the allow
/// for the duration, the allow is untouched. Adding redundant allow
/// rules here would be a foot-gun if someone copies this CCNP as a
/// template — they'd inherit a `world` carve-out they didn't intend.
fn chaos_ccnp(target: &ChaosTarget, from: &ChaosFrom) -> DynamicObject {
    serde_json::from_value(json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumClusterwideNetworkPolicy",
        "metadata": {
            "name": CCNP_NAME,
            "labels": {
                "app.kubernetes.io/name": "rio-chaos",
                "app.kubernetes.io/part-of": "rio-build",
                "app.kubernetes.io/managed-by": "xtask",
            },
        },
        "spec": {
            "endpointSelector": from_endpoint_selector(from),
            "egressDeny": [{"toEndpoints": target_endpoints(target)}],
        },
    }))
    // Fixed-shape literal — failure is a compile-time bug class.
    .expect("chaos CCNP is well-formed JSON")
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
    let api = ccnp_api(&client);

    // Precondition: workers must already be policy-enforced (see the
    // module doc). Bail with the fix, not just the symptom.
    for required in precondition_ccnps(&from) {
        if api.get_opt(required).await?.is_none() {
            bail!(
                "chaos blackhole requires CCNP {required:?} (from \
                 networkPolicy.enabled=true) so the deny rule layers \
                 onto an already-policed endpoint instead of flipping \
                 it to default-deny. Enable networkPolicy in the helm \
                 values and `xtask k8s up --helm`."
            );
        }
    }

    // SIGKILL discipline: marker on disk BEFORE the CCNP exists.
    write_chaos(
        session_dir,
        &ChaosState {
            ccnp: Some(CCNP_NAME.to_string()),
        },
    )?;

    let ccnp = chaos_ccnp(&target, &from);
    info!("applying CCNP {CCNP_NAME} ({from} → egressDeny → {target})");
    api.create(&PostParams::default(), &ccnp).await?;

    // Wait for cilium-operator validation. `Valid=True` does NOT mean
    // every cilium-agent has regenerated its endpoint programs — that
    // propagation is sub-second in practice (one CCNP, a handful of
    // endpoints) but isn't observable from the k8s API without
    // per-node polling. The spike showed first-heartbeat-fail at ~15s;
    // i048c budgets 30s of startup slack on top of the 45s keepalive
    // window. A short fixed grace after Valid is plenty.
    crate::ui::poll(
        &format!("CCNP {CCNP_NAME} valid"),
        Duration::from_secs(1),
        30,
        || {
            let api = api.clone();
            async move {
                let cnp = api.get(CCNP_NAME).await?;
                Ok(ccnp_is_valid(&cnp).then_some(()))
            }
        },
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    eprintln!();
    eprintln!(
        "{} blackhole active: {} {} {} (CCNP {CCNP_NAME})",
        style("⏺").red().bold(),
        style(&from.to_string()).cyan(),
        style("→ egressDeny →").dim(),
        style(&target.to_string()).bold(),
    );
    eprintln!(
        "  holding for {} ... (Ctrl-C to lift early)",
        style(format!("{}s", duration.as_secs())).yellow()
    );
    eprintln!();

    // Block. Either the timer fires or Ctrl-C. Both fall through to
    // cleanup. `ctrl_c()` lazy-install is acceptable HERE: the only
    // cluster-side resource is the CCNP, and the next-invocation
    // [`remediate`] sweep already handles a SIGKILL leak. Contrast
    // stress.rs/status.rs where the leaked resource is a local
    // process holding a port.
    let lift_reason = tokio::select! {
        _ = tokio::time::sleep(duration) => "duration elapsed",
        _ = tokio::signal::ctrl_c() => "Ctrl-C",
    };
    info!("lifting blackhole ({lift_reason})");

    // Idempotent: 404 = already gone (operator cleaned manually).
    if let Err(e) = api.delete(CCNP_NAME, &DeleteParams::default()).await
        && !e.is_not_found()
    {
        // Don't `?` — leave the marker on disk so the next invocation
        // remediates.
        warn!("delete CCNP {CCNP_NAME}: {e:#} — left chaos.json marker");
        eprintln!();
        eprintln!(
            "{} cleanup incomplete — next `qa --fault` will retry the delete",
            style("!").yellow()
        );
        return Ok(());
    }

    write_chaos(session_dir, &ChaosState::default())?;
    eprintln!();
    eprintln!(
        "{} blackhole lifted, CCNP {CCNP_NAME} deleted",
        style("✓").green()
    );
    Ok(())
}

/// `.status.conditions[type=Valid].status == "True"` on a CCNP
/// `DynamicObject`. Cilium-operator sets this once the policy parses
/// and the selectors resolve.
fn ccnp_is_valid(cnp: &DynamicObject) -> bool {
    cnp.data["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["type"] == "Valid" && c["status"] == "True")
}

// ─── cleanup integration ────────────────────────────────────────────

/// Remediate a leftover chaos CCNP. Called at the start of `qa
/// --fault` and `i048c`. Reads `chaos.json` so a clean state is a
/// disk-read, not an API round-trip; if the marker says a CCNP was
/// applied, delete it (404 = already gone, count as remediated).
///
/// Returns `(remediated_count, had_marker)`. `had_marker` lets the
/// caller decide whether to print a chaos-cleanup summary line.
pub async fn remediate(session_dir: &Path) -> Result<(usize, bool)> {
    let state = read_chaos(session_dir)?;
    let Some(name) = state.ccnp else {
        return Ok((0, false));
    };

    let client = k::client().await?;
    let api = ccnp_api(&client);
    info!("remediating leftover CCNP {name}");
    let remediated = match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => 1,
        Err(e) if e.is_not_found() => {
            // chaos pod from a pre-SIGKILL run already self-cleaned, or
            // an operator deleted it manually. Either way: clean.
            0
        }
        Err(e) => {
            warn!("delete CCNP {name}: {e:#}");
            // Leave the marker so the *next* invocation retries.
            return Ok((0, true));
        }
    };

    write_chaos(session_dir, &ChaosState::default())?;
    Ok((remediated, true))
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chaos_enums_roundtrip() {
        // ValueEnum derive gives kebab-case parse/display; spot-check the
        // shape and that target/from variant sets stay distinct.
        for v in ChaosTarget::value_variants() {
            assert_eq!(ChaosTarget::from_str(&v.to_string(), false).unwrap(), *v);
        }
        for v in ChaosFrom::value_variants() {
            assert_eq!(ChaosFrom::from_str(&v.to_string(), false).unwrap(), *v);
        }
        // RENAMED in the CNP migration: label-selector targeting hits
        // every `rio-scheduler` pod, not just the lease-holder. The old
        // `scheduler-leader` name was a lie under the new mechanism —
        // make it a CLI parse error so a stale invocation surfaces
        // loudly instead of silently doing the wrong thing.
        assert_eq!(ChaosTarget::Scheduler.to_string(), "scheduler");
        assert!(ChaosTarget::from_str("scheduler-leader", false).is_err());
        assert_eq!(ChaosFrom::AllWorkers.to_string(), "all-workers");
        // scheduler is a valid TARGET but not a valid FROM.
        assert!(ChaosFrom::from_str("scheduler", false).is_err());
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
        let dir = tempfile::tempdir().unwrap();
        let st = ChaosState {
            ccnp: Some(CCNP_NAME.into()),
        };
        write_chaos(dir.path(), &st).unwrap();
        // Atomic write — tmp file gone after rename.
        assert!(!dir.path().join("chaos.json.tmp").exists());
        let r = read_chaos(dir.path()).unwrap();
        assert_eq!(r.ccnp.as_deref(), Some(CCNP_NAME));
    }

    #[test]
    fn read_chaos_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_chaos(dir.path()).unwrap().ccnp.is_none());
    }

    #[test]
    fn read_chaos_old_format_is_default() {
        // SIGKILL recovery across the chaos-pod → CNP migration: an
        // old-format `{"entries":[…]}` chaos.json (from a run killed
        // before this change shipped) must not block the next QA run.
        // The old chaos pods self-cleaned via their EXIT trap; there's
        // nothing the new code could remediate from old state.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("chaos.json"),
            r#"{"entries":[{"node":"n","pod_name":"p","target_ip":"i","chain":"c"}]}"#,
        )
        .unwrap();
        assert!(read_chaos(dir.path()).unwrap().ccnp.is_none());
    }

    #[test]
    fn precondition_ccnps_per_from() {
        // The proxy for "workers are policy-enforced" is "their
        // helm-managed egress CCNP exists." [`run`] bails if it doesn't,
        // because a deny-only CNP on an unpoliced endpoint flips it to
        // default-deny and airgaps the worker entirely.
        assert_eq!(precondition_ccnps(&ChaosFrom::Builder), &["builder-egress"]);
        assert_eq!(precondition_ccnps(&ChaosFrom::Fetcher), &["fetcher-egress"]);
        assert_eq!(
            precondition_ccnps(&ChaosFrom::AllWorkers),
            &["builder-egress", "fetcher-egress"]
        );
    }

    #[test]
    fn chaos_ccnp_scheduler_from_all_workers() {
        // The i048c shape. Cluster-scoped CCNP (workers span two
        // namespaces), `endpointSelector` selects both worker kinds,
        // `egressDeny` (not `egress`) targets `rio-scheduler`.
        let ccnp = chaos_ccnp(&ChaosTarget::Scheduler, &ChaosFrom::AllWorkers);
        let v = serde_json::to_value(&ccnp).unwrap();
        assert_eq!(v["apiVersion"], "cilium.io/v2");
        assert_eq!(v["kind"], "CiliumClusterwideNetworkPolicy");
        assert_eq!(v["metadata"]["name"], CCNP_NAME);
        assert_eq!(
            v["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "xtask"
        );

        let expr = &v["spec"]["endpointSelector"]["matchExpressions"][0];
        assert_eq!(expr["key"], "app.kubernetes.io/component");
        assert_eq!(expr["operator"], "In");
        let vals: Vec<&str> = expr["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(vals, ["rio-builder", "rio-fetcher"]);

        let to = &v["spec"]["egressDeny"][0]["toEndpoints"][0]["matchLabels"];
        assert_eq!(to["k8s:io.kubernetes.pod.namespace"], NS);
        assert_eq!(to["k8s:app.kubernetes.io/name"], "rio-scheduler");

        // Deny-ONLY. `egress` allow rules would be redundant (deny wins)
        // and a foot-gun if this CCNP gets copied as a template; `ingress*`
        // is the wrong direction for `ChaosFrom`'s "block from worker side".
        assert!(v["spec"].get("egress").is_none());
        assert!(v["spec"].get("ingress").is_none());
        assert!(v["spec"].get("ingressDeny").is_none());
    }

    #[test]
    fn chaos_ccnp_store_from_builder() {
        // Single-component `from` uses `matchLabels`, not
        // `matchExpressions` — same shape the helm policies use.
        let ccnp = chaos_ccnp(&ChaosTarget::Store, &ChaosFrom::Builder);
        let v = serde_json::to_value(&ccnp).unwrap();
        assert_eq!(
            v["spec"]["endpointSelector"]["matchLabels"]["app.kubernetes.io/component"],
            "rio-builder"
        );
        assert!(
            v["spec"]["endpointSelector"]
                .get("matchExpressions")
                .is_none()
        );
        let to = &v["spec"]["egressDeny"][0]["toEndpoints"][0]["matchLabels"];
        assert_eq!(to["k8s:io.kubernetes.pod.namespace"], NS_STORE);
        assert_eq!(to["k8s:app.kubernetes.io/name"], "rio-store");
    }

    #[test]
    fn chaos_ccnp_worker_target_no_namespace() {
        // Builder/fetcher targets match by component, not name+namespace —
        // the controller stamps the component label regardless of which
        // namespace a Pool lands in (same rationale as `builder-egress`'s
        // CCNP-not-CNP choice in `networkpolicy.yaml`).
        let ccnp = chaos_ccnp(&ChaosTarget::Builder, &ChaosFrom::Fetcher);
        let v = serde_json::to_value(&ccnp).unwrap();
        let to = &v["spec"]["egressDeny"][0]["toEndpoints"][0]["matchLabels"];
        assert_eq!(to["k8s:app.kubernetes.io/component"], "rio-builder");
        assert!(to.get("k8s:io.kubernetes.pod.namespace").is_none());
    }

    #[test]
    fn ccnp_is_valid_reads_conditions() {
        // The exact shape cilium-operator writes on a validated CCNP.
        let dyn_obj: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cilium.io/v2",
            "kind": "CiliumClusterwideNetworkPolicy",
            "metadata": {"name": "x"},
            "status": {"conditions": [
                {"type": "Valid", "status": "True", "message": "Policy validation succeeded"},
            ]},
        }))
        .unwrap();
        assert!(ccnp_is_valid(&dyn_obj));

        // Not-yet-validated: status absent.
        let pending: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cilium.io/v2",
            "kind": "CiliumClusterwideNetworkPolicy",
            "metadata": {"name": "x"},
        }))
        .unwrap();
        assert!(!ccnp_is_valid(&pending));

        // Validation FAILED — must not return true on `status: "False"`.
        let invalid: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cilium.io/v2",
            "kind": "CiliumClusterwideNetworkPolicy",
            "metadata": {"name": "x"},
            "status": {"conditions": [{"type": "Valid", "status": "False"}]},
        }))
        .unwrap();
        assert!(!ccnp_is_valid(&invalid));
    }
}
