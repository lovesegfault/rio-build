//! rio-parity namespace/SA bootstrap and the campaign / eval Job builders.
//!
//! Jobs are built here as typed `batch/v1 Job` objects and created with
//! `Api::create` (NOT helm): a campaign or eval run is per-invocation
//! operational state (operator-chosen args, one Job per campaign id /
//! eval request), while the helm chart owns only the long-lived
//! enablement — the scheduler/store CiliumNetworkPolicy admissions and
//! the campaign tenants' build-policy defaults behind `parity.enabled`
//! (infra/helm/rio-build/values.yaml). Those CNP entries admit the
//! engine by namespace + the `app.kubernetes.io/name: rio-parity` pod
//! label, so every pod template built here carries that label and runs
//! in [`NS_PARITY`]; drop either and the engine's scheduler/store
//! traffic is silently denied.
//!
//! Both Job shapes give the engine a `/work` emptyDir and point
//! HOME / XDG_CACHE_HOME / TMPDIR at it — the image itself only
//! guarantees `/tmp`, and the engine's nix/ssh/tar children need their
//! caches and temp files on the sized scratch volume. Everything
//! campaign-specific (cluster endpoints, tenants, deadline, knobs)
//! reaches the engine through its argv and the mounted campaign-spec
//! ConfigMap, not through env vars: the engine reads no `RIO_*` env
//! beyond the eval CLI's `RIO_PARITY_S3_BUCKET` default.

use std::collections::BTreeMap;

use ::kube::api::{Api, Patch, PatchParams, PostParams};
use anyhow::{Result, bail};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde_json::json;

use super::{NS_PARITY, SA_PARITY};
use crate::k8s::client as kube;

/// Secret holding the per-tenant SSH private keys (created/merged by
/// `parity launch`, one key per campaign tenant).
pub const SSH_SECRET_NAME: &str = "rio-parity-ssh";

/// Where the campaign Job mounts [`SSH_SECRET_NAME`]. The launch-written
/// campaign spec records this directory as `cluster.ssh_key_dir` (the engine
/// derives per-tenant key paths from it) and points the build tenant's
/// ssh-ng store URL at `<dir>/<tenant>` via the `ssh-key=` query parameter
/// (see [`ssh_key_path`]).
pub const SSH_KEY_MOUNT_DIR: &str = "/etc/rio/parity-ssh";

/// Per-tenant SSH private-key path inside the campaign pod: the
/// [`SSH_SECRET_NAME`] data key for `tenant`, mounted under
/// [`SSH_KEY_MOUNT_DIR`] — what the launch-written spec records in the build
/// tenant's store URL `ssh-key=` query parameter (other tenants' keys are
/// resolved by the engine from `cluster.ssh_key_dir`).
pub fn ssh_key_path(tenant: &str) -> String {
    format!("{SSH_KEY_MOUNT_DIR}/{tenant}")
}

/// Service-HMAC Secret in the parity namespace (copied from the
/// rio-system Secret of the same name by `parity launch`).
pub const HMAC_SECRET_NAME: &str = "rio-service-hmac";

// The HMAC mount dir and key filename are spelled as macros so
// [`HMAC_KEY_MOUNT_PATH`] can be composed from them at compile time
// (`concat!` only takes literals) — the dir, the filename, and the full
// path can never drift apart.
macro_rules! hmac_mount_dir {
    () => {
        "/etc/rio/hmac"
    };
}
macro_rules! hmac_key_filename {
    () => {
        "service-hmac.key"
    };
}

/// Where the campaign Job mounts [`HMAC_SECRET_NAME`].
pub const HMAC_MOUNT_DIR: &str = hmac_mount_dir!();

/// Data key of [`HMAC_SECRET_NAME`] (and therefore the file name under
/// [`HMAC_MOUNT_DIR`]) — the same key the chart's serviceHmac mounts
/// use; `parity launch` copies the rio-system Secret's value under this
/// key.
pub const HMAC_KEY_FILENAME: &str = hmac_key_filename!();

/// HMAC key file path inside the campaign pod
/// ([`HMAC_MOUNT_DIR`]/[`HMAC_KEY_FILENAME`]) — what the launch-written
/// spec records in `cluster.service_hmac_key_path`.
pub const HMAC_KEY_MOUNT_PATH: &str = concat!(hmac_mount_dir!(), "/", hmac_key_filename!());

