//! `xtask k8s probe-boot` — single-obs `leadTimeSeed` measurement +
//! Karpenter conformance check (ADR-023 §13b prerequisite).
//!
//! Creates one NodeClaim per `sla.hwClasses × {spot,od}` cell, waits for
//! `Registered`, reports boot time, deletes. Asserts:
//!
//! 1. Naked NodeClaim launches (no NodePool ownerRef).
//! 2. Shim NodePool (`limits:{cpu:0}`) skipped — no NodeClaim labelled
//!    `karpenter.sh/nodepool=rio-nodeclaim-shim` AND `Launched=True`
//!    that we didn't create (the shim must never provision).
//! 3. `Registered.lastTransitionTime` populated.
//! 4. Controller-stamped `karpenter.sh/nodepool` label survives to Node.
//! 5. `budgets:nodes:"0"` blocks drift (hold one probe 30s past
//!    Registered, assert no `Disrupting`/`DisruptionReason`).
//!
//! Output: per-cell boot seconds + a YAML block ready for
//! `infra/helm/rio-build/values.yaml` `sla.leadTimeSeed:`.
//!
//! EKS-only, operator-run; NOT a CI test. The shim NodePool is
//! pre-created here (idempotent) so the probe is runnable before B3
//! helm-manages it — without it, Karpenter parks naked NodeClaims at
//! `AwaitingReconciliation` because the `karpenter.sh/nodepool` label
//! references a NodePool that doesn't exist.

use std::collections::BTreeMap;
use std::time::Duration;

use ::kube::api::{Api, DeleteParams, ListParams, PostParams};
use ::kube::core::DynamicObject;
use anyhow::{Context, Result, bail, ensure};
use k8s_openapi::api::core::v1::{ConfigMap, Node};
use rio_scheduler::sla::config::{CapacityType, Cell, HwClassDef, SlaConfig};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::k8s::status::{nodeclaim_api, nodepool_api};
use crate::k8s::{NS, client as kube};
use crate::ui;

/// `karpenter.sh/nodepool` value the controller stamps (ADR-023 §13b
/// `r[ctrl.nodeclaim.shim-nodepool]`). The probe ensures the NodePool
/// object exists (`ensure_shim_nodepool`) before stamping the label —
/// Karpenter refuses to reconcile a NodeClaim whose nodepool label
/// points at a missing NodePool. B3 will helm-manage the same object.
const SHIM_NODEPOOL: &str = "rio-nodeclaim-shim";

/// `metadata.labels` key marking a probe-boot NodeClaim. Lets cleanup
/// (`kubectl delete nodeclaims -l rio.build/probe=true`) be a one-liner
/// and lets assertion 2 exclude our own naked claims.
const PROBE_LABEL: &str = "rio.build/probe";

/// Per-cell registration timeout. AMI boot + kubelet join is ~90s
/// typical; 300s covers spot ICE retry + cold ENI attach.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(300);

/// Assertion 5 hold window. Karpenter's disruption controller ticks
/// every 10s; 30s gives it three passes to mark `Disrupting` if the
/// shim's `budgets:nodes:"0"` is misconfigured.
const DRIFT_HOLD: Duration = Duration::from_secs(30);

