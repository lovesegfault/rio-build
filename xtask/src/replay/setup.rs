//! `cargo xtask replay setup` — make the deployed EKS cluster replay-ready.
//!
//! Replay enablement is owned by this command. It is self-contained and
//! idempotent:
//!
//! 1. **Pre-flight**: the kubeconfig points at an EKS cluster, the
//!    rio-build helm release is deployed, and the replay IAM role exists
//!    in the tofu state. Each refusal names its fix.
//! 2. **Engine image**: `rio-replay:<tree tag>` must exist in ECR; when
//!    missing, exactly that one image is built and pushed (the service
//!    images are already there from the `up --push` that deployed the
//!    chart).
//! 3. **Chart enablement**: `helm upgrade --reuse-values` flips
//!    `replay.enabled=true` (+ `replay.namespace`) on the EXISTING
//!    release — the service deploy's value set is untouched — then the
//!    gateway rollout (config-checksum roll) is waited out.
//! 4. **Namespace bootstrap**: the rio-replay namespace, IRSA-annotated
//!    ServiceAccount, and per-tenant SSH key Secrets — the same
//!    idempotent helpers `replay launch`/`record` call, so a cluster
//!    that ran setup is fully ready before any campaign.
//! 5. **Verify + report**: the CiliumNetworkPolicy admissions and the
//!    gateway build-policy entries are read back from the cluster, and
//!    the next steps (record/launch) are printed.
//!
//! The service deploy (`cargo xtask k8s -p eks up --deploy`) PRESERVES
//! replay enablement but never sets it (see `k8s::eks::deploy`); a
//! data-plane wipe (`up --wipe`) resets the release values, so setup
//! must be re-run after a wipe.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Args;

use super::{NS_REPLAY, TENANT_MATRIX, jobs, launch, preflight};
use crate::config::XtaskConfig;
use crate::k8s::client as kclient;
use crate::k8s::eks::deploy::{DRIFT_SKIP_NODEPOOLS, wait_drift_settled};
use crate::k8s::eks::{Eks, TF_DIR, push};
use crate::k8s::provider::Provider;
use crate::k8s::{NS, NS_STORE, shared};
use crate::{git, helm, tofu, ui};

/// Helm release + chart the rio-build control plane is deployed as
/// (must match `k8s::eks::deploy`).
const RELEASE: &str = "rio";
const CHART: &str = "infra/helm/rio-build";

#[derive(Args)]
pub struct SetupArgs {
    /// Skip the confirmation prompt before enabling replay on the
    /// deployed release (the gateway Deployment rolls when its config
    /// checksum changes). Required for non-interactive runs.
    #[arg(long)]
    pub yes: bool,
    /// Wait for Karpenter node drift to settle after enabling replay
    /// (same semantics as `k8s up --wait-drift`).
    #[arg(long)]
    pub wait_drift: bool,
}