/// Scratch volume mount point; also HOME for the engine and its
/// nix/ssh/tar children.
pub const WORK_DIR: &str = "/work";

/// Where the campaign Job mounts the campaign-spec ConfigMap.
pub const SPEC_MOUNT_DIR: &str = "/etc/rio/parity";

/// ConfigMap key (and therefore file name) of the campaign spec.
pub const SPEC_FILENAME: &str = "spec.json";

/// In-pod path of the campaign spec — what `parity launch` passes to the
/// engine as `run --spec <path>`.
pub const SPEC_MOUNT_PATH: &str = "/etc/rio/parity/spec.json";

/// ConfigMap holding `<campaign-id>`'s spec (created by `parity launch`
/// before the Job, mounted at [`SPEC_MOUNT_DIR`]).
pub fn spec_configmap_name(campaign_id: &str) -> String {
    format!("{campaign_id}-spec")
}

/// Values shared by both Job shapes.
pub struct EngineJobCommon {
    /// Full image ref `<ecr-registry>/rio-parity:<tag>`.
    pub image: String,
    /// Chunk bucket name (tofu output `chunk_bucket_name`). Exported as
    /// `RIO_PARITY_S3_BUCKET` — the eval CLI's bucket default; the
    /// campaign engine reads `s3.bucket` from its spec instead.
    pub s3_bucket: String,
    /// AWS region (tofu output `region`) for the engine's S3 client.
    pub region: String,
    /// `RUST_LOG` for the engine pod.
    pub log_level: String,
}

/// Environment shared by both Job shapes. Everything campaign-specific
/// stays in the spec / argv (see the module doc).
fn common_env(c: &EngineJobCommon) -> serde_json::Value {
    json!([
        {"name": "RUST_LOG", "value": c.log_level},
        {"name": "AWS_REGION", "value": c.region},
        // IPv6-only pod network: the default AWS endpoints are v4-only,
        // dualstack endpoints have AAAA records (same setting as the
        // chart's bootstrap Job).
        {"name": "AWS_USE_DUALSTACK_ENDPOINT", "value": "true"},
        // Default for the eval CLI's --s3-bucket; the run subcommand
        // takes the bucket from spec.s3.bucket instead (harmless there).
        {"name": "RIO_PARITY_S3_BUCKET", "value": c.s3_bucket},
        // Single-user nix, ssh, and tar children key off HOME /
        // XDG_CACHE_HOME / TMPDIR; point them at the /work emptyDir so
        // caches and temp files land on the sized scratch volume, not
        // the node's container layer. TMPDIR is /work itself: nothing
        // creates a /work/tmp subdirectory, and a TMPDIR naming a
        // missing directory breaks every mkstemp call.
        {"name": "HOME", "value": WORK_DIR},
        {"name": "XDG_CACHE_HOME", "value": format!("{WORK_DIR}/.cache")},
        {"name": "TMPDIR", "value": WORK_DIR},
    ])
}

