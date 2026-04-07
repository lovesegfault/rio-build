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

/// Scheduler `[[size_classes]]` config — names + cutoffs MUST match
/// `builderPoolSetDefaults.classes` in values.yaml. `memLimitBytes` ≈ the
/// class's `resources.limits.memory` (the bump threshold: a build
/// whose EMA peak exceeds it routes to the next class up).
/// `cpuLimitCores` mirrors `requests.cpu` for the same reason.
const SIZE_CLASSES_JSON: &str = r#"[
  {"name":"tiny","cutoffSecs":30,"memLimitBytes":4294967296,"cpuLimitCores":2.0},
  {"name":"small","cutoffSecs":120,"memLimitBytes":17179869184,"cpuLimitCores":8.0},
  {"name":"medium","cutoffSecs":600,"memLimitBytes":68719476736,"cpuLimitCores":32.0},
  {"name":"large","cutoffSecs":1800,"memLimitBytes":137438953472,"cpuLimitCores":64.0},
  {"name":"xlarge","cutoffSecs":7200,"memLimitBytes":274877906944,"cpuLimitCores":128.0}
]"#;

/// `builderPoolSets[]` (helm list — replaced wholesale, hence one const).
/// Per-arch general sets use `builderPoolSetDefaults.classes` (5-tier).
/// Per-arch kvm sets override `classes` to a single xlarge bucket (NixOS
/// VM tests are uniformly heavy — no point tiering) and `poolTemplate.
/// features` to the standard nixpkgs VM-test feature triple. The
/// controller derives the metal nodeSelector, rio.build/kvm toleration,
/// and rio.build/kvm resource from `features:[kvm]`
/// (`r[ctrl.builderpool.kvm-device]`) — NOT set here.
///
/// Validate end-to-end after deploy by building a derivation with
/// `requiredSystemFeatures = ["kvm" "nixos-test"]` through the gateway:
///   cargo xtask k8s -p eks rsb -- nix build -L --store ssh-ng://… \
///     'nixpkgs#nixosTests.simple'
/// Scheduler routes it to the `{arch}-kvm-xlarge` pool; Karpenter
/// provisions a `.metal` node; pod gets `/dev/kvm` via containerd
/// base_runtime_spec (rio.build/kvm is scheduling-signal only).
const BUILDER_POOL_SETS_JSON: &str = r#"[
  {"name":"x86-64","systems":["x86_64-linux"]},
  {"name":"aarch64","systems":["aarch64-linux"]},
  {"name":"x86-64-kvm","systems":["x86_64-linux"],
   "poolTemplate":{"features":["kvm","nixos-test","big-parallel"]},
   "classes":[{"name":"xlarge","cutoffSecs":7200,"maxConcurrent":10,
     "resources":{"requests":{"cpu":"128","memory":"128Gi","ephemeral-storage":"62Gi"},
                  "limits":{"cpu":"128","memory":"256Gi","ephemeral-storage":"96Gi"}}}]},
  {"name":"aarch64-kvm","systems":["aarch64-linux"],
   "poolTemplate":{"features":["kvm","nixos-test","big-parallel"]},
   "classes":[{"name":"xlarge","cutoffSecs":7200,"maxConcurrent":10,
     "resources":{"requests":{"cpu":"128","memory":"128Gi","ephemeral-storage":"62Gi"},
                  "limits":{"cpu":"128","memory":"256Gi","ephemeral-storage":"96Gi"}}}]}
]"#;

/// One FetcherPool per arch — `system="builtin"` FODs overflow to
/// either (every executor advertises `builtin`; `best_executor` scores
/// across the union). nodeSelector REPLACES the reconciler default, so
/// `rio.build/node-role: fetcher` is repeated. Image stays the ECR ref
/// from `fetcherPoolDefaults.image` (PLAN-PREBAKE: layer-cache-warm,
/// not digest-pin).
const FETCHER_POOLS_JSON: &str = r#"[
  {"name":"x86-64","systems":["x86_64-linux","builtin"],
   "nodeSelector":{"rio.build/node-role":"fetcher","kubernetes.io/arch":"amd64"}},
  {"name":"aarch64","systems":["aarch64-linux","builtin"],
   "nodeSelector":{"rio.build/node-role":"fetcher","kubernetes.io/arch":"arm64"}}
]"#;

