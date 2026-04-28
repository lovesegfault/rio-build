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

use std::collections::{BTreeMap, BTreeSet};
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
        "{} cell(s) ({} hw-class × {} capacity-type), nodeClassRef={node_class}",
        cells.len(),
        sla.hw_classes.len(),
        CapacityType::ALL.len(),
    );

    ensure_shim_nodepool(&kube, node_class).await?;

    // Snapshot all NodePools' template-labels + template-requirements so
    // each hw-class can be resolved to the NodePool that would actually
    // serve it. The hw-class's `labels` are NODE labels a NodePool STAMPS
    // (template.metadata.labels), not instance-type selectors — putting
    // them in `requirements` constrains nothing and Karpenter picks the
    // cheapest instance (t3.nano) which then can't register.
    let pool_templates = list_pool_templates(&kube).await?;

    // Expand cells × matching-NodePools so each arch is probed
    // separately. Previously `resolve_pool` picked the first name-sorted
    // match (always `*-aarch64`), so x86 boot times were never measured
    // and we had no data on whether arch affects boot.
    let mut probes: Vec<(Cell, &PoolTemplate, String)> = Vec::new();
    let mut arches: BTreeSet<String> = BTreeSet::new();
    for cell in &cells {
        let def = &sla.hw_classes[&cell.0];
        for pool in resolve_pools(&pool_templates, &cell.0, def)? {
            let arch = pool_arch(pool);
            arches.insert(arch.clone());
            probes.push((cell.clone(), pool, arch));
        }
    }
    info!(
        "probing {} cell(s) × {} arch(es) = {} NodeClaims",
        cells.len(),
        arches.len(),
        probes.len(),
    );

    let claims = nodeclaim_api(&kube);
    let nodes: Api<Node> = Api::all(kube.clone());
    let mut created: Vec<String> = Vec::new();
    let mut results: Vec<ProbeResult> = Vec::new();

    let result: Result<()> = async {
        // Drive every (cell, pool) to Registered, collecting boot times.
        // The last claim is held for assertions 4+5 instead of being
        // deleted in-loop.
        //
        // Serial (one NodeClaim at a time, ≤300s each) is intentional
        // for first-run: 12 simultaneous claims would race spot ICE
        // retry across cells and pile up CreateFleet quota. A
        // `--parallel` flag for re-runs is a follow-up.
        // TODO: `--parallel` re-probe — fan out cells via join_all once
        // the first serial run has populated leadTimeSeed.
        let last = probes.len() - 1;
        let mut last_claim: Option<String> = None;
        for (i, (cell, pool, arch)) in probes.iter().enumerate() {
            let key = format!("{}:{arch}", cell_key(cell));
            info!(
                "  [{key}] resolved NodePool={}, requirements=[{}]",
                pool.name,
                fmt_requirements(&pool.requirements),
            );
            let nc = mk_probe_nodeclaim(cell, pool, node_class);
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
            info!("  [{key}] created NodeClaim {name}");

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

            // ── assertion 1: naked NodeClaim launched ──────────────
            // Re-read post-Registered: Karpenter never adds an
            // ownerReference to a NodeClaim it didn't create. If one
            // appeared, Karpenter adopted it under a real NodePool —
            // §13b's lifecycle model (rio owns deletion) is broken.
            let live = claims.get(&name).await?;
            let instance_type = live
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("node.kubernetes.io/instance-type"))
                .cloned()
                .unwrap_or_else(|| "?".into());
            results.push(ProbeResult {
                hw_class: cell.0.clone(),
                cap: cell.1,
                arch: arch.clone(),
                pool: pool.name.clone(),
                instance_type,
                boot_secs: boot,
            });
            info!("  [{key}] Registered in {boot:.1}s");
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
    print_results(&mut results);
    Ok(())
}

