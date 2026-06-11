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
use crate::k8s::provider::DeployOpts;
use crate::k8s::{NS, NS_STORE, ensure_namespaces, shared, status};
use crate::{git, helm, tofu, ui};

/// `pools[]` (helm list — replaced wholesale, hence one const).
/// Per-arch general builder pools, per-arch kvm builder pools (NixOS
/// VM tests; the controller derives the `rio.build/kvm` toleration
/// from `features:[kvm]`; metal placement is per-intent `nodeAffinity`
/// — `r[ctrl.pool.kvm-device+2]`), and per-arch fetcher pools.
///
/// Per-pod (cores, mem, disk) come from the scheduler's per-drv
/// SpawnIntent (ADR-023) — there is NO per-pool resources knob.
///
/// Fetcher entries: `system="builtin"` FODs overflow to either arch;
/// `nodeSelector:null` clears poolDefaults.nodeSelector so the
/// reconciler default (`{rio.build/fetcher: true}` injected
/// unconditionally by `effective_node_selector` and merged with any
/// operator entries) passes through. CEL forbids privileged/
/// seccompProfile on Fetcher entries — those fields are deep-merged
/// from poolDefaults but rejected at admission; the `null` clears
/// prevent the merge. `hostUsers:null` clears poolDefaults.
/// hostUsers:true so EKS gets the reconciler default `false`
/// (hostUsers is NOT CEL-gated for Fetcher — k3s escape hatch).
const POOLS_JSON: &str = r#"[
  {"name":"x86-64","kind":"Builder","systems":["x86_64-linux","i686-linux"]},
  {"name":"aarch64","kind":"Builder","systems":["aarch64-linux"]},
  {"name":"x86-64-kvm","kind":"Builder","systems":["x86_64-linux","i686-linux"],
   "features":["kvm","nixos-test","big-parallel"],"maxConcurrent":10},
  {"name":"aarch64-kvm","kind":"Builder","systems":["aarch64-linux"],
   "features":["kvm","nixos-test","big-parallel"],"maxConcurrent":10},
  {"name":"x86-64-fetcher","kind":"Fetcher",
   "systems":["x86_64-linux","i686-linux","builtin"],
   "privileged":null,"hostUsers":null,"seccompProfile":null,"tolerations":null,
   "nodeSelector":null},
  {"name":"aarch64-fetcher","kind":"Fetcher",
   "systems":["aarch64-linux","builtin"],
   "privileged":null,"hostUsers":null,"seccompProfile":null,"tolerations":null,
   "nodeSelector":null}
]"#;