pub async fn run() -> Result<()> {
    let kube = kube::client()
        .await
        .context("kube client (run `xtask k8s -p eks up --kubeconfig` first)")?;
    let sla = load_sla_config(&kube).await?;

    let cells = cells(&sla);
    ensure!(
        !cells.is_empty(),
        "sla.hwClasses is empty — probe-boot needs at least one hw-class. \
         Populate `scheduler.sla.hwClasses` in the helm values and `up --deploy`."
    );
    let node_class = sla.node_class_ref.as_deref().unwrap_or("rio-default");
    info!(
        "probing {} cell(s) ({} hw-class × {} capacity-type), nodeClassRef={node_class}",
        cells.len(),
        sla.hw_classes.len(),
        CapacityType::ALL.len(),
    );

    ensure_shim_nodepool(&kube, node_class).await?;

    let claims = nodeclaim_api(&kube);
    let nodes: Api<Node> = Api::all(kube.clone());
    let mut created: Vec<String> = Vec::new();
    // Keyed by `"h:cap"` (cell_key) so iteration order is stable and
    // matches the `[sla.lead_time_seed]` serde shape directly.
    let mut seeds: BTreeMap<String, f64> = BTreeMap::new();

    let result: Result<()> = async {
        // Drive every cell to Registered, collecting boot times. The
        // last cell's claim is held for assertions 4+5 instead of being
        // deleted in-loop.
        //
        // Serial (one NodeClaim at a time, ≤300s each) is intentional
        // for first-run: 12 simultaneous claims would race spot ICE
        // retry across cells and pile up CreateFleet quota. A
        // `--parallel` flag for re-runs is a follow-up.
        // TODO: `--parallel` re-probe — fan out cells via join_all once
        // the first serial run has populated leadTimeSeed.
        let last = cells.len() - 1;
        let mut last_claim: Option<String> = None;
        for (i, cell) in cells.iter().enumerate() {
            let def = &sla.hw_classes[&cell.0];
            let nc = mk_probe_nodeclaim(cell, def, node_class);
            let start = jiff::Timestamp::now();
            let obj = claims
                .create(&PostParams::default(), &nc)
                .await
                .with_context(|| {
                    format!(
                        "create probe NodeClaim for {cell:?}; check Karpenter CRDs are installed"
                    )
                })?;
            let name = obj
                .metadata
                .name
                .clone()
                .context("apiserver returned NodeClaim without metadata.name")?;
            created.push(name.clone());
            info!("  [{}] created NodeClaim {name}", cell_key(cell));

            let reg = wait_condition(&claims, &name, "Registered", REGISTER_TIMEOUT)
                .await
                .with_context(|| {
                    format!(
                        "NodeClaim {name} never reached Registered=True within \
                         {REGISTER_TIMEOUT:?}; `kubectl describe nodeclaim {name}` for the \
                         blocking condition"
                    )
                })?;

            // ── assertion 3: lastTransitionTime populated ──────────
            let ltt = reg
                .get("lastTransitionTime")
                .and_then(Value::as_str)
                .with_context(|| {
                    format!(
                        "assertion 3 FAIL: NodeClaim {name} Registered condition has no \
                         lastTransitionTime — Karpenter status writer broken? \
                         `kubectl get nodeclaim {name} -o yaml`"
                    )
                })?;
            let reg_ts: jiff::Timestamp = ltt.parse().with_context(|| {
                format!("assertion 3 FAIL: lastTransitionTime {ltt:?} not RFC3339")
            })?;
            let boot = (reg_ts - start).get_seconds() as f64;
            ensure!(
                boot > 0.0,
                "assertion 3 FAIL: Registered.lastTransitionTime={ltt} predates probe \
                 creation ({start}) — clock skew or stale condition?"
            );
            seeds.insert(cell_key(cell), boot);
            info!("  [{}] Registered in {boot:.1}s", cell_key(cell));

            // ── assertion 1: naked NodeClaim launched ──────────────
            // Re-read post-Registered: Karpenter never adds an
            // ownerReference to a NodeClaim it didn't create. If one
            // appeared, Karpenter adopted it under a real NodePool —
            // §13b's lifecycle model (rio owns deletion) is broken.
            let live = claims.get(&name).await?;
            let owners = live.metadata.owner_references.unwrap_or_default();
            ensure!(
                owners.is_empty(),
                "assertion 1 FAIL: NodeClaim {name} has ownerReferences {owners:?} \
                 — Karpenter adopted the naked claim under a NodePool. §13b requires \
                 rio-owned lifecycle; check no NodePool's selector accidentally \
                 matches `{PROBE_LABEL}=true`."
            );

            if i == last {
                last_claim = Some(name);
            } else {
                claims.delete(&name, &DeleteParams::default()).await?;
            }
        }

        let held = last_claim.expect("cells non-empty");

        // ── assertion 4: nodepool label survives to Node ───────────
        let live = claims.get(&held).await?;
        let node_name = live
            .data
            .pointer("/status/nodeName")
            .and_then(Value::as_str)
            .with_context(|| {
                format!(
                    "assertion 4 FAIL: NodeClaim {held} has no status.nodeName after \
                     Registered — `kubectl get nodeclaim {held} -o jsonpath='{{.status}}'`"
                )
            })?;
        let node = nodes.get(node_name).await.with_context(|| {
            format!("assertion 4 FAIL: Node {node_name} (from NodeClaim {held}) not found")
        })?;
        let node_label = node
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("karpenter.sh/nodepool"));
        ensure!(
            node_label.map(String::as_str) == Some(SHIM_NODEPOOL),
            "assertion 4 FAIL: Node {node_name} label karpenter.sh/nodepool={node_label:?}, \
             expected {SHIM_NODEPOOL:?}. The controller-stamped label on \
             NodeClaim.metadata.labels must propagate to the Node — check Karpenter's \
             node registration webhook."
        );
        info!("  assertion 4 ok: Node {node_name} carries karpenter.sh/nodepool={SHIM_NODEPOOL}");

        // ── assertion 5: budgets:nodes:"0" blocks drift ────────────
        // Hold the last probe 30s past Registered. The node is empty
        // (no pods tolerate `rio.build/builder`), so consolidation
        // would normally mark it; the shim's budget must block.
        info!("  holding {held} for {DRIFT_HOLD:?} (assertion 5 drift-budget check)…");
        tokio::time::sleep(DRIFT_HOLD).await;
        let live = claims.get_opt(&held).await?.with_context(|| {
            format!(
                "assertion 5 FAIL: NodeClaim {held} deleted during the {DRIFT_HOLD:?} hold \
                 — disruption budget did not block. Check NodePool {SHIM_NODEPOOL} has \
                 `disruption.budgets: [{{nodes: \"0\"}}]`."
            )
        })?;
        let disrupting = find_condition(&live, "Disrupting")
            .or_else(|| find_condition(&live, "Disrupted"))
            .filter(|c| c.get("status").and_then(Value::as_str) == Some("True"));
        ensure!(
            disrupting.is_none(),
            "assertion 5 FAIL: NodeClaim {held} has {disrupting:?} after {DRIFT_HOLD:?} \
             — `budgets:nodes:\"0\"` did not block disruption. Fix the shim NodePool's \
             `disruption.budgets` and re-run."
        );
        ensure!(
            live.metadata.deletion_timestamp.is_none(),
            "assertion 5 FAIL: NodeClaim {held} has deletionTimestamp — Karpenter began \
             terminating it despite `budgets:nodes:\"0\"`."
        );
        info!("  assertion 5 ok: {held} undisrupted after {DRIFT_HOLD:?}");

        claims.delete(&held, &DeleteParams::default()).await?;
        Ok(())
    }
    .await;

    // ── assertion 2: shim NodePool never provisioned ───────────────
    // Runs regardless of `result` so a mid-loop failure still surfaces
    // a shim leak. Our own probes carry `rio.build/probe=true`; any
    // shim-labelled claim WITHOUT it is one Karpenter created on the
    // shim's behalf — `limits:{cpu:0}` should make that impossible.
    let shim_leaks: Vec<String> = claims
        .list(&ListParams::default().labels(&format!(
            "karpenter.sh/nodepool={SHIM_NODEPOOL},{PROBE_LABEL}!=true"
        )))
        .await?
        .into_iter()
        .filter(|nc| {
            find_condition(nc, "Launched")
                .is_some_and(|c| c.get("status").and_then(Value::as_str) == Some("True"))
        })
        .filter_map(|nc| nc.metadata.name)
        .collect();
    ensure!(
        shim_leaks.is_empty(),
        "assertion 2 FAIL: shim NodePool {SHIM_NODEPOOL} provisioned NodeClaim(s) \
         {shim_leaks:?} (Launched=True). The shim MUST have `limits:{{cpu:0}}` so it \
         never provisions — fix `infra/helm/rio-build/templates/karpenter.yaml` and \
         redeploy."
    );

    // Best-effort cleanup of anything still around (mid-loop bail,
    // delete race). Operator can also `kubectl delete nodeclaims -l
    // rio.build/probe=true`.
    for name in &created {
        if let Err(e) = claims.delete(name, &DeleteParams::default()).await {
            warn!("cleanup: delete NodeClaim {name}: {e}");
        }
    }

    result?;
    print_seeds(&seeds);
    Ok(())
}