/// Labels for the Jobs, their pod templates, and the campaign-spec
/// ConfigMap `parity launch` applies next to the campaign Job. The
/// pod-level `app.kubernetes.io/name: rio-parity` is what the chart's
/// CiliumNetworkPolicies match (see the module doc).
pub fn labels(component: &str) -> BTreeMap<String, String> {
    BTreeMap::from(
        [
            ("app.kubernetes.io/name", "rio-parity"),
            ("app.kubernetes.io/component", component),
            ("app.kubernetes.io/part-of", "rio-build"),
            ("app.kubernetes.io/managed-by", "xtask"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
    )
}

/// Pod-level security context: nonroot (the image's 65532 user) with the
/// fsGroup making the /work emptyDir and the mounted Secrets readable.
fn pod_security() -> serde_json::Value {
    json!({
        "runAsNonRoot": true,
        "runAsUser": 65532,
        "runAsGroup": 65532,
        "fsGroup": 65532,
        "seccompProfile": {"type": "RuntimeDefault"},
    })
}

/// Container-level security context shared by both Job shapes.
fn container_security() -> serde_json::Value {
    json!({
        "allowPrivilegeEscalation": false,
        "capabilities": {"drop": ["ALL"]},
    })
}

/// Campaign Job: the long-running engine `run` (days). 8 vCPU /
/// 16-32Gi, 100Gi emptyDir scratch — the engine fans out submissions
/// and narinfo sweeps but never builds locally.
///
/// Shape decisions, tied to the engine's operational contract:
///
/// - `args` are caller-provided (`run --spec /etc/rio/parity/spec.json
///   …`); the spec ConfigMap ([`spec_configmap_name`]) is mounted at
///   [`SPEC_MOUNT_DIR`], and the spec inside it must pin its
///   `campaign_id` field to `campaign_id` (the Job name) so a
///   rescheduled pod resumes from the campaign's S3 prefix.
/// - NO `activeDeadlineSeconds`: the campaign deadline is enforced by
///   the engine (deadline → explicitly-partial report, exit 0); a
///   pod-level deadline would kill the pod mid-drain instead of letting
///   the partial report render.
/// - `restartPolicy: OnFailure` restarts the container in the SAME pod,
///   so the /work emptyDir (state dir, downloaded replay archive, caches)
///   survives engine crashes; only a reschedule loses it, and the S3
///   sync plus the pinned campaign id cover that. The generous
///   `backoffLimit` keeps node loss from failing the Job outright.
/// - `karpenter.sh/do-not-disrupt` + the general node role keep
///   Karpenter from consolidating the node under a multi-day pod.
pub fn campaign_job(c: &EngineJobCommon, campaign_id: &str, args: &[String]) -> Result<Job> {
    let job = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": campaign_id,
            "namespace": NS_PARITY,
            "labels": labels("parity-campaign"),
        },
        "spec": {
            // Container restarts resume in place (same emptyDir); pod
            // reschedules resume from the synced S3 state. A generous
            // backoffLimit keeps node loss from failing the Job.
            "backoffLimit": 6,
            "template": {
                "metadata": {
                    "labels": labels("parity-campaign"),
                    // Karpenter must not consolidate the node under a
                    // multi-day campaign pod.
                    "annotations": {"karpenter.sh/do-not-disrupt": "true"},
                },
                "spec": {
                    "serviceAccountName": SA_PARITY,
                    "restartPolicy": "OnFailure",
                    "nodeSelector": {"rio.build/node-role": "general"},
                    "securityContext": pod_security(),
                    "containers": [{
                        "name": "engine",
                        "image": c.image,
                        "args": args,
                        // Relative engine defaults (./parity-state) land
                        // on the scratch volume, not the container layer.
                        "workingDir": WORK_DIR,
                        "env": common_env(c),
                        "volumeMounts": [
                            {"name": "work", "mountPath": WORK_DIR},
                            {"name": "campaign-spec", "mountPath": SPEC_MOUNT_DIR, "readOnly": true},
                            {"name": "parity-ssh", "mountPath": SSH_KEY_MOUNT_DIR, "readOnly": true},
                            {"name": "service-hmac", "mountPath": HMAC_MOUNT_DIR, "readOnly": true},
                        ],
                        "resources": {
                            "requests": {"cpu": "8", "memory": "16Gi", "ephemeral-storage": "100Gi"},
                            "limits": {"memory": "32Gi", "ephemeral-storage": "100Gi"},
                        },
                        "securityContext": container_security(),
                    }],
                    "volumes": [
                        {"name": "work", "emptyDir": {"sizeLimit": "100Gi"}},
                        {"name": "campaign-spec", "configMap": {"name": spec_configmap_name(campaign_id)}},
                        // 0400 (decimal 256): the kubelet's fsGroup pass
                        // re-modes Secret files to owner+group read, which
                        // is what lets uid 65532 read them; ssh accepts the
                        // group-readable keys because its strict-permission
                        // check only applies to keys owned by the calling
                        // uid (Secret files are root-owned), and 0400 keeps
                        // "other" off entirely.
                        {"name": "parity-ssh", "secret": {"secretName": SSH_SECRET_NAME, "defaultMode": 256}},
                        {"name": "service-hmac", "secret": {"secretName": HMAC_SECRET_NAME, "defaultMode": 256}},
                    ],
                },
            },
        },
    }))?;
    Ok(job)
}