pub async fn run(cfg: &XtaskConfig, opts: &DeployOpts) -> Result<()> {
    let log_level = opts.log_level.as_str();
    let tenant = opts.tenant.as_deref();
    let skip_preflight = opts.skip_preflight;
    let skip_pg_preflight = opts.skip_pg_preflight;
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
    let vpc_ipv6_cidr = tf.get("vpc_ipv6_cidr_block")?;
    let cluster = tf.get("cluster_name")?;
    let node_role = tf.get("karpenter_node_role_name")?;
    // rds.tf locals: the AWS PG max_connections table at the
    // provisioned capacity, min-capacity cap applied (merged_bug_080).
    let modeled_pg_max: u32 = tf
        .get("pg_max_connections")?
        .parse()
        .context("tf output pg_max_connections is not an integer")?;
    let gateway_dns_fqdn = tf.get_opt("gateway_dns_fqdn");
    // bug_326: the S3 logs/ lifecycle expires at THIS + 7 (s3.tf) —
    // deploying the same variable into the store sweep keeps the two
    // deleters coupled by construction.
    let log_retention_days = tf.get("log_retention_days")?;

    info!("deploy tag={tag} ami={ami_tag} registry={ecr} cluster={cluster}");

    let client = kube::client().await?;

    // Preflight: bail early if the cluster is in a state where helm
    // upgrade will likely wedge (stuck NodeClaims, pending-upgrade
    // from a prior failed deploy). Cheap compared to the helm --wait
    // timeout. Bypass: --deploy-skip-preflight.
    if !skip_preflight {
        let ctx = kube::current_context().unwrap_or_default();
        let report = ui::step("preflight", || async {
            Ok::<_, anyhow::Error>(status::gather(&client, ctx).await)
        })
        .await?;
        status::preflight_check(&report)?;
    }

    // PG connection-budget preflight (merged_bug_080): MEASURE the
    // live server's max_connections from inside the cluster, assert
    // it equals the tf model (rds.tf locals — the AWS PG table at
    // the provisioned capacity), and derive the store ceiling from
    // the MEASUREMENT. The chart's values.yaml default is only the
    // modeled fallback for chart-only consumers; EKS always deploys
    // the derived value. Bypass: --deploy-skip-pg-preflight.
    //
    // Gate: the measurement pod's hard dependencies (the rio-store
    // namespace and the ESO-synced rio-postgres Secret it mounts) are
    // created by/after the very helm install this preflight gates, so
    // on a fresh or wiped cluster they CANNOT exist yet. Probe the
    // Secret and classify: readable -> measure; NotFound shapes ->
    // first-install degrade to the model (same path as
    // --deploy-skip-pg-preflight, same loud warning); anything else
    // (5xx, timeout, auth) -> abort. See classify_pg_preflight_gate.
    //
    // Hostable arm (D-052-1b): both ceiling paths below also clamp to
    // what the rio-general Karpenter pool can actually host — the pg
    // arm is a backstop, not a schedulability statement (live_052:
    // 173 deployed, 46 hostable, 133 pods Pending). Inputs read from
    // the chart so a pool-limit change flows through on the next
    // deploy instead of requiring an xtask edit.
    let values_yaml =
        std::fs::read_to_string(crate::sh::repo_root().join("infra/helm/rio-build/values.yaml"))
            .context(
                "read infra/helm/rio-build/values.yaml for the store hostable-ceiling inputs",
            )?;
    let karpenter_cpu_limit = general_pool_cpu_limit(&values_yaml)?;
    let node_vcpus = store_node_vcpus(STORE_CPU_REQUEST)?;
    let store_ceiling = if skip_pg_preflight {
        let fallback = derive_store_ceiling(
            modeled_pg_max,
            NON_STORE_PG_BUDGET,
            STORE_PG_MAX_CONNECTIONS_PER_REPLICA,
            STORE_PG_HEADROOM,
            karpenter_cpu_limit,
            node_vcpus,
        );
        warn!(
            modeled_pg_max,
            ceiling = fallback,
            "pg preflight SKIPPED (--deploy-skip-pg-preflight): store ceiling derived from the tf MODEL, not a live measurement"
        );
        fallback
    } else {
        let probe = kube::probe_secret_key(&client, NS_STORE, "rio-postgres", "url").await;
        match classify_pg_preflight_gate(probe)? {
            PgPreflightGate::DegradeFirstInstall => {
                let fallback = derive_store_ceiling(
                    modeled_pg_max,
                    NON_STORE_PG_BUDGET,
                    STORE_PG_MAX_CONNECTIONS_PER_REPLICA,
                    STORE_PG_HEADROOM,
                    karpenter_cpu_limit,
                    node_vcpus,
                );
                warn!(
                    modeled_pg_max,
                    ceiling = fallback,
                    "pg preflight: rio-store/rio-postgres secret absent (first install or post-wipe) - store ceiling derived from the tf MODEL; the next deploy measures and enforces the live value"
                );
                fallback
            }
            PgPreflightGate::Measure => {
                let measured =
                    ui::step("pg preflight", || pg_preflight_measure(&client, &ecr, tag)).await?;
                if measured != modeled_pg_max {
                    anyhow::bail!(
                        "pg preflight: live max_connections={measured} but the tf model \
                         (rds.tf aurora_pg_max_connections_by_max_acu + min-capacity cap) \
                         says {modeled_pg_max}. Either the capacity change has not been \
                         applied/rebooted yet (max_connections is a STATIC parameter — \
                         it changes only after an instance reboot) or the rds.tf table \
                         is wrong for this capacity. Fix the model or finish the resize; \
                         do not hand-edit the ceiling."
                    );
                }
                derive_store_ceiling(
                    measured,
                    NON_STORE_PG_BUDGET,
                    STORE_PG_MAX_CONNECTIONS_PER_REPLICA,
                    STORE_PG_HEADROOM,
                    karpenter_cpu_limit,
                    node_vcpus,
                )
            }
        }
    };
    info!(
        store_ceiling,
        modeled_pg_max,
        karpenter_cpu_limit,
        node_vcpus,
        "store autoscaling ceiling: min(pg arm, karpenter-hostable arm)"
    );

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
    let mut nlb_ann = json!({
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
    // external-dns (dns.tf) reconciles this annotation → DNS record.
    // Absent when gateway_dns is disabled or the state predates it.
    if let Some(fqdn) = &gateway_dns_fqdn {
        nlb_ann["external-dns.alpha.kubernetes.io/hostname"] = json!(fqdn);
    }

    ui::step("helm upgrade rio", || async {
        // helm --wait is silent for the full timeout; on a post-wipe
        // cold start that's 3-4min of nothing. Side-task prints
        // not-yet-Ready Deployments every 15s; aborted when helm exits.
        let progress = shared::spawn_helm_wait_progress(&client);
        let r = helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
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
            // store-egress CiliumNetworkPolicy admits postgres on this
            // CIDR; the chart default fc00::/7 (ULA) does NOT match a
            // VPC GUA, so without this rio-store→Aurora is dropped.
            .set("global.postgresCidr", &vpc_ipv6_cidr)
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
            //
            // r[impl infra.store.autoscaling+4]
            // Store replica count: owned by the chart-default KEDA
            // ScaledObject (store.autoscaling.enabled=true; backlog/
            // builders/CPU triggers — the I-128 fixed-8 era and the
            // ComponentScaler era both retired). store.replicas is
            // not rendered while autoscaling is on (store.yaml's
            // lookup-echo branch).
            //
            // The CEILING is the pg preflight's derivation above:
            // measured (or tf-modeled, on skip/first-install)
            // max_connections -> derive_store_ceiling. The chart
            // default (values.yaml) is only the modeled fallback for
            // chart-only consumers; EKS always deploys this value.
            .set("store.autoscaling.maxReplicas", store_ceiling.to_string())
            // I-171: pgMaxConnections was 200 (sized for 16-ACU
            // Aurora); 20 + idle_timeout (rio-store/src/main.rs) keeps
            // steady-state under budget and self-shrinks after bursts.
            // Single-sourced with the ceiling derivation above
            // (STORE_PG_MAX_CONNECTIONS_PER_REPLICA) so the divisor
            // and the deployed pool size cannot drift.
            .set(
                "store.pgMaxConnections",
                STORE_PG_MAX_CONNECTIONS_PER_REPLICA.to_string(),
            )
            // bug_326: single-sourced with the S3 lifecycle (tf).
            .set("store.logRetentionDays", &log_retention_days)
            // I-147/I-150: production-scale resources. values.yaml defaults
            // stay small so VM-test k3s (2-node QEMU) can schedule; EKS
            // gets the real sizing here.
            .set("controller.resources.requests.cpu", "8")
            .set("controller.resources.requests.memory", "8Gi")
            .set("controller.resources.limits.memory", "64Gi")
            // Single-sourced with the hostable-arm node-shape
            // derivation (STORE_CPU_REQUEST -> store_node_vcpus): the
            // deployed request and the ceiling divisor cannot drift.
            .set(
                "store.resources.requests.cpu",
                STORE_CPU_REQUEST.to_string(),
            )
            .set("store.resources.requests.memory", "8Gi")
            .set("store.resources.limits.memory", "32Gi")
            .set("scheduler.resources.requests.cpu", "32")
            .set("scheduler.resources.requests.memory", "16Gi")
            .set("scheduler.resources.limits.memory", "64Gi")
            .set("gateway.resources.requests.cpu", "32")
            .set("gateway.resources.requests.memory", "16Gi")
            .set("gateway.resources.limits.memory", "64Gi")
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
            // the hook so the chart lands; operator runs `k8s qa --health`
            // manually once nodes are up.
            .no_hooks(no_hooks)
            .run()
            .await;
        progress.abort();
        r
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

/// Non-store PG connection consumers, subtracted from the budget
/// before dividing by the per-replica pool size: scheduler 2 replicas
/// x pool 10 + controller 4 + ad-hoc psql headroom 10.
const NON_STORE_PG_BUDGET: u32 = 34;

/// The store chart's `pgMaxConnections` (per-replica sqlx pool size).
/// Single source for both the helm `--set` and the ceiling division.
const STORE_PG_MAX_CONNECTIONS_PER_REPLICA: u32 = 20;

/// Fraction of the measured budget the store fleet may consume; the
/// remaining ~30% absorbs migration bursts, ESO, and operator psql.
const STORE_PG_HEADROOM: f64 = 0.70;

/// The store Deployment's CPU request on EKS — single source for the
/// helm `--set` below and the hostable-arm node-shape derivation
/// (`store_node_vcpus`), so the deployed request and the ceiling's
/// divisor cannot drift.
const STORE_CPU_REQUEST: u32 = 16;

/// The chart's `karpenter.nodePools[rio-general]` instance-family pin
/// (values.yaml). `general_pool_cpu_limit` BAILS if the chart moves
/// outside this set, because `GENERAL_NODE_VCPU_LADDER` below is
/// derived from these families — a family change must re-derive the
/// ladder, not silently divide by the wrong node shape.
const GENERAL_POOL_FAMILIES: &[&str] = &["c8a", "m8a", "r8a"];

/// vCPU sizes of the rio-general families (c8a/m8a/r8a share the AMD
/// gen-8 ladder: large..48xlarge). The hostable arm needs the NODE
/// shape, not the pod request: store placement is required
/// one-replica-per-node podAntiAffinity (store.yaml), and Karpenter
/// counts the minted node's full capacity against the pool's
/// `limits.cpu` — so each replica consumes a whole node from the
/// budget regardless of its request.
const GENERAL_NODE_VCPU_LADDER: &[u32] = &[2, 4, 8, 16, 32, 48, 64, 96, 128, 192];

/// Allocatable model: a pod fits a node when its request fits ~90% of
/// capacity (kubelet/system reserve + daemonsets — the same 90%
/// approximation the chart uses for `max_node_disk`). This is why a
/// 16-CPU store request pins a 32-vCPU node, not a 16-vCPU one.
const NODE_ALLOCATABLE_FRACTION: f64 = 0.9;

/// The node shape one store replica pins: the smallest rio-general
/// ladder size whose modeled allocatable fits `request_cpu`. Errors
/// when nothing fits (a request past the ladder top is a config bug,
/// not a ceiling-0 situation).
fn store_node_vcpus(request_cpu: u32) -> Result<u32> {
    GENERAL_NODE_VCPU_LADDER
        .iter()
        .copied()
        .find(|&size| NODE_ALLOCATABLE_FRACTION * f64::from(size) >= f64::from(request_cpu))
        .with_context(|| {
            format!(
                "store cpu request {request_cpu} does not fit any rio-general \
                 instance size (ladder top {} x {NODE_ALLOCATABLE_FRACTION} \
                 allocatable); shrink the request or re-derive the ladder",
                GENERAL_NODE_VCPU_LADDER.last().expect("ladder non-empty"),
            )
        })
}

/// `karpenter.nodePools[rio-general].limits.cpu` from the chart's
/// values.yaml body — the hostable arm's budget input, read from the
/// chart (not duplicated here) so an operator raising the pool limit
/// raises the deployed ceiling on the next `eks up` with no xtask
/// edit. Also asserts the pool's instance-family pin stays inside
/// `GENERAL_POOL_FAMILIES` (the ladder's provenance).
///
/// No path-reading unit test (crate2nix per-crate builds stage only
/// the xtask/ subtree, the seccomp-regen precedent); the parser is
/// tested on an embedded fixture and the runtime read fails loudly.
fn general_pool_cpu_limit(values_yaml: &str) -> Result<u32> {
    #[derive(serde::Deserialize)]
    struct Values {
        karpenter: Karpenter,
    }
    #[derive(serde::Deserialize)]
    struct Karpenter {
        #[serde(rename = "nodePools")]
        node_pools: Vec<NodePool>,
    }
    #[derive(serde::Deserialize)]
    struct NodePool {
        name: String,
        #[serde(default)]
        requirements: Vec<Requirement>,
        limits: Limits,
    }
    #[derive(serde::Deserialize)]
    struct Requirement {
        key: String,
        #[serde(default)]
        values: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Limits {
        cpu: u32,
    }
    let v: Values = serde_saphyr::from_str(values_yaml)
        .context("parse helm values.yaml (karpenter.nodePools shape)")?;
    let pool = v
        .karpenter
        .node_pools
        .iter()
        .find(|p| p.name == "rio-general")
        .context("karpenter.nodePools has no rio-general entry — the store hostable-ceiling derivation needs its limits.cpu")?;
    for req in &pool.requirements {
        if req.key == "karpenter.k8s.aws/instance-family" {
            for fam in &req.values {
                anyhow::ensure!(
                    GENERAL_POOL_FAMILIES.contains(&fam.as_str()),
                    "rio-general instance-family {fam:?} is outside the derived \
                     vCPU ladder's families {GENERAL_POOL_FAMILIES:?}; re-derive \
                     GENERAL_NODE_VCPU_LADDER for the new family before deploying"
                );
            }
        }
    }
    Ok(pool.limits.cpu)
}

/// Store autoscaling ceiling: the MIN of two independently-binding
/// arms (D-052-1b, live_052).
///
/// PG arm: `floor((headroom x measured - non_store) / per_replica)`,
/// saturating at 0 — the connection-budget backstop the values.yaml
/// default comment documents (change both together).
///
/// Hostable arm: `floor(karpenter_cpu_limit / node_vcpus)` — how many
/// store nodes the rio-general pool can actually mint. live_052: the
/// PG-only ceiling (173) let KEDA commit 4->173 replicas in 75s
/// against a pool that hosts 46 nodes; 133 pods sat Pending while
/// Karpenter churned NodeClaims at the limit. A Pending pod is not
/// harmless at that volume: every reconcile pass and scheduler sweep
/// pays for it, and the backlog gauge keeps asking. The deployed
/// ceiling now stops where hosting stops; the previous live fix was a
/// hand `--set` to 46 that the next `eks up` would have clobbered
/// back to 173.
fn derive_store_ceiling(
    measured_max_connections: u32,
    non_store_budget: u32,
    per_replica: u32,
    headroom: f64,
    karpenter_cpu_limit: u32,
    node_vcpus: u32,
) -> u32 {
    debug_assert!(node_vcpus > 0, "store_node_vcpus never yields 0");
    let usable = headroom * f64::from(measured_max_connections) - f64::from(non_store_budget);
    let pg_arm = if usable <= 0.0 {
        0
    } else {
        (usable / f64::from(per_replica)).floor() as u32
    };
    let hostable_arm = karpenter_cpu_limit / node_vcpus.max(1);
    pg_arm.min(hostable_arm)
}

/// What the pg connection-budget preflight should do, decided from
/// probing the ESO-synced `rio-store/rio-postgres` Secret — the
/// measurement pod's hard dependency (its env is a secretKeyRef on
/// that Secret, and its namespace is the Secret's namespace).
#[derive(Debug, PartialEq, Eq)]
enum PgPreflightGate {
    /// Secret readable: namespace and credentials exist, so the
    /// measurement pod can run. Measure live and enforce the model.
    Measure,
    /// First-install shape: the Secret, its `url` key, or the
    /// rio-store namespace itself does not exist yet. All of them are
    /// created by/after the helm install this preflight gates, so on
    /// a fresh or wiped cluster the tf model is the only available
    /// truth — degrade to it loudly; the next deploy measures live.
    DegradeFirstInstall,
}

/// Classify the Secret probe into the preflight's gate decision.
///
/// Invariant (merged_bug_080 follow-up): the preflight must be
/// runnable against ANY cluster state the deploy itself is legal
/// against — fresh (no rio namespaces), partially torn down
/// (rio-store gone, infra add-ons alive), or healthy. First-boot
/// cannot 404: the measurement pod is only attempted after its
/// Secret was actually READ, so its namespace existed.
///
/// Only the apiserver's NotFound shapes degrade —
/// `secrets "rio-postgres" not found` and
/// `namespaces "rio-store" not found` (the same `is_not_found`
/// predicate kube-rs `get_opt` folds on). Every other error (5xx,
/// timeout, auth) stays a hard failure so a flaky apiserver aborts
/// the deploy instead of silently shipping a modeled ceiling.
fn classify_pg_preflight_gate(
    probe: Result<Option<String>, ::kube::Error>,
) -> Result<PgPreflightGate> {
    match probe {
        Ok(Some(_)) => Ok(PgPreflightGate::Measure),
        // Secret exists but the url key is missing/non-UTF-8: ESO
        // half-sync; the gated deploy is what repairs it. Without
        // credentials the measurement pod would only sit in
        // CreateContainerConfigError until timeout.
        Ok(None) => Ok(PgPreflightGate::DegradeFirstInstall),
        // The apiserver's explicit NotFound (same predicate kube-rs
        // get_opt folds on): `secrets "rio-postgres" not found` when
        // only the Secret is missing, `namespaces "rio-store" not
        // found` when the namespace itself is. Both are the
        // first-install shape.
        Err(::kube::Error::Api(st)) if st.is_not_found() => {
            Ok(PgPreflightGate::DegradeFirstInstall)
        }
        Err(e) => Err(e).context(
            "pg preflight: probing rio-store/rio-postgres failed with a \
             non-NotFound error; aborting rather than silently deploying \
             the modeled ceiling (rerun, or bypass deliberately with \
             --deploy-skip-pg-preflight)",
        ),
    }
}

/// Spawn the one-shot `rio-store pg-preflight` pod in the store
/// namespace (it inherits the store-egress CiliumNetworkPolicy via the
/// rio-store name label) and parse its `max_connections=N` output.
async fn pg_preflight_measure(client: &kube::Client, ecr: &str, tag: &str) -> Result<u32> {
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "rio-pg-preflight",
            "labels": {
                // rio-store name label: rides the store-egress CNP
                // (postgres allow). Distinct component label so the
                // Service selector / PDB never match it.
                "app.kubernetes.io/name": "rio-store",
                "app.kubernetes.io/component": "pg-preflight",
            },
        },
        "spec": {
            "restartPolicy": "Never",
            "securityContext": {
                "runAsNonRoot": true,
                "runAsUser": 65532,
                "runAsGroup": 65532,
                "fsGroup": 65532,
                "seccompProfile": {"type": "RuntimeDefault"},
            },
            "containers": [{
                "name": "pg-preflight",
                "image": format!("{ecr}/rio-store:{tag}"),
                "args": ["pg-preflight"],
                "env": [{
                    "name": "RIO_DATABASE_URL",
                    "valueFrom": {"secretKeyRef": {"name": "rio-postgres", "key": "url"}},
                }],
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": {"drop": ["ALL"]},
                },
                "resources": {
                    "requests": {"cpu": "100m", "memory": "128Mi"},
                    "limits": {"memory": "256Mi"},
                },
            }],
        },
    });
    let logs = kube::run_oneshot_pod(client, NS_STORE, pod, Duration::from_secs(180)).await?;
    parse_pg_preflight_output(&logs)
}

/// Parse `max_connections=N` from the preflight pod's logs (last
/// occurrence wins — earlier lines may be connection-retry noise).
fn parse_pg_preflight_output(logs: &str) -> Result<u32> {
    logs.lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("max_connections="))
        .and_then(|v| v.trim().parse().ok())
        .with_context(|| {
            format!("pg preflight pod produced no max_connections=N line; logs:\n{logs}")
        })
}

#[cfg(test)]
mod ceiling_tests {
    use super::*;

    /// A karpenter limit high enough that the hostable arm never
    /// binds — isolates the PG arm in the tests below.
    const PG_ARM_ONLY: u32 = 1_000_000;

    /// The two documented operating points of the AWS PG table:
    /// the min-capacity-capped 2,000 and the 32-ACU 5,000 (PG arm
    /// isolated; the deployed value additionally clamps to the
    /// hostable arm — see the incident test).
    #[test]
    fn derive_store_ceiling_documented_points() {
        // floor((0.70x2000 - 34)/20) = floor(1366/20) = 68
        assert_eq!(
            derive_store_ceiling(2000, 34, 20, 0.70, PG_ARM_ONLY, 32),
            68
        );
        // floor((0.70x5000 - 34)/20) = floor(3466/20) = 173
        assert_eq!(
            derive_store_ceiling(5000, 34, 20, 0.70, PG_ARM_ONLY, 32),
            173
        );
    }

    /// THE live_052 numbers (D-052-1b). The pg arm said 173; the
    /// rio-general pool (limits.cpu 1500) hosts floor(1500/32) = 46
    /// store nodes under one-replica-per-node anti-affinity — KEDA
    /// committed 4->173 in 75s and stranded 133 pods Pending. The
    /// deployed ceiling is the min of the arms.
    #[test]
    fn derive_store_ceiling_clamps_to_karpenter_hostable() {
        // The incident shape: min(173, 46) = 46 — the value the live
        // hand-override pinned and the next `eks up` would have
        // clobbered back to 173 before this clamp existed.
        assert_eq!(derive_store_ceiling(5000, 34, 20, 0.70, 1500, 32), 46);
        // Both documented PG points clamp identically at the current
        // pool shape (68 and 173 both exceed 46).
        assert_eq!(derive_store_ceiling(2000, 34, 20, 0.70, 1500, 32), 46);

        // The naive divisor is the COUNTEREXAMPLE, not the formula:
        // dividing the pool limit by the store pod's cpu REQUEST
        // (16) models bin-packing that placement forbids — required
        // one-replica-per-node podAntiAffinity makes each replica
        // consume a whole node, and Karpenter counts node CAPACITY
        // (32 vCPU for the minted shape) against limits.cpu. The
        // request-divisor answer (93) over-admits 2x.
        assert_eq!(1500 / STORE_CPU_REQUEST, 93);
        assert_eq!(store_node_vcpus(STORE_CPU_REQUEST).unwrap(), 32);
        assert_ne!(
            1500 / STORE_CPU_REQUEST,
            derive_store_ceiling(5000, 34, 20, 0.70, 1500, 32),
            "the hostable arm must divide by the node shape, not the pod request"
        );
    }

    /// Node-shape derivation: smallest rio-general ladder size whose
    /// 90%-allocatable fits the request. A request equal to a ladder
    /// size never fits that size (capacity != allocatable — exactly
    /// why 16 cpu pins a 32-vCPU node); past the ladder top is an
    /// error, not a silent clamp.
    #[test]
    fn store_node_vcpus_allocatable_model() {
        assert_eq!(store_node_vcpus(1).unwrap(), 2); // 0.9*2 = 1.8 >= 1
        assert_eq!(store_node_vcpus(2).unwrap(), 4); // 0.9*2 = 1.8 < 2
        assert_eq!(store_node_vcpus(8).unwrap(), 16); // 0.9*8 = 7.2 < 8
        assert_eq!(store_node_vcpus(16).unwrap(), 32); // 0.9*16 = 14.4 < 16
        assert_eq!(store_node_vcpus(29).unwrap(), 48); // 0.9*32 = 28.8 < 29
        assert!(store_node_vcpus(192).is_err()); // 0.9*192 = 172.8 < 192
    }

    /// values.yaml parser: reads rio-general's limits.cpu, refuses an
    /// instance-family pin outside the ladder's provenance, refuses a
    /// values shape with no rio-general pool. Embedded fixture (the
    /// crate2nix test sandbox stages only xtask/, so the real chart
    /// file is runtime-read only — the seccomp-regen precedent).
    #[test]
    fn general_pool_cpu_limit_parses_and_guards() {
        let fixture = r#"
karpenter:
  nodePools:
    - name: rio-general
      weight: 50
      labels:
        rio.build/node-role: general
      taints: []
      requirements:
        - key: karpenter.k8s.aws/instance-family
          operator: In
          values: [c8a, m8a, r8a]
        - key: karpenter.sh/capacity-type
          operator: In
          values: [on-demand]
      limits: {cpu: 1500}
"#;
        assert_eq!(general_pool_cpu_limit(fixture).unwrap(), 1500);

        let drifted = fixture.replace("c8a", "c9g");
        let err = general_pool_cpu_limit(&drifted).unwrap_err().to_string();
        assert!(
            err.contains("re-derive"),
            "family drift must demand a ladder re-derivation, got: {err}"
        );

        let renamed = fixture.replace("rio-general", "rio-everything");
        assert!(general_pool_cpu_limit(&renamed).is_err());
    }

    /// Headroom edges: budgets at or below the non-store floor
    /// saturate at 0 (the caller deploys a floor-violating ceiling
    /// loudly rather than panicking), and the boundary where one
    /// replica first fits.
    #[test]
    fn derive_store_ceiling_edges() {
        assert_eq!(derive_store_ceiling(0, 34, 20, 0.70, PG_ARM_ONLY, 32), 0);
        // 33.6 - 34 < 0
        assert_eq!(derive_store_ceiling(48, 34, 20, 0.70, PG_ARM_ONLY, 32), 0);
        // 0.3/20 floors to 0
        assert_eq!(derive_store_ceiling(49, 34, 20, 0.70, PG_ARM_ONLY, 32), 0);
        // 20.6/20 -> 1
        assert_eq!(derive_store_ceiling(78, 34, 20, 0.70, PG_ARM_ONLY, 32), 1);
        // Full headroom (1.0) sanity: floor((189-34)/20) = 7.
        assert_eq!(derive_store_ceiling(189, 34, 20, 1.0, PG_ARM_ONLY, 32), 7);
        // A pool limit below one node floors the hostable arm to 0:
        // the ceiling honestly says "nothing is hostable".
        assert_eq!(derive_store_ceiling(5000, 34, 20, 0.70, 31, 32), 0);
    }

    /// Output-contract parser: last max_connections= line wins, noise
    /// tolerated, absence is an error.
    #[test]
    fn parse_pg_preflight_output_contract() {
        assert_eq!(
            parse_pg_preflight_output("warn: retrying\nmax_connections=5000\n").unwrap(),
            5000
        );
        assert_eq!(
            parse_pg_preflight_output("max_connections=10\nmax_connections=2000").unwrap(),
            2000
        );
        assert!(parse_pg_preflight_output("no luck").is_err());
    }
}

#[cfg(test)]
mod pg_gate_tests {
    use super::*;
    use ::kube::core::Status;

    /// kube 3.x apiserver error shape: `Error::Api(Box<Status>)`.
    fn api_err(message: &str, reason: &str, code: u16) -> ::kube::Error {
        ::kube::Error::Api(Status::failure(message, reason).with_code(code).boxed())
    }

    /// The deploy-blocking first-boot failure, pinned: on a cluster
    /// where the rio-store NAMESPACE does not exist (fresh, or wiped
    /// while infra add-ons survive), the Secret probe 404s. That MUST
    /// route to the first-install degrade — the shipped gate instead
    /// measured, and the measurement pod's create then failed the
    /// whole deploy with `namespaces "rio-store" not found`.
    #[test]
    fn gate_degrades_on_absent_namespace() {
        let probe = Err(api_err(
            r#"namespaces "rio-store" not found"#,
            "NotFound",
            404,
        ));
        assert_eq!(
            classify_pg_preflight_gate(probe).unwrap(),
            PgPreflightGate::DegradeFirstInstall
        );
    }

    /// Namespace exists but the ESO-synced Secret hasn't landed (ESO
    /// syncs only after the chart's ExternalSecret is installed):
    /// same first-install shape, same degrade.
    #[test]
    fn gate_degrades_on_absent_secret() {
        let probe = Err(api_err(
            r#"secrets "rio-postgres" not found"#,
            "NotFound",
            404,
        ));
        assert_eq!(
            classify_pg_preflight_gate(probe).unwrap(),
            PgPreflightGate::DegradeFirstInstall
        );
    }

    /// Secret synced but the `url` key is missing/non-UTF-8 (ESO
    /// half-sync). Measuring is impossible without credentials, and
    /// the deploy being gated is exactly what repairs the sync —
    /// degrade, don't spawn a pod doomed to CreateContainerConfigError.
    #[test]
    fn gate_degrades_on_missing_key() {
        assert_eq!(
            classify_pg_preflight_gate(Ok(None)).unwrap(),
            PgPreflightGate::DegradeFirstInstall
        );
    }

    /// Healthy path unchanged: secret readable -> measure live.
    #[test]
    fn gate_measures_when_secret_readable() {
        let probe = Ok(Some("postgres://rio:pw@db.example:5432/rio".into()));
        assert_eq!(
            classify_pg_preflight_gate(probe).unwrap(),
            PgPreflightGate::Measure
        );
    }

    /// Transient apiserver failures must ABORT the deploy, not
    /// silently fall back to the modeled ceiling. The shipped gate
    /// had this inverted (`.is_err()` -> degrade): a flaky apiserver
    /// mid-deploy would have deployed a model-derived ceiling with
    /// nothing but a "first install?" warning.
    #[test]
    fn gate_aborts_on_transient_api_errors() {
        for (message, reason, code) in [
            ("etcdserver: leader changed", "ServiceUnavailable", 503),
            (
                "an error on the server has prevented the request from succeeding",
                "InternalError",
                500,
            ),
            ("Unauthorized", "Unauthorized", 401),
            (
                "the server has received too many requests",
                "TooManyRequests",
                429,
            ),
        ] {
            let err = classify_pg_preflight_gate(Err(api_err(message, reason, code)))
                .expect_err(&format!("{reason} ({code}) must abort, not degrade"));
            assert!(
                err.to_string().contains("aborting"),
                "{reason} ({code}) error must carry the abort context, got: {err:#}"
            );
        }
    }

    /// Non-API transport errors (connect timeout, TLS, DNS) likewise
    /// abort — only the apiserver's explicit NotFound is "absent".
    #[test]
    fn gate_aborts_on_transport_error() {
        let probe = Err(::kube::Error::Service("connect timeout".into()));
        let err = classify_pg_preflight_gate(probe).expect_err("transport error must abort");
        assert!(err.to_string().contains("aborting"), "got: {err:#}");
    }
}
