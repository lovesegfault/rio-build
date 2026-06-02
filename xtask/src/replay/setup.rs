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
use crate::k8s::{NS, shared};
use crate::{git, helm, tofu, ui};

/// Helm release + chart the rio-build control plane is deployed as
/// (must match `k8s::eks::deploy`). `pub(super)` so the CNP read-back's
/// remediation hint (`preflight::verify_cnp_admissions`) names the real
/// release instead of hand-copying it.
pub(super) const RELEASE: &str = "rio";
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
    if replay_values_current(current_values.as_ref()) {
        ui::step_skip(
            "enable replay on the helm release",
            "replay.enabled=true with replay.namespace at its required value on the current \
             release",
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
    // Same artifact read-back the launch pre-flight runs: the deployed
    // CNPs must admit the engine on the gRPC ports it dials.
    ui::step("verify CiliumNetworkPolicy admissions", || {
        preflight::verify_cnp_admissions(&client)
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

/// Whether the release's user-supplied values already carry exactly the
/// replay enablement setup writes: `replay.enabled: true` AND
/// `replay.namespace` equal to [`NS_REPLAY`]. Checking the namespace too
/// is what makes "re-run `cargo xtask replay setup`" a real remediation
/// for a drifted override (e.g. a raw `helm upgrade --set
/// replay.namespace=…`): on the skip path setup would only re-DETECT the
/// drift in its CNP read-back, while this predicate routes it through
/// the corrective upgrade that writes the value back. A missing
/// namespace key also re-upgrades — the chart default happens to match,
/// but converging the release to the explicitly written pair keeps the
/// skip decision trivial. Pure (values JSON in → bool out) so the
/// idempotency decision is unit-testable; `None` (release installed with
/// no user values — cannot happen for a real deploy, but helm prints
/// `null` for it) reads as not-enabled.
fn replay_values_current(values: Option<&serde_json::Value>) -> bool {
    let enabled = values
        .and_then(|v| v.pointer("/replay/enabled"))
        .and_then(|v| v.as_bool())
        == Some(true);
    let namespace_current = values
        .and_then(|v| v.pointer("/replay/namespace"))
        .and_then(|v| v.as_str())
        == Some(NS_REPLAY);
    enabled && namespace_current
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
    fn replay_values_current_detection() {
        // Upgrade required: no release values, null values, no replay
        // block, explicitly disabled, or a non-boolean enabled value.
        assert!(!replay_values_current(None));
        assert!(!replay_values_current(Some(&json!(null))));
        assert!(!replay_values_current(Some(&json!({"global": {}}))));
        assert!(!replay_values_current(Some(
            &json!({"replay": {"enabled": false, "namespace": "rio-replay"}})
        )));
        assert!(!replay_values_current(Some(
            &json!({"replay": {"enabled": "true", "namespace": "rio-replay"}})
        )));
        // Upgrade required even though enabled: a drifted or missing
        // replay.namespace must route through the corrective upgrade,
        // not the skip path — skipping would leave CNPs admitting a
        // namespace the engine never runs in, with only the read-back
        // left to (repeatedly) detect it.
        assert!(!replay_values_current(Some(
            &json!({"replay": {"enabled": true, "namespace": "my-replay"}})
        )));
        assert!(!replay_values_current(Some(
            &json!({"replay": {"enabled": true}})
        )));
        // Current — exactly the pair setup itself writes.
        assert!(replay_values_current(Some(
            &json!({"replay": {"enabled": true, "namespace": NS_REPLAY}})
        )));
    }
}