/// One (hw_class, cap, arch) probe observation.
#[derive(Debug)]
struct ProbeResult {
    hw_class: String,
    cap: CapacityType,
    arch: String,
    pool: String,
    instance_type: String,
    boot_secs: f64,
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

/// One NodePool's template surface, extracted for the hw-class →
/// NodePool resolver.
#[derive(Debug, Clone)]
struct PoolTemplate {
    name: String,
    /// `spec.template.metadata.labels` — labels Karpenter STAMPS onto
    /// nodes this pool launches (e.g. `rio.build/hw-band`,
    /// `rio.build/storage`). These are what hw-class `labels` match.
    stamps: BTreeMap<String, String>,
    /// `spec.template.spec.requirements` — instance-type selectors
    /// (e.g. `instance-category In [c,m,r]`). These are what a
    /// NodeClaim needs to actually constrain the launch.
    requirements: Vec<Value>,
}

/// List every NodePool's template-labels + template-requirements.
/// Sorted by name so [`resolve_pools`] returns a stable order when
/// multiple pools stamp the same labels (e.g. x86 + arm64 variants of
/// the same band/storage).
async fn list_pool_templates(client: &kube::Client) -> Result<Vec<PoolTemplate>> {
    let pools = nodepool_api(client);
    let mut out: Vec<PoolTemplate> = pools
        .list(&ListParams::default())
        .await
        .context("list NodePools (Karpenter CRD installed?)")?
        .into_iter()
        .filter_map(|np| {
            let name = np.metadata.name?;
            let stamps = np
                .data
                .pointer("/spec/template/metadata/labels")
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let requirements = np
                .data
                .pointer("/spec/template/spec/requirements")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Some(PoolTemplate {
                name,
                stamps,
                requirements,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Find ALL NodePools whose `template.metadata.labels` ⊇ the
/// hw-class's `labels` (every `{key,value}` in `def.labels` is stamped
/// by the pool). The match is on STAMPED labels because that's what an
/// hw-class is: "a node carrying these labels". Each matched pool's
/// `template.spec.requirements` are the instance-type constraints a
/// probe NodeClaim must carry to launch that hardware.
///
/// Returns every match (sorted by name via [`list_pool_templates`]) so
/// both `…-aarch64` and `…-x86` variants of the same band/storage get
/// probed — arch may affect boot time and the previous first-match-only
/// behaviour silently picked `…-aarch64` (alphabetical) and never
/// measured x86.
fn resolve_pools<'a>(
    pools: &'a [PoolTemplate],
    h: &str,
    def: &HwClassDef,
) -> Result<Vec<&'a PoolTemplate>> {
    let matched: Vec<&PoolTemplate> = pools
        .iter()
        .filter(|p| {
            def.labels
                .iter()
                .all(|m| p.stamps.get(&m.key).map(String::as_str) == Some(&m.value))
        })
        .collect();
    if matched.is_empty() {
        let labels: Vec<String> = def
            .labels
            .iter()
            .map(|m| format!("{}={}", m.key, m.value))
            .collect();
        bail!(
            "hw-class {h:?} labels {labels:?} match no NodePool's template.metadata.labels — \
             check that scheduler.sla.hwClasses keys on labels a builder NodePool actually \
             stamps (e.g. rio.build/hw-band, rio.build/storage)"
        );
    }
    Ok(matched)
}

/// Extract arch from a NodePool: prefer the `kubernetes.io/arch In
/// [...]` requirement value (what Karpenter actually constrains on),
/// fall back to the pool-name suffix (`-aarch64` / `-x86`), else `?`.
fn pool_arch(pool: &PoolTemplate) -> String {
    pool.requirements
        .iter()
        .find(|r| r.get("key").and_then(Value::as_str) == Some("kubernetes.io/arch"))
        .and_then(|r| r.get("values")?.as_array()?.first()?.as_str())
        .map(str::to_string)
        .or_else(|| {
            ["aarch64", "x86"]
                .into_iter()
                .find(|a| pool.name.ends_with(&format!("-{a}")))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "?".into())
}

/// Compact one-line summary of a requirements array for the resolver
/// log line: `instance-category In [c,m,r], instance-generation In [6], …`.
fn fmt_requirements(reqs: &[Value]) -> String {
    reqs.iter()
        .filter_map(|r| {
            let key = r.get("key")?.as_str()?;
            let key = key.rsplit_once('/').map_or(key, |(_, k)| k);
            let op = r.get("operator")?.as_str()?;
            let vals: Vec<&str> = r
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            Some(format!("{key} {op} [{}]", vals.join(",")))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a naked probe NodeClaim mirroring what the §13b controller
/// will emit: `karpenter.sh/nodepool` shim label + `rio.build/*`
/// labels, EC2NodeClass ref, the resolved NodePool's instance-type
/// requirements + capacity-type narrowing, NO `ownerReferences`.
/// `generateName` so re-runs don't conflict.
///
/// `metadata.labels` carries the matched NodePool's template-labels
/// (so the launched Node has `rio.build/hw-band`/`storage` and
/// assertion 4 checks something real) plus the shim/probe/hw-class
/// markers.
fn mk_probe_nodeclaim(cell: &Cell, pool: &PoolTemplate, node_class: &str) -> DynamicObject {
    let (h, cap) = cell;
    let mut reqs = pool.requirements.clone();
    // The NodePool's own capacity-type req is `In [spot, on-demand]`;
    // requirements AND, so appending the cell's narrows correctly.
    reqs.push(json!({
        "key": "karpenter.sh/capacity-type",
        "operator": "In",
        "values": [cap.label()],
    }));
    let mut labels = pool.stamps.clone();
    labels.insert("karpenter.sh/nodepool".into(), SHIM_NODEPOOL.into());
    labels.insert(PROBE_LABEL.into(), "true".into());
    labels.insert("rio.build/hw-class".into(), h.clone());
    serde_json::from_value(json!({
        "apiVersion": "karpenter.sh/v1",
        "kind": "NodeClaim",
        "metadata": {
            "generateName": format!("rio-probe-{h}-{}-", cap.label()),
            "labels": labels,
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
/// `lastTransitionTime`). Surfaces the SPECIFIC waited-on condition's
/// `reason` (falling back to `Launched`'s reason pre-launch) plus the
/// chosen instance-type once Launched, so the operator sees e.g.
/// `NodeNotFound` / `t3.nano` immediately instead of after a 300s
/// timeout.
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
            // Reason for the SPECIFIC condition we're waiting on (not
            // "first non-True", which previously surfaced
            // `Initialized`'s `AwaitingReconciliation` while
            // `Registered`'s actual reason was `NodeNotFound`). If the
            // waited-on condition isn't posted yet, fall back to
            // `Launched` — that's where ICE / AMINotFound /
            // UnfulfillableCapacity live pre-launch.
            let reason = find_condition(&nc, cond)
                .or_else(|| find_condition(&nc, "Launched"))
                .and_then(|c| c.get("reason").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "Pending".into());
            // Karpenter writes the picked instance type to the
            // NodeClaim's labels at Launched=True — surface it so a
            // bad pick (t3.nano) is visible on the first poll, not
            // after the 300s timeout.
            let inst = nc
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("node.kubernetes.io/instance-type"))
                .map(|t| format!(", instance-type={t}"))
                .unwrap_or_default();
            info!("  {name}: waiting ({reason}{inst})");
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

fn print_results(results: &mut [ProbeResult]) {
    results.sort_by(|a, b| {
        (&a.hw_class, a.cap.label(), &a.arch).cmp(&(&b.hw_class, b.cap.label(), &b.arch))
    });

    println!("\nall 5 conformance assertions PASS\n");
    println!(
        "{:<18} {:<5} {:<8} {:<28} {:<16} {:>10}",
        "hw-class", "cap", "arch", "pool", "instance-type", "boot-secs"
    );
    println!(
        "{:-<18} {:-<5} {:-<8} {:-<28} {:-<16} {:->10}",
        "", "", "", "", "", ""
    );
    for r in results.iter() {
        println!(
            "{:<18} {:<5} {:<8} {:<28} {:<16} {:>10.1}",
            r.hw_class,
            r.cap.label(),
            r.arch,
            r.pool,
            r.instance_type,
            r.boot_secs,
        );
    }

    // leadTimeSeed: max boot per cell. With arch in the hw-class label
    // conjunction (12 keys), resolve_pools() returns a single pool per
    // hw-class and the max-fold here is the identity. Kept as a
    // structural guard for future multi-match conjunctions.
    let mut seeds: BTreeMap<String, f64> = BTreeMap::new();
    for r in results.iter() {
        let key = cell_key(&(r.hw_class.clone(), r.cap));
        let slot = seeds.entry(key).or_insert(0.0);
        *slot = slot.max(r.boot_secs);
    }
    println!(
        "\n# paste into infra/helm/rio-build/values.yaml scheduler.sla.leadTimeSeed:\n\
         # with arch in hw-class, each cell resolves one pool (24 entries).\n\
         leadTimeSeed:"
    );
    for (cell, boot) in &seeds {
        println!("  {cell:?}: {boot:.1}");
    }
    println!("# per-pool breakdown:");
    for r in results.iter() {
        let key = cell_key(&(r.hw_class.clone(), r.cap));
        println!("#   \"{key}:{}\": {:.1}", r.arch, r.boot_secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_scheduler::sla::config::NodeLabelMatch;

    fn mk_pool(name: &str, stamps: &[(&str, &str)], reqs: Value) -> PoolTemplate {
        PoolTemplate {
            name: name.into(),
            stamps: stamps
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            requirements: reqs.as_array().cloned().unwrap_or_default(),
        }
    }

    fn hw_class(labels: &[(&str, &str)]) -> HwClassDef {
        HwClassDef {
            labels: labels
                .iter()
                .map(|(k, v)| NodeLabelMatch {
                    key: (*k).into(),
                    value: (*v).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_pools_matches_stamped_labels() {
        // Two pools: one stamps {hw-band:lo, storage:ebs} with real
        // instance-type reqs; one stamps {hw-band:hi}. gen6-ebs-lo's
        // hw-class labels must resolve to the first only.
        let pools = vec![
            mk_pool(
                "rio-builder-hi-nvme-x86",
                &[("rio.build/hw-band", "hi"), ("rio.build/storage", "nvme")],
                json!([{"key": "karpenter.k8s.aws/instance-generation",
                        "operator": "In", "values": ["7"]}]),
            ),
            mk_pool(
                "rio-builder-lo-ebs-x86",
                &[
                    ("rio.build/hw-band", "lo"),
                    ("rio.build/storage", "ebs"),
                    ("rio.build/node-role", "builder"),
                ],
                json!([
                    {"key": "karpenter.k8s.aws/instance-category",
                     "operator": "In", "values": ["c","m","r"]},
                    {"key": "karpenter.k8s.aws/instance-generation",
                     "operator": "In", "values": ["6"]},
                ]),
            ),
        ];
        let def = hw_class(&[("rio.build/hw-band", "lo"), ("rio.build/storage", "ebs")]);
        let ps = resolve_pools(&pools, "gen6-ebs-lo", &def).unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].name, "rio-builder-lo-ebs-x86");
        assert_eq!(ps[0].requirements.len(), 2);
        // Superset OK: pool stamps node-role too, hw-class doesn't ask.
        assert_eq!(ps[0].stamps.len(), 3);

        // Unmatched hw-class bails with the actionable message.
        let bad = hw_class(&[("rio.build/hw-band", "mid")]);
        let err = resolve_pools(&pools, "mid-ebs", &bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mid-ebs"), "{err}");
        assert!(
            err.contains("match no NodePool's template.metadata.labels"),
            "{err}"
        );
    }

    #[test]
    fn resolve_pools_returns_all_matches() {
        // x86 and arm64 variants stamp identical band/storage labels.
        // Resolver must return BOTH (sorted), and pool_arch must
        // identify each.
        let mut pools = vec![
            mk_pool(
                "rio-builder-lo-ebs-x86",
                &[("rio.build/hw-band", "lo"), ("rio.build/storage", "ebs")],
                json!([{"key": "kubernetes.io/arch", "operator": "In",
                        "values": ["amd64"]}]),
            ),
            mk_pool(
                "rio-builder-lo-ebs-aarch64",
                &[("rio.build/hw-band", "lo"), ("rio.build/storage", "ebs")],
                json!([{"key": "kubernetes.io/arch", "operator": "In",
                        "values": ["arm64"]}]),
            ),
        ];
        pools.sort_by(|a, b| a.name.cmp(&b.name));
        let def = hw_class(&[("rio.build/hw-band", "lo"), ("rio.build/storage", "ebs")]);
        let ps = resolve_pools(&pools, "gen6-ebs-lo", &def).unwrap();
        assert_eq!(
            ps.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["rio-builder-lo-ebs-aarch64", "rio-builder-lo-ebs-x86"]
        );
        assert_eq!(pool_arch(ps[0]), "arm64");
        assert_eq!(pool_arch(ps[1]), "amd64");
    }

    #[test]
    fn pool_arch_fallback_to_name_suffix() {
        // No kubernetes.io/arch req → fall back to name suffix.
        let p = mk_pool(
            "rio-builder-lo-ebs-aarch64",
            &[],
            json!([{"key": "karpenter.k8s.aws/instance-category",
                    "operator": "In", "values": ["c"]}]),
        );
        assert_eq!(pool_arch(&p), "aarch64");
        let p = mk_pool("rio-builder-lo-ebs-x86", &[], json!([]));
        assert_eq!(pool_arch(&p), "x86");
        // Neither → "?".
        let p = mk_pool("rio-nodeclaim-shim", &[], json!([]));
        assert_eq!(pool_arch(&p), "?");
    }

    #[test]
    fn print_results_seeds_max_across_arches() {
        // Two arches for one cell; leadTimeSeed must use the max.
        let mut rs = vec![
            ProbeResult {
                hw_class: "lo".into(),
                cap: CapacityType::Spot,
                arch: "amd64".into(),
                pool: "p-x86".into(),
                instance_type: "c6a.large".into(),
                boot_secs: 80.0,
            },
            ProbeResult {
                hw_class: "lo".into(),
                cap: CapacityType::Spot,
                arch: "arm64".into(),
                pool: "p-aarch64".into(),
                instance_type: "c6g.large".into(),
                boot_secs: 95.0,
            },
        ];
        // Sort order: (hw_class, cap, arch) — amd64 before arm64.
        rs.sort_by(|a, b| {
            (&a.hw_class, a.cap.label(), &a.arch).cmp(&(&b.hw_class, b.cap.label(), &b.arch))
        });
        assert_eq!(rs[0].arch, "amd64");
        // Max-across-arches reduction (mirrors print_results body).
        let mut seeds: BTreeMap<String, f64> = BTreeMap::new();
        for r in &rs {
            let key = cell_key(&(r.hw_class.clone(), r.cap));
            let slot = seeds.entry(key).or_insert(0.0);
            *slot = slot.max(r.boot_secs);
        }
        assert_eq!(seeds["lo:spot"], 95.0);
    }

    #[test]
    fn probe_nodeclaim_shape() {
        let pool = mk_pool(
            "rio-builder-lo-ebs-x86",
            &[
                ("rio.build/hw-band", "lo"),
                ("rio.build/storage", "ebs"),
                ("rio.build/node-role", "builder"),
            ],
            json!([
                {"key": "karpenter.k8s.aws/instance-category",
                 "operator": "In", "values": ["c","m","r"]},
                {"key": "kubernetes.io/arch", "operator": "In", "values": ["amd64"]},
            ]),
        );
        let cell = ("gen6-ebs-lo".into(), CapacityType::Spot);
        let nc = mk_probe_nodeclaim(&cell, &pool, "rio-nvme");
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
        // Matched NodePool's template-labels propagated so the Node
        // carries rio.build/hw-band and assertion 4 checks reality.
        assert_eq!(
            v.pointer("/metadata/labels/rio.build~1hw-band")
                .and_then(Value::as_str),
            Some("lo")
        );
        // Requirements are the NodePool's instance-type filters, NOT
        // rio.build/* node-label matches (the bug: those constrain
        // nothing → t3.nano).
        let reqs = v.pointer("/spec/requirements").unwrap().as_array().unwrap();
        assert!(
            reqs.iter()
                .any(|r| r["key"] == "karpenter.k8s.aws/instance-category"
                    && r["values"].as_array().unwrap().len() == 3),
            "{reqs:?}"
        );
        assert!(
            !reqs
                .iter()
                .any(|r| r["key"].as_str().unwrap_or("").starts_with("rio.build/")),
            "rio.build/* labels are stamps, not requirements: {reqs:?}"
        );
        // Capacity-type narrowing appended (Karpenter's label value).
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