/// Eval Job: one-shot replay-archive recording. Scoped (M1/M2) shape by
/// default; `full_scale` sizes for a full nixpkgs/NixOS evaluation
/// (r8a.48xlarge-class: ~160 vCPU / 1.2Ti, ephemeral-storage capped at
/// 400Gi of the 500Gi node root volume).
///
/// One-shot semantics: TTL after finished + a small backoffLimit (eval
/// is expensive and has no resume — retry once, then let the operator
/// read the logs). The eval engine only talks to Hydra, the nixpkgs
/// tarball host, cache.nixos.org and S3 — it mounts no campaign
/// Secrets.
pub fn eval_job(
    c: &EngineJobCommon,
    job_name: &str,
    args: &[String],
    full_scale: bool,
) -> Result<Job> {
    // Scoped (M1/M2) vs full-scope sizing. Full scope keeps the
    // ephemeral-storage request at 400Gi, under the 500Gi root volume of
    // the node class it targets; scoped runs fit ordinary general nodes.
    let (cpu, mem_req, mem_lim, eph) = if full_scale {
        ("160", "1200Gi", "1450Gi", "400Gi")
    } else {
        ("16", "64Gi", "96Gi", "200Gi")
    };
    let job = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": job_name,
            "namespace": NS_PARITY,
            "labels": labels("parity-eval"),
        },
        "spec": {
            // Eval is expensive and has no resume — retry once, then let
            // the operator look at the logs. Finished Jobs clean
            // themselves up after a day (the artifacts live in S3).
            "backoffLimit": 1,
            "ttlSecondsAfterFinished": 86400,
            "template": {
                "metadata": {
                    "labels": labels("parity-eval"),
                    // A full-scope eval holds a large node for 1-2h;
                    // don't let Karpenter consolidate it mid-run.
                    "annotations": {"karpenter.sh/do-not-disrupt": "true"},
                },
                "spec": {
                    "serviceAccountName": SA_PARITY,
                    "restartPolicy": "OnFailure",
                    "nodeSelector": {"rio.build/node-role": "general"},
                    "securityContext": pod_security(),
                    "containers": [{
                        "name": "eval",
                        "image": c.image,
                        "args": args,
                        "workingDir": WORK_DIR,
                        "env": common_env(c),
                        "volumeMounts": [{"name": "work", "mountPath": WORK_DIR}],
                        "resources": {
                            "requests": {"cpu": cpu, "memory": mem_req, "ephemeral-storage": eph},
                            "limits": {"memory": mem_lim, "ephemeral-storage": eph},
                        },
                        "securityContext": container_security(),
                    }],
                    "volumes": [{"name": "work", "emptyDir": {"sizeLimit": eph}}],
                },
            },
        },
    }))?;
    Ok(job)
}

/// The IRSA-annotated [`SA_PARITY`] ServiceAccount both Job shapes run
/// as. Pure builder (no I/O) so the IRSA binding and the token-automount
/// posture stay unit-testable; [`ensure_base`] applies it.
fn parity_service_account(role_arn: &str) -> ServiceAccount {
    ServiceAccount {
        metadata: ObjectMeta {
            name: Some(SA_PARITY.into()),
            namespace: Some(NS_PARITY.into()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/part-of".into(), "rio-build".into()),
                ("app.kubernetes.io/managed-by".into(), "xtask".into()),
            ])),
            // IRSA: the EKS pod-identity webhook injects the projected
            // web-identity token off this annotation; the role's trust
            // policy is bound to rio-parity:rio-parity
            // (infra/eks/parity.tf).
            annotations: Some(BTreeMap::from([(
                "eks.amazonaws.com/role-arn".into(),
                role_arn.to_owned(),
            )])),
            ..Default::default()
        },
        // The engine never talks to the k8s API, and IRSA injects its own
        // projected token — the default SA token is not needed (same as
        // the chart's bootstrap Job).
        automount_service_account_token: Some(false),
        ..Default::default()
    }
}