/// Idempotently ensure the §13b shim NodePool exists. Karpenter refuses
/// to reconcile a NodeClaim whose `karpenter.sh/nodepool` label points
/// at a missing NodePool — the claim sits at `AwaitingReconciliation`
/// forever. The probe stamps that label, so it must pre-create the pool.
///
/// The shim is inert: `limits.cpu=0` means it never provisions on its
/// own (assertion 2), `budgets:[{nodes:"0"}]` means it never disrupts
/// (assertion 5), `expireAfter: Never` means probe nodes aren't
/// drift-churned. NOT deleted on exit — B3 helm-manages the same object
/// later, and `cpu:0` makes it harmless to leave.
async fn ensure_shim_nodepool(client: &kube::Client, node_class: &str) -> Result<()> {
    let pools = nodepool_api(client);
    if pools.get_opt(SHIM_NODEPOOL).await?.is_some() {
        info!("{SHIM_NODEPOOL} already present (B3 helm-managed?)");
        return Ok(());
    }
    let np = mk_shim_nodepool(node_class);
    pools
        .create(&PostParams::default(), &np)
        .await
        .with_context(|| {
            format!(
                "create NodePool {SHIM_NODEPOOL}; check Karpenter CRDs are installed \
                 (`kubectl get crd nodepools.karpenter.sh`)"
            )
        })?;
    info!("ensured {SHIM_NODEPOOL} NodePool (limits.cpu=0, budgets=0)");
    Ok(())
}

