//! helm upgrade from the working tree.
//!
//! Reads infra values from tofu outputs, image tag recomputed from git
//! (content-addressed; matches what `eks push` computed for the same
//! tree state). No git roundtrip — chart changes on a dirty tree deploy
//! directly.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{info, warn};

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::client as kube;
use crate::k8s::provider::{DeployOpts, ProviderKind};
use crate::k8s::{NS, ensure_namespaces, shared, status};
use crate::{git, helm, tofu, ui};

/// `pools[]` (helm list — replaced wholesale, hence one const).
/// Per-arch general builder pools, per-arch kvm builder pools (NixOS
/// VM tests; the controller derives the metal nodeSelector +
/// `rio.build/kvm` toleration from `features:[kvm]` —
/// `r[ctrl.pool.kvm-device]`), and per-arch fetcher pools.
///
/// Per-pod (cores, mem, disk) come from the scheduler's per-drv
/// SpawnIntent (ADR-023) — there is NO per-pool resources knob.
///
/// Fetcher entries: `system="builtin"` FODs overflow to either arch;
/// nodeSelector REPLACES the reconciler default. CEL forbids
/// privileged/hostUsers/seccompProfile on Fetcher entries — those
/// fields are deep-merged from poolDefaults but rejected at admission;
/// the `seccompProfile:null` clears prevent the merge.
const POOLS_JSON: &str = r#"[
  {"name":"x86-64","kind":"Builder","systems":["x86_64-linux","i686-linux"]},
  {"name":"aarch64","kind":"Builder","systems":["aarch64-linux"]},
  {"name":"x86-64-kvm","kind":"Builder","systems":["x86_64-linux","i686-linux"],
   "features":["kvm","nixos-test","big-parallel"],"maxConcurrent":10},
  {"name":"aarch64-kvm","kind":"Builder","systems":["aarch64-linux"],
   "features":["kvm","nixos-test","big-parallel"],"maxConcurrent":10},
  {"name":"x86-64-fetcher","kind":"Fetcher","image":"rio-fetcher",
   "systems":["x86_64-linux","i686-linux","builtin"],
   "privileged":null,"hostUsers":null,"seccompProfile":null,"tolerations":null,
   "nodeSelector":{"rio.build/node-role":"fetcher","kubernetes.io/arch":"amd64"}},
  {"name":"aarch64-fetcher","kind":"Fetcher","image":"rio-fetcher",
   "systems":["aarch64-linux","builtin"],
   "privileged":null,"hostUsers":null,"seccompProfile":null,"tolerations":null,
   "nodeSelector":{"rio.build/node-role":"fetcher","kubernetes.io/arch":"arm64"}}
]"#;