/// Ensure namespace + IRSA-annotated ServiceAccount exist (idempotent,
/// SSA). Both `parity eval` and `parity launch` call this first.
pub async fn ensure_base(client: &kube::Client, role_arn: &str) -> Result<()> {
    kube::ensure_namespace(client, NS_PARITY, false).await?;
    let sa = parity_service_account(role_arn);
    let api: Api<ServiceAccount> = Api::namespaced(client.clone(), NS_PARITY);
    let ssapply = PatchParams::apply("xtask").force();
    api.patch(SA_PARITY, &ssapply, &Patch::Apply(&sa)).await?;
    Ok(())
}

/// Create the Job in [`NS_PARITY`] — the only namespace campaign/eval
/// Jobs run in (the `job` built by [`campaign_job`]/[`eval_job`] already
/// pins its metadata there). A 409 (already exists) is turned into
/// actionable guidance instead of an SSA overwrite (Job templates are
/// immutable).
pub async fn create_job(client: &kube::Client, job: &Job) -> Result<()> {
    let name = job.metadata.name.clone().unwrap_or_default();
    let api: Api<Job> = Api::namespaced(client.clone(), NS_PARITY);
    match api.create(&PostParams::default(), job).await {
        Ok(_) => {
            tracing::info!("created Job {NS_PARITY}/{name}");
            Ok(())
        }
        Err(::kube::Error::Api(ae)) if ae.code == 409 => bail!(
            "Job {NS_PARITY}/{name} already exists (Job templates are immutable). \
             Inspect it with `kubectl -n {NS_PARITY} get job {name}`, then delete it with \
             `kubectl -n {NS_PARITY} delete job {name}` before re-running."
        ),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common() -> EngineJobCommon {
        EngineJobCommon {
            image: "123.dkr.ecr.us-east-2.amazonaws.com/rio-parity:abc123".into(),
            s3_bucket: "rio-build-chunks-deadbeef".into(),
            region: "us-east-2".into(),
            log_level: "info,rio_replay=debug".into(),
        }
    }

    fn pod_spec(job: &Job) -> k8s_openapi::api::core::v1::PodSpec {
        job.spec
            .clone()
            .unwrap()
            .template
            .spec
            .expect("pod spec present")
    }

    fn env_names(spec: &k8s_openapi::api::core::v1::PodSpec) -> Vec<String> {
        spec.containers[0]
            .env
            .clone()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn campaign_job_pins_node_role_and_disruption_and_scratch() {
        let id = "parity-leaf-20260601-ab12";
        let j = campaign_job(&common(), id, &["run".into()]).unwrap();
        assert_eq!(j.metadata.name.as_deref(), Some(id));
        assert_eq!(j.metadata.namespace.as_deref(), Some(NS_PARITY));

        let spec = j.spec.clone().unwrap();
        // The engine owns the deadline (partial report at deadline, exit
        // 0); a pod-level deadline would kill it mid-drain.
        assert_eq!(spec.active_deadline_seconds, None);
        assert_eq!(spec.backoff_limit, Some(6));

        let tmpl_meta = spec.template.metadata.clone().unwrap();
        let ann = tmpl_meta.annotations.unwrap();
        assert_eq!(
            ann.get("karpenter.sh/do-not-disrupt").map(String::as_str),
            Some("true")
        );

        let pod = pod_spec(&j);
        assert_eq!(pod.service_account_name.as_deref(), Some(SA_PARITY));
        assert_eq!(pod.restart_policy.as_deref(), Some("OnFailure"));
        assert_eq!(
            pod.node_selector
                .as_ref()
                .unwrap()
                .get("rio.build/node-role"),
            Some(&"general".to_string())
        );

        let c = &pod.containers[0];
        assert_eq!(c.image.as_deref(), Some(common().image.as_str()));
        assert_eq!(c.args.as_deref(), Some(&["run".to_string()][..]));
        // Relative engine defaults (e.g. --state-dir ./parity-state) must
        // land on the scratch volume, not the container layer.
        assert_eq!(c.working_dir.as_deref(), Some(WORK_DIR));
        let req = c.resources.clone().unwrap().requests.unwrap();
        assert_eq!(req.get("ephemeral-storage").unwrap().0, "100Gi");
        assert_eq!(req.get("cpu").unwrap().0, "8");

        // emptyDir scratch + the spec ConfigMap + the two Secret mounts
        // the engine contract names (per-tenant SSH keys, service HMAC).
        let vols = pod.volumes.clone().unwrap();
        assert!(
            vols.iter()
                .any(|v| v.name == "work" && v.empty_dir.is_some())
        );
        assert!(vols.iter().any(|v| {
            v.name == "campaign-spec"
                && v.config_map
                    .as_ref()
                    .is_some_and(|cm| cm.name == spec_configmap_name(id))
        }));
        assert!(vols.iter().any(|v| {
            v.name == "parity-ssh"
                && v.secret
                    .as_ref()
                    .is_some_and(|s| s.secret_name.as_deref() == Some(SSH_SECRET_NAME))
        }));
        assert!(vols.iter().any(|v| {
            v.name == "service-hmac"
                && v.secret
                    .as_ref()
                    .is_some_and(|s| s.secret_name.as_deref() == Some(HMAC_SECRET_NAME))
        }));
        // Secret files are 0400: fsGroup re-modes them owner+group read
        // for uid 65532, and "other" stays off entirely.
        for name in ["parity-ssh", "service-hmac"] {
            let v = vols.iter().find(|v| v.name == name).unwrap();
            assert_eq!(
                v.secret.as_ref().unwrap().default_mode,
                Some(0o400),
                "volume {name} defaultMode"
            );
        }
        let mounts: Vec<_> = c
            .volume_mounts
            .clone()
            .unwrap()
            .into_iter()
            .map(|m| m.mount_path)
            .collect();
        for path in [WORK_DIR, SPEC_MOUNT_DIR, SSH_KEY_MOUNT_DIR, HMAC_MOUNT_DIR] {
            assert!(mounts.iter().any(|m| m == path), "missing mount {path}");
        }

        // Engine contract env: AWS SDK + scratch-volume pointers only;
        // cluster endpoints / tenants come from the mounted spec.
        let env = env_names(&pod);
        for name in [
            "RUST_LOG",
            "AWS_REGION",
            "AWS_USE_DUALSTACK_ENDPOINT",
            "RIO_PARITY_S3_BUCKET",
            "HOME",
            "XDG_CACHE_HOME",
            "TMPDIR",
        ] {
            assert!(env.iter().any(|e| e == name), "missing env {name}");
        }
    }

    #[test]
    fn eval_job_scoped_vs_full_scale_resources() {
        let name = "parity-eval-1824219-ab12cd34";
        let scoped = eval_job(&common(), name, &["eval".into()], false).unwrap();
        let full = eval_job(&common(), name, &["eval".into()], true).unwrap();

        let req = |j: &Job| {
            pod_spec(j).containers[0]
                .resources
                .clone()
                .unwrap()
                .requests
                .unwrap()
        };
        assert_eq!(req(&scoped).get("cpu").unwrap().0, "16");
        assert_eq!(req(&full).get("cpu").unwrap().0, "160");
        assert_eq!(req(&full).get("memory").unwrap().0, "1200Gi");
        // Keep the eval Job's ephemeral-storage request at 400Gi, under
        // the 500Gi root volume of the node class it targets.
        assert_eq!(req(&full).get("ephemeral-storage").unwrap().0, "400Gi");

        // One-shot semantics: TTL after finished + small backoffLimit;
        // the deadline (if any) is the operator's, not the pod's.
        let spec = scoped.spec.clone().unwrap();
        assert_eq!(spec.ttl_seconds_after_finished, Some(86400));
        assert_eq!(spec.backoff_limit, Some(1));
        assert_eq!(spec.active_deadline_seconds, None);

        // Same node-role pin and Karpenter disruption opt-out as the
        // campaign Job: a full-scope eval holds a large node for 1-2h.
        assert_eq!(
            pod_spec(&scoped)
                .node_selector
                .as_ref()
                .unwrap()
                .get("rio.build/node-role"),
            Some(&"general".to_string())
        );
        let ann = spec.template.metadata.clone().unwrap().annotations.unwrap();
        assert_eq!(
            ann.get("karpenter.sh/do-not-disrupt").map(String::as_str),
            Some("true")
        );

        // Eval Jobs mount no campaign secrets and upload via the
        // RIO_PARITY_S3_BUCKET env the eval CLI reads.
        assert!(
            pod_spec(&scoped)
                .volumes
                .unwrap()
                .iter()
                .all(|v| v.secret.is_none())
        );
        assert!(
            env_names(&pod_spec(&scoped))
                .iter()
                .any(|e| e == "RIO_PARITY_S3_BUCKET")
        );
    }

    #[test]
    fn both_job_shapes_run_nonroot_with_the_cnp_label() {
        let campaign =
            campaign_job(&common(), "parity-leaf-20260601-ab12", &["run".into()]).unwrap();
        let eval = eval_job(
            &common(),
            "parity-eval-1824219-ab12cd34",
            &["eval".into()],
            false,
        )
        .unwrap();
        for j in [&campaign, &eval] {
            assert_eq!(j.metadata.namespace.as_deref(), Some(NS_PARITY));
            // The chart's CNPs admit the engine by namespace + this pod
            // label; without it scheduler/store traffic is dropped.
            let tmpl_labels = j
                .spec
                .clone()
                .unwrap()
                .template
                .metadata
                .unwrap()
                .labels
                .unwrap();
            assert_eq!(
                tmpl_labels
                    .get("app.kubernetes.io/name")
                    .map(String::as_str),
                Some("rio-parity")
            );

            let pod = pod_spec(j);
            assert_eq!(pod.service_account_name.as_deref(), Some(SA_PARITY));
            let psc = pod.security_context.clone().unwrap();
            assert_eq!(psc.run_as_non_root, Some(true));
            assert_eq!(psc.run_as_user, Some(65532));
            assert_eq!(psc.fs_group, Some(65532));
            assert_eq!(
                psc.seccomp_profile.unwrap().type_,
                "RuntimeDefault".to_string()
            );
            let csc = pod.containers[0].security_context.clone().unwrap();
            assert_eq!(csc.allow_privilege_escalation, Some(false));
            assert_eq!(
                csc.capabilities.unwrap().drop,
                Some(vec!["ALL".to_string()])
            );
            assert_eq!(pod.containers[0].working_dir.as_deref(), Some(WORK_DIR));
        }
    }

    #[test]
    fn service_account_pins_irsa_role_and_disables_token_automount() {
        let arn = "arn:aws:iam::123456789012:role/rio-parity";
        let sa = parity_service_account(arn);
        assert_eq!(sa.metadata.name.as_deref(), Some(SA_PARITY));
        assert_eq!(sa.metadata.namespace.as_deref(), Some(NS_PARITY));
        // IRSA: the pod-identity webhook keys off this exact annotation.
        assert_eq!(
            sa.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get("eks.amazonaws.com/role-arn")
                .map(String::as_str),
            Some(arn)
        );
        // IRSA injects its own projected token; the default SA token
        // must stay off.
        assert_eq!(sa.automount_service_account_token, Some(false));
    }

    #[test]
    fn labels_carry_the_full_app_kubernetes_io_set() {
        // launch reuses this exact set on the campaign-spec ConfigMap, so
        // the engine's k8s objects all answer the same label selectors.
        let l = labels("parity-campaign");
        assert_eq!(l.get("app.kubernetes.io/name").unwrap(), "rio-parity");
        assert_eq!(
            l.get("app.kubernetes.io/component").unwrap(),
            "parity-campaign"
        );
        assert_eq!(l.get("app.kubernetes.io/part-of").unwrap(), "rio-build");
        assert_eq!(l.get("app.kubernetes.io/managed-by").unwrap(), "xtask");
    }

    #[test]
    fn spec_mount_constants_are_consistent() {
        assert_eq!(SPEC_MOUNT_PATH, format!("{SPEC_MOUNT_DIR}/{SPEC_FILENAME}"));
        assert_eq!(HMAC_KEY_MOUNT_PATH, "/etc/rio/hmac/service-hmac.key");
        assert_eq!(
            HMAC_KEY_MOUNT_PATH,
            format!("{HMAC_MOUNT_DIR}/{HMAC_KEY_FILENAME}")
        );
        assert_eq!(
            ssh_key_path("parity-leaf"),
            "/etc/rio/parity-ssh/parity-leaf"
        );
        assert_eq!(
            spec_configmap_name("parity-leaf-20260601-ab12"),
            "parity-leaf-20260601-ab12-spec"
        );
    }
}