/// §13b shim NodePool spec. Field shapes mirror the working NodePools in
/// `infra/helm/rio-build/templates/karpenter.yaml`. `requirements` needs
/// at least one entry (Karpenter v1 CRD validation); `kubernetes.io/os
/// In [linux]` is the no-op choice — the NodeClaim's own requirements
/// drive actual instance selection.
fn mk_shim_nodepool(node_class: &str) -> DynamicObject {
    serde_json::from_value(json!({
        "apiVersion": "karpenter.sh/v1",
        "kind": "NodePool",
        "metadata": {
            "name": SHIM_NODEPOOL,
            "labels": {PROBE_LABEL: "true"},
        },
        "spec": {
            "limits": {"cpu": "0"},
            "disruption": {
                "budgets": [{"nodes": "0"}],
                "consolidationPolicy": "WhenEmpty",
                "consolidateAfter": "Never",
            },
            "template": {
                "spec": {
                    "nodeClassRef": {
                        "group": "karpenter.k8s.aws",
                        "kind": "EC2NodeClass",
                        "name": node_class,
                    },
                    "requirements": [{
                        "key": "kubernetes.io/os",
                        "operator": "In",
                        "values": ["linux"],
                    }],
                    "expireAfter": "Never",
                },
            },
        },
    }))
    .expect("static NodePool json")
}

/// Read the live `[sla]` table from the `rio-scheduler-config`
/// ConfigMap. Probing what's DEPLOYED (not local helm values) means
/// the seeds match the cluster the operator is about to enable §13b on.
async fn load_sla_config(client: &kube::Client) -> Result<SlaConfig> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), NS);
    let cm = api
        .get("rio-scheduler-config")
        .await
        .context("ConfigMap rio-system/rio-scheduler-config not found; run `up --deploy` first")?;
    let body = cm
        .data
        .and_then(|d| d.get("scheduler.toml").cloned())
        .context("rio-scheduler-config missing key `scheduler.toml`")?;
    let v: toml::Value = toml::from_str(&body)
        .with_context(|| format!("parse scheduler.toml from ConfigMap:\n{body}"))?;
    let sla = v
        .get("sla")
        .cloned()
        .context("scheduler.toml has no [sla] table — chart must render `scheduler.sla`")?;
    sla.try_into()
        .context("deserialize [sla] as SlaConfig — schema drift between chart and rio-scheduler?")
}

