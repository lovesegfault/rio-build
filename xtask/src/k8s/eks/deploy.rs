//! helm upgrade from the working tree.
//!
//! Reads infra values from tofu outputs, image tag from .rio-image-tag
//! (written by `eks push`). No git roundtrip — chart changes on a dirty
//! tree deploy directly.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::info;

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::provider::ProviderKind;
use crate::k8s::{NS, ensure_namespaces, shared, status};
use crate::sh::repo_root;
use crate::{helm, kube, tofu, ui};

pub async fn run(
    cfg: &XtaskConfig,
    log_level: &str,
    tenant: Option<&str>,
    skip_preflight: bool,
) -> Result<()> {
    let tag = std::fs::read_to_string(repo_root().join(".rio-image-tag"))
        .context("no .rio-image-tag — run `cargo xtask k8s push -p eks` first")?;
    let tag = tag.trim();

    let tf = tofu::outputs(TF_DIR)?;
    let ecr = tf.get("ecr_registry")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let store_arn = tf.get("store_iam_role_arn")?;
    let scheduler_arn = tf.get("scheduler_iam_role_arn")?;
    let bootstrap_arn = tf.get("bootstrap_iam_role_arn")?;
    let db_arn = tf.get("db_secret_arn")?;
    let db_host = tf.get("db_endpoint")?;
    let region = tf.get("region")?;
    let cluster = tf.get("cluster_name")?;
    let node_role = tf.get("karpenter_node_role_name")?;

    info!("deploy tag={tag} registry={ecr} cluster={cluster}");

    let client = kube::client().await?;

    // Preflight: bail early if the cluster is in a state where helm
    // upgrade will likely wedge (IP-starved subnets, stuck NodeClaims,
    // pending-upgrade from a prior failed deploy). Cheap compared to
    // the helm --wait timeout. Bypass: --skip-preflight.
    if !skip_preflight {
        let ctx = kube::current_context().unwrap_or_default();
        let report = ui::step("preflight", || async {
            Ok::<_, anyhow::Error>(status::gather(&client, ctx, ProviderKind::Eks).await)
        })
        .await?;
        status::preflight_check(&report)?;
    }

    // CRDs first, server-side apply.
    ui::step("apply CRDs", || kube::apply_crds(&client)).await?;

    // NodeOverlay CRD comes from the Karpenter chart (terraform-managed).
    // The rio chart renders a NodeOverlay CR — helm install fails with
    // "no matches for kind" if the CRD hasn't established yet.
    kube::wait_crd_established(
        &client,
        "nodeoverlays.karpenter.sh",
        Duration::from_secs(120),
    )
    .await?;

    // Namespaces first. Created here (not by the chart —
    // namespace.create=false below) because: (a) the SSH Secret must
    // exist before helm runs; (b) Helm refuses to adopt a namespace it
    // didn't create. ADR-019 four-namespace split: control plane +
    // store at baseline, builders + fetchers at privileged (SYS_ADMIN
    // for FUSE).
    ui::step("namespaces + ssh secret", || async {
        ensure_namespaces(&client).await?;
        shared::ensure_gateway_ssh_secret(&client, cfg, tenant).await
    })
    .await?;

    // security-profiles-operator: vendored static manifest (see
    // infra/k8s/README.md). Applied before the rio chart so the
    // SeccompProfile CRD is established when seccomp-profiles.yaml
    // renders. cert-manager (tofu-managed, addons.tf) is its
    // prerequisite — already running by the time xtask deploy runs.
    //
    // kubectl apply (not kube-rs SSA): the manifest has 8 CRDs +
    // RBAC + Deployment + webhook in one stream. kubectl's
    // multi-doc apply handles ordering and field-manager conflicts;
    // re-implementing that in kube-rs is not worth it for one file.
    ui::step("apply security-profiles-operator", || async {
        let sh = crate::sh::shell()?;
        let manifest = repo_root().join("infra/k8s/security-profiles-operator.yaml");
        crate::sh::run(xshell::cmd!(
            sh,
            "kubectl apply --server-side --force-conflicts -f {manifest}"
        ))
        .await?;
        // The chart's seccomp-profiles.yaml renders SeccompProfile +
        // SecurityProfilesOperatorDaemon CRs. helm fails with "no
        // matches for kind" if those CRDs haven't established.
        kube::wait_crd_established(
            &client,
            "seccompprofiles.security-profiles-operator.x-k8s.io",
            Duration::from_secs(60),
        )
        .await?;
        kube::wait_crd_established(
            &client,
            "securityprofilesoperatordaemons.security-profiles-operator.x-k8s.io",
            Duration::from_secs(60),
        )
        .await?;
        // The operator creates a default `spod` SecurityProfilesOperatorDaemon
        // on startup; helm can't adopt operator-created resources. Apply the
        // scheduling override (constrain spod DS to builder/fetcher nodes,
        // disable bpfrecorder) via SSA AFTER the operator has created it.
        // Poll for existence — operator's first reconcile creates it.
        // `--for=create` (kubectl 1.31+) actually polls; `--for=jsonpath`
        // fails immediately with NotFound if the resource is absent (the
        // previous form raced the operator's ~5s startup on fresh clusters).
        crate::sh::run(xshell::cmd!(
            sh,
            "kubectl wait --for=create securityprofilesoperatordaemon/spod -n security-profiles-operator --timeout=60s"
        ))
        .await?;
        let spod_cfg = repo_root().join("infra/k8s/spod-config.yaml");
        crate::sh::run(xshell::cmd!(
            sh,
            "kubectl apply --server-side --force-conflicts -f {spod_cfg}"
        ))
        .await
    })
    .await?;

    // JWT keypair: mint-or-read. If `rio-jwt-signing` Secret exists,
    // reuse its seed (idempotent across deploys). Otherwise generate
    // fresh. Seed never touches disk or source — passes via --set,
    // lives only in process memory + the helm release secret (same
    // trust boundary as the rendered Secret). Derives pubkey here so
    // operators don't have to compute it offline.
    let (jwt_seed_b64, jwt_pubkey_b64) = jwt_keypair(&client).await?;

    // Subchart symlink (same requirement as dev apply).
    ui::step("chart deps", || async { crate::k8s::shared::chart_deps() }).await?;

    // NLB annotations (previously a --set-json one-liner in bash).
    let nlb_ann = json!({
        "service.beta.kubernetes.io/aws-load-balancer-type": "external",
        "service.beta.kubernetes.io/aws-load-balancer-nlb-target-type": "ip",
        "service.beta.kubernetes.io/aws-load-balancer-scheme": "internal",
        // P0542: dualstack NLB so the gateway has both A and AAAA.
        // v6-only pods inside the cluster reach it via the Service
        // ClusterIP anyway; this is for the SSM-tunnel client side.
        "service.beta.kubernetes.io/aws-load-balancer-ip-address-type": "dualstack",
        "service.beta.kubernetes.io/aws-load-balancer-attributes": "load_balancing.cross_zone.enabled=true",
        "service.beta.kubernetes.io/aws-load-balancer-listener-attributes.TCP-22": "tcp.idle_timeout.seconds=3600",
    });

    ui::step("helm upgrade rio", || async {
        helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
            .namespace(NS)
            .set("namespace.create", "false")
            .set("global.image.registry", &ecr)
            .set("global.image.tag", tag)
            .set("global.region", &region)
            .set("global.logLevel", log_level)
            .set("store.chunkBackend.bucket", &bucket)
            .set("scheduler.logS3Bucket", &bucket)
            .set(
                r"store.serviceAccount.annotations.eks\.amazonaws\.com/role-arn",
                &store_arn,
            )
            .set(
                r"scheduler.serviceAccount.annotations.eks\.amazonaws\.com/role-arn",
                &scheduler_arn,
            )
            .set("externalSecrets.enabled", "true")
            .set("externalSecrets.auroraSecretArn", &db_arn)
            .set("externalSecrets.auroraEndpoint", &db_host)
            .set("bootstrap.enabled", "true")
            .set(
                r"bootstrap.serviceAccount.annotations.eks\.amazonaws\.com/role-arn",
                &bootstrap_arn,
            )
            .set_json("gateway.service.annotations", nlb_ann.to_string())
            .set("karpenter.enabled", "true")
            .set("karpenter.clusterName", &cluster)
            .set("karpenter.nodeRoleName", &node_role)
            .set("builderPoolDefaults.enabled", "true")
            // I-108: EKS supports both arches via Karpenter (rio-builder-
            // preferred is c6a/c7a x86; rio-builder-fallback covers
            // Graviton). Chart default `builderPools` is x86-64 only;
            // override to provision both. Per-pool overrides go here
            // (e.g. lower aarch64 max); everything else inherits from
            // builderPoolDefaults.
            .set_json(
                "builderPools",
                r#"[{"name":"x86-64","systems":["x86_64-linux"]},{"name":"aarch64","systems":["aarch64-linux"],"replicas":{"min":1,"max":100}}]"#,
            )
            // P0452 hard-split: SMOKE_EXPR's builtin:fetchurl FOD routes
            // to FetcherPool only. Without this, the FOD queues forever
            // (scheduler never sends a FOD to a builder per ADR-019).
            .set("fetcherPool.enabled", "true")
            // P0541: ephemeral fetchers (one Job per FOD). Chart default
            // is false (preserves existing STS pools); EKS opts in. The
            // CRD's CEL requires replicas.min==0 for ephemeral (no
            // standing set).
            .set("fetcherPool.ephemeral", "true")
            .set("fetcherPool.replicas.min", "0")
            // I-054: JWT enables per-tenant upstream substitution
            // (cache.nixos.org). Keypair minted/read by jwt_keypair().
            // I-105: ephemeral builders' FUSE-warm burst (~800
            // GetPath/builder × 100 builders) exhausts a single store's
            // PG pool. 23985e4d dropped this after I-078 was fixed, but
            // ephemeral mode is a different load pattern. The concurrent-
            // migration race (values.yaml:207) is mitigated by the
            // bootstrap Job running migrations before store starts.
            .set("store.replicas", "4")
            .set("jwt.enabled", "true")
            .set("jwt.signingSeed", &jwt_seed_b64)
            .set("jwt.publicKey", &jwt_pubkey_b64)
            .wait(Duration::from_secs(600))
            .run()
    })
    .await
}

