//! Tear down the EKS deployment.
//!
//! Ordering matters — getting this wrong leaves orphaned EC2 instances,
//! NLBs, or a tofu destroy that hangs on a stuck Namespace finalizer:
//!
//!   1. **Delete pool CRs.** rio-controller's drain finalizer would
//!      normally block until DrainExecutor completes — but we're about
//!      to delete the controller and scheduler too, so trigger delete
//!      now (controller starts draining), then strip finalizers in
//!      step 3 once the controller is gone. We don't scale to 0 first:
//!      ephemeral pools have CEL `max>0`, and the finalizer-strip
//!      makes graceful drain best-effort anyway.
//!   2. **helm uninstall rio.** Removes the chart's NodePool /
//!      EC2NodeClass / Service type=LoadBalancer (NLB) / SeccompProfile
//!      CRs / etc. Tofu's `helm_release.aws_lbc` is what tears down the
//!      NLB infra, but the *Service* must go first or aws-lbc never gets
//!      the delete event → NLB orphaned.
//!   3. **Strip drain finalizers + wait.** With the controller gone
//!      (step 2), the finalizers from step 1 are orphaned. Patch them
//!      off so the namespace delete in step 5 doesn't wedge.
//!   4. **Delete NodeClaims.** Karpenter-provisioned EC2. If we let
//!      tofu delete `helm_release.karpenter` first, the controller is
//!      gone before it can terminate its instances → EC2 orphans.
//!   5. **Delete xtask-managed K8s objects.** rio-* namespaces (xtask
//!      created them with `namespace.create=false`), the SSH Secret,
//!      SPO + spod (kubectl-applied, not helm). Envoy Gateway too if
//!      it was installed.
//!   6. **tofu destroy.** Everything else: cluster, VPC, RDS, S3, ECR,
//!      tofu-managed helm releases (cert-manager, aws-lbc, karpenter,
//!      ESO). RDS `skip_final_snapshot=true`, S3 `force_destroy=true`,
//!      ECR `force_delete=true` are already set in the `.tf` files.
//!      Aurora's `deletion_protection` defaults to false in the AWS
//!      provider, and rds.tf doesn't override it.
//!
//! Any K8s step tolerates "cluster already gone / kubeconfig stale" —
//! we're tearing down, so a 404 or unreachable apiserver is success.
//! tofu destroy at the end is the real assertion.
//!
//! NOT cleaned up (deliberately): the `rio/*` Secrets Manager secrets
//! (created by the bootstrap Job, not tofu — they enter a 30-day
//! recovery window on their own when re-created with the same name);
//! the tfstate bucket (xtask `bootstrap` owns that lifecycle).

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::TF_DIR;
use crate::k8s::{NAMESPACES, NS, NS_BUILDERS, NS_FETCHERS};
use crate::sh::{self, cmd, repo_root, shell};
use crate::{helm, tofu, ui};

/// Best-effort kubectl. Captures + dims output; "not found" / NotFound
/// / unreachable-apiserver are treated as success because we're
/// destroying — the resource being gone is the goal. Any other failure
/// dumps captured output via `sh::run`'s usual path.
async fn k(args: &[&str]) -> Result<()> {
    let sh = shell()?;
    match sh::run(cmd!(sh, "kubectl {args...}")).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = format!("{e:#}");
            // The failure modes we WANT to swallow: object already
            // gone, CRD never installed, or the cluster itself is gone.
            let benign = msg.contains("NotFound")
                || msg.contains("not found")
                || msg.contains("the server doesn't have a resource type")
                || msg.contains("Unable to connect to the server")
                || msg.contains("could not find the requested resource");
            if benign {
                info!("(already gone) kubectl {}", args.join(" "));
                Ok(())
            } else {
                Err(e).with_context(|| format!("kubectl {}", args.join(" ")))
            }
        }
    }
}