pub async fn run(
    cfg: &XtaskConfig,
    log_level: &str,
    tenant: Option<&str>,
    skip_preflight: bool,
    no_hooks: bool,
) -> Result<()> {
    let tag = std::fs::read_to_string(repo_root().join(".rio-image-tag"))
        .context("no .rio-image-tag — run `cargo xtask k8s -p eks up --push` first")?;
    let tag = tag.trim();

    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;

    // ADR-021: NixOS node AMI is the only EC2NodeClass. I-182: resolve
    // the content-addressed `rio.build/ami` tag from EC2 (newest image
    // tagged `rio.build/ami-latest=true`, written by `up --ami`) — NOT
    // from the gitignored per-worktree `.rio-ami-tag` file. A worktree
    // that never ran `up --ami` previously deployed whatever stale tag
    // was on disk (or recomputed a drvPath-hash with no backing AMI).
    // EC2 is the source of truth for "what's actually registered".
    // assert_registered then confirms BOTH arches exist for that tag —
    // a half-uploaded set (interrupted `up --ami`) wedges Karpenter.
    let ami_tag = super::ami::resolve_latest_tag(&region).await?;
    let ami_tag = ami_tag.as_str();
    super::ami::assert_registered(ami_tag, &region).await?;

    let ecr = tf.get("ecr_registry")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let store_arn = tf.get("store_iam_role_arn")?;
    let scheduler_arn = tf.get("scheduler_iam_role_arn")?;
    let bootstrap_arn = tf.get("bootstrap_iam_role_arn")?;
    let db_arn = tf.get("db_secret_arn")?;
    let db_host = tf.get("db_endpoint")?;
    let cluster = tf.get("cluster_name")?;
    let node_role = tf.get("karpenter_node_role_name")?;

    info!("deploy tag={tag} ami={ami_tag} registry={ecr} cluster={cluster}");

    let client = kube::client().await?;

    // Preflight: bail early if the cluster is in a state where helm
    // upgrade will likely wedge (IP-starved subnets, stuck NodeClaims,
    // pending-upgrade from a prior failed deploy). Cheap compared to
    // the helm --wait timeout. Bypass: --deploy-skip-preflight.
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

    // ADR-021: SPO replaced by AMI-baked seccomp profiles (nix/nixos-node/
    // hardening.nix tmpfiles) — profiles are on disk before kubelet starts,
    // so no spod DS, no wait-seccomp init. The chart's seccomp-profiles.yaml
    // (SeccompProfile CRs) is gated on securityProfilesOperator.enabled,
    // which we set false below alongside controller.seccompPreinstalled=
    // true. To remove a leftover SPO from a pre-cutover cluster:
    //   kubectl delete -f infra/k8s/security-profiles-operator.yaml --ignore-not-found

    // JWT keypair: mint-or-read. If `rio-jwt-signing` Secret exists,
    // reuse its seed (idempotent across deploys). Otherwise generate
    // fresh. Seed never touches disk or source — passes via --set,
    // lives only in process memory + the helm release secret (same
    // trust boundary as the rendered Secret). Derives pubkey here so
    // operators don't have to compute it offline.
    let (jwt_seed_b64, jwt_pubkey_b64) = jwt_keypair(&client).await?;

    // Subchart symlink (same requirement as dev apply).
    ui::step("chart deps", crate::k8s::shared::chart_deps).await?;

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
            .set("karpenter.amiTag", ami_tag)
            // I-117: BuilderPoolSet supersedes the flat builderPools[] —
            // five size classes (tiny..xlarge, chart default) with
            // per-class resource requests. The scheduler's classify()
            // routes each derivation to the smallest covering class by
            // (est_duration, peak_memory) so a 50MB hello build gets a
            // 2-core/4Gi pod and llvm gets 128-core/256Gi. Ephemeral
            // children: one Job per build, sized by class. Karpenter
            // bin-packs across c6a.large..c6a.32xlarge.
            //
            // I-117b: one BuilderPoolSet per arch (same I-108 list/
            // defaults split). Both arches get adaptive sizing — child
            // pools are `x86-64-tiny`..`x86-64-xlarge` + `aarch64-tiny`
            // ..`aarch64-xlarge`. Each child's `systems` propagates from
            // its BPS, so I-098's kubernetes.io/arch nodeSelector lands
            // arm pods on arm nodes. The scheduler's hard_filter checks
            // BOTH systems AND size_class, so an aarch64 drv routes to
            // aarch64-medium, never x86-64-medium.
            //
            // builderPoolDefaults stays the poolTemplate base (seccomp,
            // tolerations, nodeSelector, hostUsers — deep-merged in the
            // chart). enabled=false stops the flat builderpool.yaml
            // template from ALSO rendering. builderPools=[] drops the
            // chart-default x86-64 flat pool (BPS handles both arches).
            .set("builderPoolDefaults.enabled", "false")
            .set_json("builderPools", "[]")
            .set("builderPoolSetDefaults.enabled", "true")
            // I-186: hostUsers:false breaks FUSE passthrough
            // (FUSE_DEV_IOC_BACKING_OPEN needs init-userns
            // CAP_SYS_ADMIN) and fusectl mount (I-165b). Passthrough
            // is critical for compile-heavy builds. Stay on
            // hostUsers:true until P0560 (EROFS+fscache) deletes FUSE
            // — EROFS warm reads are page-cache-native, no passthrough
            // dependency. ADR-012 userns isolation deferred to then.
            // The NixOS AMI's containerd cgroup_writable=true is in
            // place (nix/nixos-node/eks-node.nix) so the flip is a
            // one-line revert here when P0560 lands.
            .set_json("builderPoolSets", BUILDER_POOL_SETS_JSON)
            // scheduler.sizeClasses MUST agree with builderPoolSetDefaults.
            // classes (names + cutoffs). memLimitBytes ≈ the class's
            // resources.limits.memory — a build whose EMA peak exceeds
            // it bumps to the next class even if duration fits.
            .set_json("scheduler.sizeClasses", SIZE_CLASSES_JSON)
            // P0452 hard-split: SMOKE_EXPR's builtin:fetchurl FOD routes
            // to FetcherPool only. Without this, the FOD queues forever
            // (scheduler never sends a FOD to a builder per ADR-019).
            // One pool per arch; builtin FODs overflow to whichever
            // has capacity.
            .set_json("fetcherPools", FETCHER_POOLS_JSON)
            .set("fetcherPoolDefaults.enabled", "true")
            // I-054: JWT enables per-tenant upstream substitution
            // (cache.nixos.org). Keypair minted/read by jwt_keypair().
            // I-128: store.replicas was a fixed "8" here (I-105
            // mitigation — ephemeral builders' FUSE-warm burst exhausts
            // a single store's PG pool). ComponentScaler now scales
            // store 2..14 from Σ(queued+running)/learnedRatio, corrected
            // against max(GetLoad). store.replicas is IGNORED when the
            // scaler is enabled (store.yaml omits .spec.replicas).
            .set("componentScaler.store.enabled", "true")
            // I-171: was 200 (sized for 16-ACU Aurora). At min 0.5 ACU
            // (~105 conns) with ComponentScaler max=14 replicas, 200×14
            // = 2800 saturates; even 2×200=400 does. 20 + idle_timeout
            // (rio-store/src/main.rs) keeps steady-state under budget.
            // 14×20=280 can still burst-saturate — TODO(P-new): bump
            // Aurora min_capacity OR cap componentScaler.store.max
            // against (rds_max_conns / pgMaxConnections).
            .set("store.pgMaxConnections", "20")
            // I-147/I-150: production-scale resources. values.yaml defaults
            // stay small so VM-test k3s (2-node QEMU) can schedule; EKS
            // gets the real sizing here.
            .set("controller.resources.requests.cpu", "8")
            .set("controller.resources.requests.memory", "8Gi")
            .set("controller.resources.limits.memory", "64Gi")
            .set("store.resources.requests.cpu", "16")
            .set("store.resources.requests.memory", "8Gi")
            .set("store.resources.limits.memory", "32Gi")
            .set("scheduler.resources.requests.cpu", "32")
            .set("scheduler.resources.requests.memory", "16Gi")
            .set("scheduler.resources.limits.memory", "64Gi")
            .set("gateway.resources.requests.cpu", "32")
            .set("gateway.resources.requests.memory", "16Gi")
            .set("gateway.resources.limits.memory", "64Gi")
            .set("fetcherDefaults.resources.requests.cpu", "2")
            .set("fetcherDefaults.resources.limits.memory", "4Gi")
            .set("jwt.enabled", "true")
            .set("jwt.signingSeed", &jwt_seed_b64)
            .set("jwt.publicKey", &jwt_pubkey_b64)
            // P0539a: ServiceMonitor/PodMonitor/PrometheusRule. CRDs come
            // from kube-prometheus-stack (infra/eks/monitoring.tf), which
            // tofu apply lands before this runs.
            .set("monitoring.enabled", "true")
            // ADR-021: NixOS AMI bakes the Localhost seccomp profiles
            // (nix/nixos-node/hardening.nix) — no SPO, no wait-seccomp
            // init. SeccompProfile CRs (templates/seccomp-profiles.yaml)
            // are gated on securityProfilesOperator.enabled.
            .set("securityProfilesOperator.enabled", "false")
            .set("controller.seccompPreinstalled", "true")
            .wait(Duration::from_secs(600))
            // AMI bring-up chicken-and-egg: the chart's post-install
            // hook smoke-tests through the gateway, which needs working
            // builder nodes, which need a validated AMI, which is what
            // we're trying to deploy to test. --deploy-no-hooks skips
            // the hook so the chart lands; operator runs `k8s smoke`
            // manually once nodes are up.
            .no_hooks(no_hooks)
            .run()
    })
    .await?;

    // P0539e: helm --wait returns when PODS are Ready, but the NLB's
    // target registration + health-check round lags ~30-90s behind.
    // A follow-up `rsb` in that window connects to a TG with zero
    // healthy backends → SSM forwards to nothing → russh sees the
    // bastion's "no route" as garbage → "unexpected packet type 80".
    // Block until ≥1 target is healthy so deploy → rsb is race-free.
    ui::step("NLB target health", || {
        super::smoke::wait_any_target_healthy(&region)
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