/// `hw_classes × {spot,od}`. Sorted so output is stable across runs.
fn cells(sla: &SlaConfig) -> Vec<Cell> {
    let mut hs: Vec<_> = sla.hw_classes.keys().cloned().collect();
    hs.sort();
    hs.into_iter()
        .flat_map(|h| CapacityType::ALL.map(move |c| (h.clone(), c)))
        .collect()
}

/// `"h:spot"` / `"h:od"` — same key shape `[sla.lead_time_seed]` uses
/// (cell_key_serde), so the YAML block is paste-ready.
fn cell_key((h, c): &Cell) -> String {
    let cap = match c {
        CapacityType::Spot => "spot",
        CapacityType::Od => "od",
    };
    format!("{h}:{cap}")
}

/// Build a naked probe NodeClaim mirroring what the §13b controller
/// will emit: `karpenter.sh/nodepool` shim label + `rio.build/*`
/// labels, EC2NodeClass ref, hw-class label conjunction + capacity-type
/// requirement, NO `ownerReferences`. `generateName` so re-runs don't
/// conflict.
fn mk_probe_nodeclaim(cell: &Cell, def: &HwClassDef, node_class: &str) -> DynamicObject {
    let (h, cap) = cell;
    let mut reqs: Vec<Value> = def
        .labels
        .iter()
        .map(|m| json!({"key": m.key, "operator": "In", "values": [m.value]}))
        .collect();
    reqs.push(json!({
        "key": "karpenter.sh/capacity-type",
        "operator": "In",
        "values": [cap.label()],
    }));
    serde_json::from_value(json!({
        "apiVersion": "karpenter.sh/v1",
        "kind": "NodeClaim",
        "metadata": {
            "generateName": format!("rio-probe-{h}-{}-", cap.label()),
            "labels": {
                "karpenter.sh/nodepool": SHIM_NODEPOOL,
                PROBE_LABEL: "true",
                "rio.build/hw-class": h,
            },
        },
        "spec": {
            "nodeClassRef": {
                "group": "karpenter.k8s.aws",
                "kind": "EC2NodeClass",
                "name": node_class,
            },
            "requirements": reqs,
        },
    }))
    .expect("static NodeClaim json")
}

/// Poll `name` until `.status.conditions[type=cond].status == "True"`.
/// Returns the matching condition object (so callers read
/// `lastTransitionTime`). Surfaces the blocking condition's `reason`
/// while waiting.
async fn wait_condition(
    api: &Api<DynamicObject>,
    name: &str,
    cond: &str,
    timeout: Duration,
) -> Result<Value> {
    let interval = Duration::from_secs(5);
    let max = (timeout.as_secs() / interval.as_secs()).max(1) as u32;
    ui::poll(
        &format!("NodeClaim {name} {cond}=True"),
        interval,
        max,
        || async {
            let Some(nc) = api.get_opt(name).await? else {
                bail!("NodeClaim {name} disappeared while waiting for {cond}")
            };
            if let Some(c) = find_condition(&nc, cond)
                && c.get("status").and_then(Value::as_str) == Some("True")
            {
                return Ok(Some(c));
            }
            // Surface the first non-True condition's reason so the
            // operator sees ICE / AMINotFound / Unschedulable without
            // a separate `kubectl describe`.
            if let Some(reason) = nc
                .data
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|c| c.get("status").and_then(Value::as_str) != Some("True"))
                .and_then(|c| c.get("reason").and_then(Value::as_str))
            {
                info!("  {name}: waiting ({reason})");
            }
            Ok(None)
        },
    )
    .await
}

fn find_condition(nc: &DynamicObject, cond: &str) -> Option<Value> {
    nc.data
        .pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(cond))
        .cloned()
}