pub async fn run(cfg: &XtaskConfig, opts: &DeployOpts) -> Result<()> {
    let log_level = opts.log_level.as_str();
    let tenant = opts.tenant.as_deref();
    let skip_preflight = opts.skip_preflight;
    let no_hooks = opts.no_hooks;
    // Image tag recomputed from git state — `git::image_tag()` is
    // content-addressed (`sha256(git diff HEAD)`), so the same tree
    // state yields the same tag push computed. Tree drift since
    // `--push` → different tag → `assert_in_ecr` fails loudly with
    // "run --push first" instead of silently deploying a stale tag.
    let tag = git::image_tag(&git::open()?)?;
    let tag = tag.as_str();

    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;

    super::push::assert_in_ecr(tag, &region).await?;

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

    // Karpenter CRDs come from the karpenter-crd chart (terraform-
    // managed). The rio chart renders NodePool + EC2NodeClass CRs —
    // helm install fails with "no matches for kind" if the CRDs
    // haven't established yet.
    kube::wait_crd_established(&client, "nodepools.karpenter.sh", Duration::from_secs(120)).await?;

    // Namespaces first. Created here (not by the chart —
    // namespaces.create=false below) because: (a) the SSH Secret must
    // exist before helm runs; (b) Helm refuses to adopt a namespace it
    // didn't create. ADR-019 four-namespace split: control plane +
    // store at baseline, builders + fetchers at privileged (SYS_ADMIN
    // for FUSE).
    ui::step("namespaces + ssh secret", || async {
        ensure_namespaces(&client).await?;
        shared::ensure_gateway_ssh_secret(&client, cfg, tenant).await
    })
    .await?;

    // JWT keypair: mint-or-read. If `rio-jwt-signing` Secret exists,
    // reuse its seed (idempotent across deploys). Otherwise generate
    // fresh. Seed never touches disk or source — passes via --set,
    // lives only in process memory + the helm release secret (same
    // trust boundary as the rendered Secret).
    let jwt = shared::ensure_jwt_keypair(&client).await?;

    // Subchart symlink (same requirement as dev apply).
    ui::step("chart deps", crate::k8s::shared::chart_deps).await?;

    // NLB annotations (previously a --set-json one-liner in bash).
    // target-type:instance — Cilium cluster-pool overlay IPs (fd42::)
    // are NOT VPC-routable, so target-type:ip can't reach pods. NLB
    // targets node IPs at the NodePort; Cilium's eBPF kube-proxy
    // replacement handles node→pod. externalTrafficPolicy:Local in
    // gateway.yaml means only nodes hosting a gateway pod pass NLB
    // health checks (others are correctly unhealthy, not a bug).
    // --public-cidr flips internal→internet-facing AND sets
    // loadBalancerSourceRanges (the controller writes NLB SG ingress
    // rules). NLB scheme is immutable, so a flip recreates the LB.
    let scheme = if opts.public_cidrs.is_empty() {
        "internal"
    } else {
        "internet-facing"
    };
    let nlb_ann = json!({
        "service.beta.kubernetes.io/aws-load-balancer-type": "external",
        "service.beta.kubernetes.io/aws-load-balancer-nlb-target-type": "instance",
        "service.beta.kubernetes.io/aws-load-balancer-scheme": scheme,
        // dualstack: cluster is IPv6-only (no IPv4 Service CIDR), so
        // ip-address-type=ipv4 fails with "unsupported IPv6 config".
        // dualstack with target-type=instance needs the instances to
        // have a PRIMARY IPv6 — set by the primary-ipv6-init systemd
        // oneshot baked into the NixOS AMI (eks-node.nix); system
        // nodes are excluded from external LBs (main.tf).
        "service.beta.kubernetes.io/aws-load-balancer-ip-address-type": "dualstack",
        // dualstack listener + ipv6-only TG: IPv4 clients need the NLB
        // to source-NAT to an IPv6 prefix it owns. Without this the NLB
        // RSTs every IPv4 connection (no v6 source to forward with).
        // aws-lbc reconciles this via SetSubnets; prefixes auto-assign.
        "service.beta.kubernetes.io/aws-load-balancer-enable-prefix-for-ipv6-source-nat": "on",
        "service.beta.kubernetes.io/aws-load-balancer-attributes": "load_balancing.cross_zone.enabled=true",
        // preserve_client_ip OFF: with the instance-target default (on),
        // intra-VPC clients that are themselves registered targets hit
        // NLB hairpin RST, and the IPv6-source-NAT path above can't
        // engage. Source IP is already lost at Cilium
        // (loadBalancer.mode=snat, addons.tf) and rio-gateway doesn't
        // consume it.
        "service.beta.kubernetes.io/aws-load-balancer-target-group-attributes": "preserve_client_ip.enabled=false",
        "service.beta.kubernetes.io/aws-load-balancer-listener-attributes.TCP-22": "tcp.idle_timeout.seconds=3600",
    });

    ui::step("helm upgrade rio", || async {
        helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
            .namespace(NS)
            .set("namespaces.create", "false")
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
            .set_json(
                "gateway.service.loadBalancerSourceRanges",
                json!(opts.public_cidrs).to_string(),
            )
            .set("gateway.ssh.hostKeySecret", "rio-gateway-host-key")
            .set("karpenter.enabled", "true")
            .set("karpenter.clusterName", &cluster)
            .set("karpenter.nodeRoleName", &node_role)
            .set("karpenter.amiTag", ami_tag)
            // I-117b: one Pool per arch × kind (same I-108 list/
            // defaults split). Per-pod sizing is continuous (ADR-023
            // SpawnIntent), so there are no per-arch child pools.
            // I-098's kubernetes.io/arch nodeSelector lands arm pods
            // on arm nodes; the scheduler's hard_filter checks systems
            // so an aarch64 drv routes only to aarch64 executors.
            //
            // poolDefaults stays the per-pool template base (seccomp,
            // tolerations, nodeSelector, hostUsers — deep-merged in the
            // chart).
            .set("poolDefaults.enabled", "true")
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
            .set_json("pools", POOLS_JSON)
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
            .set("jwt.signingSeed", &jwt.seed)
            .set("jwt.publicKey", &jwt.pubkey)
            // P0539a: ServiceMonitor/PodMonitor/PrometheusRule. CRDs come
            // from kube-prometheus-stack (infra/eks/monitoring.tf), which
            // tofu apply lands before this runs.
            .set("monitoring.enabled", "true")
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

    // Bootstrap the `default` tenant + cache.nixos.org upstream so
    // `xtask rsb` works out of the box. The user's SSH key (written
    // above with comment [`crate::ssh::DEFAULT_TENANT`]) routes to this
    // tenant; without it the first `rsb` after a fresh `up` fails with
    // `SubmitBuild: unknown tenant: default`. Idempotent — re-deploys
    // see AlreadyExists. Tunnel uses ephemeral local ports (port-
    // forward to scheduler+store; helm --wait above guarantees Ready).
    ui::step("bootstrap default tenant", || async {
        let cli = super::smoke::CliCtx::open(&client, 0, 0).await?;
        super::smoke::step_tenant(&cli, crate::ssh::DEFAULT_TENANT).await?;
        super::smoke::step_upstream(&cli, crate::ssh::DEFAULT_TENANT).await
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
    .await?;

    if opts.wait_drift {
        wait_drift_settled(&client, DRIFT_SKIP_NODEPOOLS).await?;
    }
    Ok(())
}

/// NodePools whose NodeClaims `--wait-drift` ignores. `rio-general`
/// has `disruption.budgets: [{nodes:"0", reasons:[Drifted]}]` so its
/// claims stay `Drifted=True` by design until `xtask k8s
/// rotate-general` — waiting on them would block forever.
pub(crate) const DRIFT_SKIP_NODEPOOLS: &[&str] = &["rio-general"];

/// Poll until no Karpenter NodeClaim has `Drifted=True`. An AMI change
/// drifts every Karpenter node; the disruption controller replaces them
/// at the NodePool's budget rate. Returning early means subsequent
/// builds may be evicted mid-run. 30min covers ~10 sequential
/// replacements at the default `budgets:10%` × ~2-3min each.
///
/// `skip_pools`: NodePools to exclude from the wait — see
/// [`DRIFT_SKIP_NODEPOOLS`]. Pass `&[]` to wait on all pools (used by
/// `rotate-general` after deleting the held-back claims).
pub(crate) async fn wait_drift_settled(client: &kube::Client, skip_pools: &[&str]) -> Result<()> {
    let skip: Vec<String> = skip_pools.iter().map(|s| s.to_string()).collect();
    let api = status::nodeclaim_api(client);
    ui::poll(
        "karpenter drift settled",
        Duration::from_secs(15),
        120,
        move || {
            let api = api.clone();
            let skip = skip.clone();
            async move {
                // Right after CRD apply the apiserver returns 429
                // "storage is (re)initializing" for a few seconds while
                // the watch cache warms. Treat list errors as a retry
                // tick (same as gather_stuck_nodeclaims) — the 30min
                // poll bound caps a persistently-failing case.
                let claims = match api.list(&Default::default()).await {
                    Ok(c) => c,
                    Err(e) => {
                        info!("NodeClaim list error (will retry): {e}");
                        return Ok(None);
                    }
                };
                let drifted: Vec<String> = claims
                    .into_iter()
                    .filter(|nc| {
                        !nc.metadata
                            .labels
                            .as_ref()
                            .and_then(|l| l.get("karpenter.sh/nodepool"))
                            .is_some_and(|p| skip.iter().any(|s| s == p))
                    })
                    // Terminating claims are settling, not awaiting
                    // disruption — Karpenter freezes the Drifted
                    // condition during drain, so without this a
                    // graceful drain reads as "still drifted" for up
                    // to terminationGracePeriodSeconds.
                    .filter(|nc| nc.metadata.deletion_timestamp.is_none())
                    .filter(|nc| {
                        nc.data
                            .pointer("/status/conditions")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .any(|c| {
                                c.get("type").and_then(|v| v.as_str()) == Some("Drifted")
                                    && c.get("status").and_then(|v| v.as_str()) == Some("True")
                            })
                    })
                    .filter_map(|nc| nc.metadata.name)
                    .collect();
                if drifted.is_empty() {
                    return Ok(Some(()));
                }
                info!(
                    "{} drifted remaining: [{}]",
                    drifted.len(),
                    drifted.join(", ")
                );
                Ok(None)
            }
        },
    )
    .await
    .context("timed out waiting for Karpenter drift to settle (--wait-drift)")
}

/// Delete `rio-general` NodeClaims whose `status.imageID` doesn't match
/// the EC2NodeClass-resolved AMI set, so Karpenter re-provisions them
/// on the current AMI; then wait for drift to settle (including
/// rio-general). See [`DRIFT_SKIP_NODEPOOLS`] for why this is manual.
///
/// Idempotent: skips claims already terminating (`deletionTimestamp`
/// set), still launching (`status.imageID` not yet populated —
/// Karpenter writes it in the same status patch as `Launched=True`),
/// or already on a target AMI, so a re-run after a partial/timed-out
/// rotation doesn't delete the fresh replacements. With
/// [`wait_drift_settled`] now filtering terminating claims, the wait is
/// bounded by node-launch (~2-3min) — it no longer races the gateway's
/// 1h `sessionDrainSecs`.
pub async fn rotate_general() -> Result<()> {
    let client = kube::client().await?;
    let api = status::nodeclaim_api(&client);
    let target_amis = ec2nodeclass_resolved_amis(&client, "rio-default").await;
    if target_amis.is_empty() {
        warn!(
            "EC2NodeClass rio-default has no resolved AMIs (status.amis empty or \
             unreadable); AMI gate disabled — every launched rio-general claim will be deleted"
        );
    }

    let lp = ::kube::api::ListParams::default().labels("karpenter.sh/nodepool=rio-general");
    let claims = api.list(&lp).await?;
    if claims.items.is_empty() {
        info!("no rio-general NodeClaims found; nothing to rotate");
        return Ok(());
    }
    let mut deleted = 0usize;
    for nc in &claims.items {
        let name = nc.metadata.name.as_deref().unwrap_or("?");
        if nc.metadata.deletion_timestamp.is_some() {
            info!("NodeClaim {name}: already rotating (deletionTimestamp set); skipping");
            continue;
        }
        let image_id = nc
            .data
            .pointer("/status/imageID")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let Some(id) = image_id else {
            info!("NodeClaim {name}: launching (no status.imageID yet); skipping");
            continue;
        };
        if target_amis.contains(&id) {
            info!("NodeClaim {name}: already on target AMI {id}; skipping");
            continue;
        }
        info!("deleting NodeClaim {name} (imageID={id})");
        api.delete(name, &Default::default()).await?;
        deleted += 1;
    }
    if deleted == 0 {
        info!(
            "nothing to rotate — all {} rio-general claims terminating, launching, or on target AMI",
            claims.items.len()
        );
        return Ok(());
    }
    info!(
        "rio-general nodes rotating ({deleted} claims); gateway sessions on \
         draining nodes have up to sessionDrainSecs (1h) to finish"
    );
    wait_drift_settled(&client, &[]).await
}

/// Karpenter publishes the AMI(s) it resolved from `amiSelectorTerms` at
/// `EC2NodeClass.status.amis[*].id`. Returns the set so `rotate_general`
/// can gate deletes on `NodeClaim.status.imageID` — the predicate
/// Karpenter's `Drifted reason=AMIDrift` uses. Degrades to an empty set
/// (no AMI filter) on lookup failure.
async fn ec2nodeclass_resolved_amis(
    client: &kube::Client,
    name: &str,
) -> std::collections::HashSet<String> {
    use ::kube::{
        api::Api,
        core::{ApiResource, DynamicObject, GroupVersionKind},
    };
    let gvk = GroupVersionKind::gvk("karpenter.k8s.aws", "v1", "EC2NodeClass");
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ApiResource::from_gvk(&gvk));
    match api.get(name).await {
        Ok(nc) => nc
            .data
            .pointer("/status/amis")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|a| a.get("id").and_then(|v| v.as_str()).map(str::to_owned))
            .collect(),
        Err(e) => {
            tracing::warn!(
                "EC2NodeClass {name}: status.amis lookup failed ({e}); \
                 proceeding without AMI filter"
            );
            std::collections::HashSet::new()
        }
    }
}