/// `kubectl patch` has no `--all` — enumerate names first, then patch
/// each. Missing CRD / empty list / unreachable cluster are all "done".
async fn k_patch_all(ns: &str, kind: &str, patch: &str) -> Result<()> {
    let sh = shell()?;
    let names = match sh::try_read(cmd!(
        sh,
        "kubectl -n {ns} get {kind} -o name --ignore-not-found"
    )) {
        Ok(s) => s,
        Err(e) => {
            // Same benign-swallow as `k()` — try_read folds stderr
            // into the error message.
            let msg = format!("{e:#}");
            if msg.contains("the server doesn't have a resource type")
                || msg.contains("Unable to connect to the server")
            {
                return Ok(());
            }
            return Err(e).with_context(|| format!("list {kind} in {ns}"));
        }
    };
    for name in names.lines().filter(|l| !l.is_empty()) {
        info!("patch {ns}/{name}");
        k(&["-n", ns, "patch", name, "--type=merge", "-p", patch]).await?;
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    let cluster =
        tofu::output(TF_DIR, "cluster_name").unwrap_or_else(|_| "(tofu output unavailable)".into());
    info!("destroy target: EKS cluster '{cluster}'");

    // Reachability gate: a re-run after partial tofu destroy has no
    // cluster to talk to. Skip all kubectl steps and go straight to
    // tofu destroy. Using a raw `kubectl version` as the probe — it
    // fails fast and the failure mode (connection refused / no such
    // host / Unauthorized) is exactly what we want to catch.
    let sh = shell()?;
    let cluster_reachable = sh::run(cmd!(sh, "kubectl version --request-timeout=5s"))
        .await
        .is_ok();
    if !cluster_reachable {
        warn!(
            "kube-apiserver unreachable (cluster already deleted?); \
             skipping kubectl steps and proceeding to tofu destroy"
        );
        return tofu_destroy().await;
    }

    // ── 1. Kick off pool CR deletion ───────────────────────────────
    // --wait=false: the drain finalizer holds these until step 3
    // strips it (controller will be gone after step 2). Deleting now
    // lets the controller START draining while it's still up — best-
    // effort graceful, not blocking.
    // The pre-ADR-019 names (workerpool, workerpoolset, build) and the
    // pre-namespace-split location (NS = rio-system) are included so a
    // cluster that was provisioned BEFORE the rename can still be torn
    // down. `k()` treats "no such resource type" as success, so listing
    // legacy kinds is free on a fresh cluster.
    const POOL_KINDS: &[(&str, &str)] = &[
        (NS_BUILDERS, "builderpool"),
        (NS_BUILDERS, "builderpoolset"),
        (NS_FETCHERS, "fetcherpool"),
        (NS, "workerpool"),
        (NS, "workerpoolset"),
        (NS, "build"),
    ];
    ui::step(
        "delete BuilderPool/FetcherPool CRs (non-blocking)",
        || async {
            for &(ns, kind) in POOL_KINDS {
                k(&[
                    "-n",
                    ns,
                    "delete",
                    kind,
                    "--all",
                    "--wait=false",
                    "--ignore-not-found",
                ])
                .await?;
            }
            Ok(())
        },
    )
    .await?;

    // ── 2. helm uninstall rio ──────────────────────────────────────
    // --wait so the chart's pre-delete hooks (none today, but future-
    // proof) and Karpenter NodePool removal land before we delete
    // NodeClaims. helm's --ignore-not-found makes this idempotent.
    ui::step("helm uninstall rio", || async {
        let sh = shell()?;
        match sh::run(cmd!(
            sh,
            "helm uninstall rio -n {NS} --wait --timeout 10m --ignore-not-found"
        ))
        .await
        {
            Ok(()) => Ok(()),
            // Unreachable apiserver = cluster already gone. Tofu
            // destroy will confirm.
            Err(e) if format!("{e:#}").contains("Kubernetes cluster unreachable") => {
                info!("(cluster unreachable; skipping helm uninstall)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    })
    .await?;

    // ── 3. Strip orphaned drain finalizers ─────────────────────────
    // Controller is gone (step 2); finalizers from step 1 are now
    // orphaned. merge-patch metadata.finalizers=[] (idempotent — also
    // a no-op if the controller already cleared them). Then wait for
    // delete to complete so step 5's namespace delete doesn't see
    // dangling CRs.
    ui::step("strip pool drain finalizers", || async {
        for &(ns, kind) in POOL_KINDS {
            k_patch_all(ns, kind, r#"{"metadata":{"finalizers":[]}}"#).await?;
            k(&[
                "-n",
                ns,
                "wait",
                "--for=delete",
                kind,
                "--all",
                "--timeout=120s",
            ])
            .await?;
        }
        Ok(())
    })
    .await?;

    // ── 4. Delete Karpenter NodeClaims ─────────────────────────────
    // Cluster-scoped. With NodePools gone (helm uninstall), Karpenter
    // will already be terminating these — we wait so tofu destroy
    // doesn't pull the controller while EC2 instances are mid-drain.
    // 600s: builder nodes can take a while to drain under load.
    ui::step("wait for Karpenter NodeClaims to terminate", || async {
        k(&[
            "delete",
            "nodeclaim",
            "--all",
            "--wait=true",
            "--timeout=600s",
            "--ignore-not-found",
        ])
        .await
    })
    .await?;

    // ── 5a. Delete xtask-applied resources ─────────────────────────
    ui::step("delete security-profiles-operator", || async {
        let spo = repo_root().join("infra/k8s/security-profiles-operator.yaml");
        let spo = spo.to_str().unwrap();
        // spod CR first (it has its own finalizer that the operator
        // clears), then the manifest. `--wait=false` — the namespace
        // delete below blocks for us.
        k(&[
            "-n",
            "security-profiles-operator",
            "delete",
            "securityprofilesoperatordaemon",
            "spod",
            "--ignore-not-found",
            "--wait=true",
            "--timeout=120s",
        ])
        .await?;
        k(&["delete", "--ignore-not-found", "--wait=false", "-f", spo]).await?;
        // Namespace delete catches anything the manifest left behind
        // (and the spod-config.yaml CR is namespaced here too).
        k(&[
            "delete",
            "ns",
            "security-profiles-operator",
            "--ignore-not-found",
            "--wait=true",
            "--timeout=180s",
        ])
        .await
    })
    .await?;

    ui::step("delete envoy-gateway (if installed)", || async {
        // helm-managed, separate release from `rio`.
        let _ = helm::uninstall("envoy-gateway", "envoy-gateway-system");
        k(&[
            "delete",
            "ns",
            "envoy-gateway-system",
            "--ignore-not-found",
            "--wait=true",
            "--timeout=180s",
        ])
        .await
    })
    .await?;

    // ── 5b. Delete rio namespaces ──────────────────────────────────
    // xtask created them (deploy uses namespace.create=false), so helm
    // uninstall did NOT remove them. The SSH/JWT secrets, headless
    // services, etc. all go with the namespace — no need to enumerate.
    ui::step("delete rio namespaces", || async {
        for &(ns, _) in NAMESPACES {
            k(&[
                "delete",
                "ns",
                ns,
                "--ignore-not-found",
                "--wait=true",
                "--timeout=300s",
            ])
            .await
            .with_context(|| {
                format!(
                    "namespace {ns} stuck — check `kubectl get ns {ns} -o jsonpath={{.spec.finalizers}}` \
                     and `kubectl get all,pvc -n {ns}`; force with \
                     `kubectl get ns {ns} -o json | jq '.spec.finalizers=[]' | \
                     kubectl replace --raw /api/v1/namespaces/{ns}/finalize -f -`"
                )
            })?;
        }
        Ok(())
    })
    .await?;

    // ── 5c. Delete rio CRDs ────────────────────────────────────────
    // xtask `apply CRDs` SSA'd these from infra/helm/crds/ — helm
    // uninstall doesn't touch them. Harmless to leave, but a clean
    // `up` after `destroy` shouldn't show drift.
    ui::step("delete rio CRDs", || async {
        let dir = repo_root().join("infra/helm/crds");
        let dir = dir.to_str().unwrap();
        k(&["delete", "--ignore-not-found", "--wait=false", "-f", dir]).await?;
        // Legacy CRD names from before the BuilderPool/FetcherPool
        // rename — won't be in infra/helm/crds/ on a current checkout,
        // so delete by name. --ignore-not-found makes this free on a
        // fresh cluster.
        k(&[
            "delete",
            "crd",
            "--ignore-not-found",
            "--wait=false",
            "workerpools.rio.build",
            "workerpoolsets.rio.build",
            "builds.rio.build",
        ])
        .await
    })
    .await?;

    tofu_destroy().await
}

/// Step 6, extracted so the cluster-unreachable early-return at the top
/// of `run()` can call it directly. Also sweeps orphaned `available`
/// VPC-CNI ENIs first — Karpenter-provisioned nodes terminated by tofu
/// (rather than via Karpenter's own deprovisioning) leave their pod
/// ENIs detached but undeleted; subnet/SG delete then fails on
/// DependencyViolation after a 20m wait.
async fn tofu_destroy() -> Result<()> {
    // ── 6a. Sweep leaked VPC-CNI ENIs ─────────────────────────────
    // Only if tofu state still has the VPC. ENIs with description
    // prefix `aws-K8S-` and status=available are pod ENIs leaked by
    // ungraceful node termination. Safe to delete: detached, no
    // instance attachment.
    if let Ok(vpc) = tofu::output(TF_DIR, "vpc_id") {
        ui::step("sweep leaked ENIs + aws-lbc SGs", || async {
            let region = tofu::output(TF_DIR, "region").unwrap_or_else(|_| "us-east-2".into());
            let conf = aws_config::from_env()
                .region(aws_config::Region::new(region))
                .load()
                .await;
            let ec2 = aws_sdk_ec2::Client::new(&conf);
            let vpc_filter = aws_sdk_ec2::types::Filter::builder()
                .name("vpc-id")
                .values(&vpc)
                .build();

            // VPC-CNI pod ENIs leaked by ungraceful node termination:
            // description prefix `aws-K8S-`, status=available (detached).
            let enis = ec2
                .describe_network_interfaces()
                .filters(vpc_filter.clone())
                .filters(
                    aws_sdk_ec2::types::Filter::builder()
                        .name("status")
                        .values("available")
                        .build(),
                )
                .send()
                .await?;
            for eni in enis.network_interfaces() {
                let desc = eni.description().unwrap_or_default();
                let Some(id) = eni.network_interface_id() else {
                    continue;
                };
                if !desc.starts_with("aws-K8S-") {
                    continue;
                }
                info!("deleting leaked ENI {id} ({desc})");
                if let Err(e) = ec2
                    .delete_network_interface()
                    .network_interface_id(id)
                    .send()
                    .await
                {
                    warn!("delete ENI {id}: {e}");
                }
            }

            // aws-load-balancer-controller's `k8s-traffic-*` backend SG
            // — controller is gone before the Service finalizer clears.
            // Tofu doesn't manage it.
            let sgs = ec2
                .describe_security_groups()
                .filters(vpc_filter)
                .send()
                .await?;
            for sg in sgs.security_groups() {
                let name = sg.group_name().unwrap_or_default();
                let Some(id) = sg.group_id() else { continue };
                if !name.starts_with("k8s-") {
                    continue;
                }
                info!("deleting leaked aws-lbc SG {id} ({name})");
                if let Err(e) = ec2.delete_security_group().group_id(id).send().await {
                    warn!("delete SG {id}: {e}");
                }
            }
            Ok(())
        })
        .await?;
    }

    // ── 6b. tofu destroy ──────────────────────────────────────────
    // S3 force_destroy=true, ECR force_delete=true, RDS
    // skip_final_snapshot=true are set in the .tf files. Aurora's
    // deletion_protection defaults false (not overridden in rds.tf).
    // The tofu helm provider may log "Kubernetes cluster unreachable"
    // for in-cluster releases once the EKS module deletes the cluster
    // — provider-level destroy of a helm_release with the cluster
    // gone still removes it from state, so this is benign.
    ui::step(
        "tofu destroy (EKS, VPC, RDS, S3, ECR, IAM, helm addons)",
        || async {
            // tofu's own progress streams through (sh::run_sync at -v;
            // captured + last-line tailed at default verbosity). 20-40
            // minutes for an EKS+RDS+NAT teardown is normal.
            tofu::destroy(TF_DIR)
        },
    )
    .await
}

/// Poll for `kubectl get nodeclaim` to be empty. Unused — `kubectl
/// delete --wait` handles this — but kept as a typed escape hatch in
/// case Karpenter's finalizer wedges (drop to `--wait=false` above and
/// call this with a longer timeout).
#[allow(dead_code)]
async fn wait_nodeclaims_gone(timeout: Duration) -> Result<()> {
    ui::poll(
        "NodeClaims terminated",
        Duration::from_secs(10),
        (timeout.as_secs() / 10) as u32,
        || async {
            let sh = shell()?;
            // try_read: error message — not stderr dump — on failure.
            let out = sh::try_read(cmd!(
                sh,
                "kubectl get nodeclaim --no-headers --ignore-not-found"
            ))
            .unwrap_or_default();
            Ok(out.trim().is_empty().then_some(()))
        },
    )
    .await
}