fn print_seeds(seeds: &BTreeMap<String, f64>) {
    println!("\nall 5 conformance assertions PASS\n");
    println!("{:<32} {:>10}", "cell", "boot_secs");
    println!("{:-<32} {:->10}", "", "");
    for (cell, boot) in seeds {
        println!("{cell:<32} {boot:>10.1}");
    }
    println!(
        "\n# paste into infra/helm/rio-build/values.yaml scheduler.sla.leadTimeSeed:\nleadTimeSeed:"
    );
    for (cell, boot) in seeds {
        println!("  {cell:?}: {boot:.1}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_scheduler::sla::config::NodeLabelMatch;

    #[test]
    fn probe_nodeclaim_shape() {
        let def = HwClassDef {
            labels: vec![NodeLabelMatch {
                key: "rio.build/hw-band".into(),
                value: "gen7".into(),
            }],
        };
        let cell = ("gen7-nvme".into(), CapacityType::Spot);
        let nc = mk_probe_nodeclaim(&cell, &def, "rio-nvme");
        let v = serde_json::to_value(&nc).unwrap();

        // Assertion 1 prerequisite: no ownerReferences emitted.
        assert!(
            v.pointer("/metadata/ownerReferences").is_none(),
            "naked NodeClaim must not carry ownerReferences"
        );
        // Assertion 4 prerequisite: shim nodepool label stamped.
        assert_eq!(
            v.pointer("/metadata/labels/karpenter.sh~1nodepool")
                .and_then(Value::as_str),
            Some(SHIM_NODEPOOL)
        );
        // Probe label present (cleanup + assertion-2 exclusion).
        assert_eq!(
            v.pointer(&format!(
                "/metadata/labels/{}",
                PROBE_LABEL.replace('/', "~1")
            ))
            .and_then(Value::as_str),
            Some("true")
        );
        // Capacity-type requirement carries Karpenter's label value
        // ("spot"/"on-demand"), not the serde enum variant.
        let reqs = v.pointer("/spec/requirements").unwrap().as_array().unwrap();
        assert!(
            reqs.iter()
                .any(|r| r["key"] == "karpenter.sh/capacity-type" && r["values"][0] == "spot")
        );
        assert_eq!(
            v.pointer("/spec/nodeClassRef/name").and_then(Value::as_str),
            Some("rio-nvme")
        );
    }

    #[test]
    fn shim_nodepool_shape() {
        let np = mk_shim_nodepool("rio-default");
        let v = serde_json::to_value(&np).unwrap();
        assert_eq!(
            v.pointer("/metadata/name").and_then(Value::as_str),
            Some(SHIM_NODEPOOL)
        );
        // Assertion 2 prerequisite: cpu=0 so the shim never provisions.
        assert_eq!(
            v.pointer("/spec/limits/cpu").and_then(Value::as_str),
            Some("0")
        );
        // Assertion 5 prerequisite: budgets nodes=0 so it never disrupts.
        assert_eq!(
            v.pointer("/spec/disruption/budgets/0/nodes")
                .and_then(Value::as_str),
            Some("0")
        );
        // CRD requires ≥1 requirement; use the linux no-op.
        assert_eq!(
            v.pointer("/spec/template/spec/requirements/0/key")
                .and_then(Value::as_str),
            Some("kubernetes.io/os")
        );
        assert_eq!(
            v.pointer("/spec/template/spec/nodeClassRef/name")
                .and_then(Value::as_str),
            Some("rio-default")
        );
        assert_eq!(
            v.pointer("/spec/template/spec/expireAfter")
                .and_then(Value::as_str),
            Some("Never")
        );
    }

    #[test]
    fn cell_key_matches_serde_shape() {
        // Must match `cell_key_serde`'s `"h:cap"` so the YAML block is
        // paste-ready into `[sla.lead_time_seed]`.
        assert_eq!(cell_key(&("h0".into(), CapacityType::Spot)), "h0:spot");
        assert_eq!(cell_key(&("h0".into(), CapacityType::Od)), "h0:od");
    }

    #[test]
    fn find_condition_reads_status_shape() {
        let nc: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "karpenter.sh/v1",
            "kind": "NodeClaim",
            "metadata": {"name": "x"},
            "status": {"conditions": [
                {"type": "Launched", "status": "True"},
                {"type": "Registered", "status": "True",
                 "lastTransitionTime": "2026-01-01T00:00:00Z"},
            ]},
        }))
        .unwrap();
        let c = find_condition(&nc, "Registered").unwrap();
        assert_eq!(c["lastTransitionTime"], "2026-01-01T00:00:00Z");
        assert!(find_condition(&nc, "Drifted").is_none());
    }
}