/// Read existing JWT signing seed from `rio-jwt-signing` Secret, or
/// mint a fresh one. Returns `(seed_b64, pubkey_b64)` ready for
/// `--set jwt.signingSeed=` / `jwt.publicKey=`.
///
/// Idempotent: re-deploys reuse the existing seed so in-flight JWTs
/// stay valid across `xtask k8s deploy` runs. Rotation = `kubectl
/// delete secret rio-jwt-signing` then redeploy.
async fn jwt_keypair(client: &::kube::Client) -> anyhow::Result<(String, String)> {
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use k8s_openapi::api::core::v1::Secret;

    let b64 = base64::engine::general_purpose::STANDARD;
    let api: ::kube::Api<Secret> = ::kube::Api::namespaced(client.clone(), NS);

    let seed: [u8; 32] = match api.get_opt("rio-jwt-signing").await? {
        Some(s) => {
            // Secret.data is already base64-decoded by kube-rs (ByteString).
            // The chart stores the OPERATOR's b64 string as the value (see
            // jwt-signing-secret.yaml `b64enc` of an already-b64 input),
            // so decode once more.
            let inner_b64 = s
                .data
                .as_ref()
                .and_then(|d| d.get("ed25519_seed"))
                .map(|b| std::str::from_utf8(&b.0).map(|s| s.to_owned()))
                .ok_or_else(|| anyhow::anyhow!("rio-jwt-signing Secret missing ed25519_seed"))??;
            let bytes: Vec<u8> = b64.decode(inner_b64.trim())?;
            bytes
                .try_into()
                .map_err(|v: Vec<u8>| anyhow::anyhow!("seed is {} bytes, want 32", v.len()))?
        }
        None => SigningKey::generate(&mut ssh_key::rand_core::OsRng).to_bytes(),
    };

    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key();
    Ok((b64.encode(seed), b64.encode(pk.to_bytes())))
}