pub async fn run(a: SetupArgs, cfg: &XtaskConfig) -> Result<()> {
    // -- Pre-flight ------------------------------------------------------
    // Replay campaigns are EKS-only (IRSA role, ECR engine image, S3
    // artifacts); the rest of the `replay` family hardcodes the same
    // assumption. Refuse early when the kubeconfig points elsewhere.
    let ctx = kclient::current_context().context(
        "no kubeconfig for the target cluster — run `cargo xtask k8s -p eks up --kubeconfig` \
         first",
    )?;
    ensure!(
        Eks.context_matches(&ctx),
        "replay setup targets EKS and the current kubeconfig context ({ctx:?}) is not an EKS \
         cluster — refresh it with `cargo xtask k8s -p eks up --kubeconfig`. (For local engine \
         runs against k3s use `cargo xtask replay dev`.)"
    );

    // The chart must already be deployed: setup only flips values on an
    // existing release, it never installs one.
    let release = ui::step("rio-build helm release deployed", || async {
        helm::release_status(RELEASE, NS)?.context(
            "helm release \"rio\" is not installed in namespace rio-system — deploy the cluster \
             first: `cargo xtask k8s -p eks up --push --deploy` (or a full `up`), then re-run \
             `cargo xtask replay setup`",
        )
    })
    .await?;
    tracing::info!(
        "release {} ({}, image tag {})",
        release.name,
        release.chart,
        release.image_tag.as_deref().unwrap_or("?")
    );

    // The campaign engine's IRSA role + ECR repo come from tofu
    // (infra/eks/replay.tf); state that predates them must be applied.
    let tf = tofu::outputs(TF_DIR)?;
    let role_arn = tf.get("replay_iam_role_arn").context(
        "the replay IAM role is missing from the tofu state — apply the current infra first: \
         `tofu -chdir=infra/eks apply` (creates the rio-replay IRSA role and ECR repository), \
         then re-run `cargo xtask replay setup`",
    )?;
    let region = tf.get("region")?;

    // -- Engine image in ECR ----------------------------------------------
    // Campaign/recorder Jobs pull rio-replay:<tag> for the CURRENT tree
    // (`replay launch`/`record` compute the same tag). The service
    // images were pushed by `up --push`; rio-replay can be missing when
    // that push predates the replay subsystem — push just this one
    // image instead of demanding a full re-push.
    let tag = git::image_tag(&git::open()?)?;
    if push::in_ecr("rio-replay", &tag, &region).await? {
        ui::step_skip(
            "rio-replay image in ECR",
            &format!("rio-replay:{tag} already pushed"),
        );
    } else {
        tracing::info!("rio-replay:{tag} not in ECR — building and pushing just the engine image");
        ui::step("push rio-replay image", || push::push_single(cfg, "replay")).await?;
    }

    // -- Enable replay on the existing release -----------------------------
    let current_values = helm::get_values(RELEASE, NS)?;
    if replay_currently_enabled(current_values.as_ref()) {
        ui::step_skip(
            "enable replay on the helm release",
            "replay.enabled is already true on the current release",
        );
    } else {
        // Flipping replay.enabled re-renders the gateway config (campaign
        // tenant build-policy defaults) and the config-checksum annotation
        // rolls the gateway Deployment.
        if !a.yes {
            let confirmed = ui::confirm_held(&format!(
                "Enable replay on helm release {RELEASE:?}? (the gateway Deployment will roll)"
            ))?;
            ensure!(confirmed, "setup cancelled (pass --yes to skip the prompt)");
        }
        // Subchart symlinks must exist for helm to render the local chart
        // (same requirement as the service deploy).
        ui::step("chart deps", shared::chart_deps).await?;
        ui::step("helm upgrade rio (replay.enabled=true)", || async {
            // --reuse-values: the release's full value set is owned by the
            // service deploy; setup overlays exactly the two replay values
            // and nothing else. The release is guaranteed to exist by the
            // pre-flight above, so the builder's implied --install can
            // never create one from this partial value set.
            helm::Helm::upgrade_install(RELEASE, CHART)
                .namespace(NS)
                .reuse_values()
                .set("replay.enabled", "true")
                .set("replay.namespace", NS_REPLAY)
                .wait(Duration::from_secs(600))
                .run()
                .await
        })
        .await?;
    }

    // The gateway re-reads its config only via the pod roll; block until
    // the rollout is complete (also confirms gateway health on the
    // already-enabled idempotent path).
    let client = kclient::client().await?;
    kclient::wait_rollout(&client, NS, "rio-gateway", Duration::from_secs(300)).await?;

    // Same drift-settle wait as `k8s up --wait-drift`, run on both the
    // enable and already-enabled paths: recordings/campaigns started on a
    // cluster with Drifted NodeClaims can be evicted mid-run when the
    // disruption controller replaces those nodes.
    if a.wait_drift {
        wait_drift_settled(&client, DRIFT_SKIP_NODEPOOLS).await?;
    }

    // -- Bootstrap the replay namespace resources ---------------------------
    // Same idempotent helpers `replay launch`/`record` call: namespace +
    // IRSA ServiceAccount, then the per-tenant SSH keys (Secret
    // rio-replay-ssh + authorized_keys merge). Campaign tenants
    // themselves (rio-cli create-tenant + upstreams) stay launch-owned —
    // they are data-plane state, not cluster enablement.
    ui::step("rio-replay namespace + ServiceAccount", || {
        jobs::ensure_base(&client, &role_arn)
    })
    .await?;
    ui::step("campaign tenant SSH keys", || {
        launch::ensure_tenant_keys(&client, false)
    })
    .await?;

    // -- Verify -------------------------------------------------------------
    ui::step("verify CiliumNetworkPolicy admissions", || {
        verify_cnp_admissions(&client)
    })
    .await?;
    ui::step("verify gateway build-policy", || {
        verify_build_policy(&client)
    })
    .await?;

    tracing::info!(
        "cluster is replay-ready (engine image rio-replay:{tag}).\n  \
         record an archive:  cargo xtask replay record --eval <HYDRA_EVAL_ID> --scope \
         constituents:<aggregate-job>\n  \
         launch a campaign:  cargo xtask replay launch --eval <HYDRA_EVAL_ID> --mode leaf\n  \
         note: `cargo xtask k8s -p eks up --deploy` preserves replay enablement; `up --wipe` \
         resets it — re-run `cargo xtask replay setup` after a wipe."
    );
    Ok(())
}

/// Whether the release's user-supplied values already carry
/// `replay.enabled: true`. Pure (values JSON in → bool out) so the
/// idempotency decision is unit-testable; `None` (release installed with
/// no user values — cannot happen for a real deploy, but helm prints
/// `null` for it) reads as not-enabled.
fn replay_currently_enabled(values: Option<&serde_json::Value>) -> bool {
    values
        .and_then(|v| v.pointer("/replay/enabled"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

/// Whether one CiliumNetworkPolicy admits the campaign engine ON THE
/// SERVICE'S gRPC PORT: some ingress rule whose `toPorts` contains
/// `grpc_port` carries a `fromEndpoints` entry matching the replay
/// namespace plus the `rio-replay` name label — exactly what the chart
/// renders under `replay.enabled=true`
/// (infra/helm/rio-build/templates/networkpolicy.yaml; helm/27 pins the
/// rendered pairing). Rules are selected by port content first: an
/// admission entry parked on some other rule (e.g. the metrics one)
/// admits nothing the engine dials, so it must NOT satisfy this
/// predicate — Cilium would still drop every gRPC call. Pure so the
/// predicate is unit-testable against chart-shaped JSON.
fn cnp_admits_replay(cnp: &serde_json::Value, grpc_port: &str) -> bool {
    cnp.pointer("/spec/ingress")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter(|rule| {
            rule.get("toPorts")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|tp| tp.get("ports").and_then(|v| v.as_array()))
                .flatten()
                .any(|p| p.get("port").and_then(|v| v.as_str()) == Some(grpc_port))
        })
        .filter_map(|rule| rule.get("fromEndpoints").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|ep| ep.get("matchLabels").and_then(|v| v.as_object()))
        .any(|labels| {
            labels
                .get("k8s:io.kubernetes.pod.namespace")
                .and_then(|v| v.as_str())
                == Some(NS_REPLAY)
                && labels
                    .get("k8s:app.kubernetes.io/name")
                    .and_then(|v| v.as_str())
                    == Some("rio-replay")
        })
}

/// Read back both component-ingress CiliumNetworkPolicies and assert each
/// admits the campaign engine on the port the engine dials
/// ([`crate::k8s::SCHEDULER_GRPC_PORT`] / [`crate::k8s::STORE_GRPC_PORT`]
/// — the same constants the campaign-spec addresses are built from). The
/// chart renders the admissions into scheduler-ingress (rio-system) and
/// store-ingress (rio-store); a miss on either means replay traffic is
/// silently dropped. On the already-enabled idempotent path this
/// read-back is the ONLY check against whatever CNPs an older deployment
/// left behind, which is why it verifies the deployed artifact rather
/// than trusting the release values.
async fn verify_cnp_admissions(client: &kclient::Client) -> Result<()> {
    use ::kube::api::Api;
    use ::kube::core::{ApiResource, DynamicObject, GroupVersionKind};

    use crate::k8s::{SCHEDULER_GRPC_PORT, STORE_GRPC_PORT};

    let gvk = GroupVersionKind::gvk("cilium.io", "v2", "CiliumNetworkPolicy");
    let ar = ApiResource::from_gvk(&gvk);
    for (ns, name, port) in [
        (NS, "scheduler-ingress", SCHEDULER_GRPC_PORT),
        (NS_STORE, "store-ingress", STORE_GRPC_PORT),
    ] {
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
        let cnp = api
            .get_opt(name)
            .await
            .with_context(|| format!("read CiliumNetworkPolicy {ns}/{name}"))?
            .with_context(|| {
                format!(
                    "CiliumNetworkPolicy {ns}/{name} not found — the deployed chart did not \
                     render its network policies"
                )
            })?;
        ensure!(
            cnp_admits_replay(&cnp.data, port),
            "CiliumNetworkPolicy {ns}/{name} has no ingress admission for the campaign engine \
             (namespace {NS_REPLAY}, label rio-replay) on gRPC port {port} — Cilium would \
             silently drop the engine's traffic to this service; check `helm get values \
             {RELEASE} -n {NS}` for replay.enabled and a replay.namespace override, then re-run \
             `cargo xtask replay setup`"
        );
    }
    Ok(())
}

/// Read back the deployed gateway build-policy and assert every campaign
/// tenant has its expected entry — the same check the launch pre-flight
/// runs, so a green setup means launch will not refuse on this account.
async fn verify_build_policy(client: &kclient::Client) -> Result<()> {
    let policy = preflight::read_build_policy(client).await?.context(
        "ConfigMap rio-system/rio-gateway-config (key gateway.toml) not found after the upgrade \
         — the chart did not render the gateway build-policy",
    )?;
    for (tenant, _, force_build_roots) in TENANT_MATRIX {
        preflight::check_build_policy(&policy, tenant, force_build_roots)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn replay_enabled_detection() {
        // Not enabled: no release values, null values, no replay block,
        // explicitly false, or a non-boolean value.
        assert!(!replay_currently_enabled(None));
        assert!(!replay_currently_enabled(Some(&json!(null))));
        assert!(!replay_currently_enabled(Some(&json!({"global": {}}))));
        assert!(!replay_currently_enabled(Some(
            &json!({"replay": {"enabled": false}})
        )));
        assert!(!replay_currently_enabled(Some(
            &json!({"replay": {"enabled": "true"}})
        )));
        // Enabled — what setup itself writes.
        assert!(replay_currently_enabled(Some(
            &json!({"replay": {"enabled": true, "namespace": "rio-replay"}})
        )));
    }

    /// `spec` of the scheduler-ingress CNP, captured verbatim from the
    /// producer: `helm template rio . --set global.image.tag=test --set
    /// replay.enabled=true` (yq'd to JSON) — terminators, sibling
    /// entries, metrics rule and all. Hand-trimming the fixture is how a
    /// predicate quietly ends up tested against a shape the chart never
    /// renders.
    fn scheduler_ingress_rendered() -> serde_json::Value {
        json!({
            "endpointSelector": {"matchLabels": {"app.kubernetes.io/name": "rio-scheduler"}},
            "ingress": [
                {
                    "fromEndpoints": [
                        {"matchLabels": {"app.kubernetes.io/name": "rio-gateway"}},
                        {"matchLabels": {"app.kubernetes.io/name": "rio-controller"}},
                        {"matchLabels": {"gateway.networking.k8s.io/gateway-name": "rio-dashboard"}},
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-builder"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-fetcher"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {
                                "k8s:io.kubernetes.pod.namespace": "rio-replay",
                                "k8s:app.kubernetes.io/name": "rio-replay"
                            }
                        }
                    ],
                    "toPorts": [{"ports": [{"port": "9001", "protocol": "TCP"}]}]
                },
                {
                    "fromEntities": ["host", "remote-node", "cluster"],
                    "toPorts": [{"ports": [{"port": "9091", "protocol": "TCP"}]}]
                }
            ]
        })
    }

    /// `spec` of the store-ingress CNP from the same render.
    fn store_ingress_rendered() -> serde_json::Value {
        json!({
            "endpointSelector": {"matchLabels": {"app.kubernetes.io/name": "rio-store"}},
            "ingress": [
                {
                    "fromEndpoints": [
                        {"matchLabels": {"k8s:io.kubernetes.pod.namespace": "rio-system"}},
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-builder"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-fetcher"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {
                                "k8s:io.kubernetes.pod.namespace": "rio-replay",
                                "k8s:app.kubernetes.io/name": "rio-replay"
                            }
                        }
                    ],
                    "toPorts": [{"ports": [{"port": "9002", "protocol": "TCP"}]}]
                },
                {
                    "fromEntities": ["host", "remote-node", "cluster"],
                    "toPorts": [{"ports": [{"port": "9092", "protocol": "TCP"}]}]
                }
            ]
        })
    }

    #[test]
    fn cnp_admission_predicate_matches_chart_render() {
        // Both rendered policies admit the engine on their gRPC port —
        // the predicate must find the entry among the sibling admissions.
        let scheduler = json!({"spec": scheduler_ingress_rendered()});
        let store = json!({"spec": store_ingress_rendered()});
        assert!(cnp_admits_replay(&scheduler, "9001"));
        assert!(cnp_admits_replay(&store, "9002"));

        // Wiring the verifier to the wrong service's port must fail even
        // against a fully correct render: the admission exists, but not
        // on the demanded port.
        assert!(!cnp_admits_replay(&scheduler, "9002"));
        assert!(!cnp_admits_replay(&store, "9001"));
    }

    #[test]
    fn cnp_admission_rejects_admission_on_non_grpc_rule() {
        // The discriminating case for port scoping: the rio-replay
        // fromEndpoints entry exists, but parked on the METRICS rule
        // (port 9091) while the gRPC rule (9001) carries only the
        // always-present admissions. A labels-only scan that flattens
        // every rule's fromEndpoints accepts this policy — yet Cilium
        // drops all engine gRPC, the exact silent failure the read-back
        // exists to catch (e.g. a divergent older deployment on setup's
        // idempotent no-upgrade path).
        let mut mis_paired = scheduler_ingress_rendered();
        let rules = mis_paired["ingress"].as_array_mut().unwrap();
        let admission = rules[0]["fromEndpoints"]
            .as_array_mut()
            .unwrap()
            .pop()
            .unwrap();
        rules[1]["fromEndpoints"] = json!([admission]);
        let mis_paired = json!({"spec": mis_paired});
        assert!(
            !cnp_admits_replay(&mis_paired, "9001"),
            "an admission on a non-gRPC rule admits nothing the engine dials"
        );

        // Same shape with the admission in place still passes — proves
        // the rejection above is the port pairing, not fixture damage.
        assert!(cnp_admits_replay(
            &json!({"spec": scheduler_ingress_rendered()}),
            "9001"
        ));
    }

    #[test]
    fn cnp_admission_predicate_rejects_weaker_shapes() {
        // replay.enabled=false: no admission entry anywhere.
        let disabled = json!({
            "spec": {
                "ingress": [
                    {
                        "fromEndpoints": [
                            {"matchLabels": {"k8s:io.kubernetes.pod.namespace": "rio-system"}}
                        ],
                        "toPorts": [{"ports": [{"port": "9001", "protocol": "TCP"}]}]
                    }
                ]
            }
        });
        assert!(!cnp_admits_replay(&disabled, "9001"));

        // The namespace label alone is not enough — the chart's admission
        // pairs it with the engine's name label, and the predicate must
        // demand both (a namespace-wide hole would be a policy bug). The
        // rule carries the right port so the rejection below is about
        // the labels, not the port filter.
        let ns_only = json!({
            "spec": {
                "ingress": [
                    {
                        "fromEndpoints": [
                            {"matchLabels": {"k8s:io.kubernetes.pod.namespace": "rio-replay"}}
                        ],
                        "toPorts": [{"ports": [{"port": "9001", "protocol": "TCP"}]}]
                    }
                ]
            }
        });
        assert!(!cnp_admits_replay(&ns_only, "9001"));

        // Degenerate shapes never panic and never pass.
        assert!(!cnp_admits_replay(&json!({}), "9001"));
        assert!(!cnp_admits_replay(
            &json!({"spec": {"ingress": []}}),
            "9001"
        ));
    }
}
