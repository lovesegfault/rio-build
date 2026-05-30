//! `cargo xtask replay record` — apply the recorder Job for one Hydra evaluation.
//!
//! The in-cluster engine (`rio-replay eval`) does the actual work and
//! publishes a v1 replay archive under
//! `s3://<chunk-bucket>/replay/archives/<archive-id-short>/` (plus the
//! recorder's by-recipe idempotency pointer under
//! `replay/archives/by-recipe/`); xtask only verifies prerequisites
//! (image pushed, namespace + IRSA ServiceAccount) and applies the
//! one-shot Job built by [`super::jobs::eval_job`].

use anyhow::Result;
use clap::Args;
use sha2::{Digest, Sha256};

use super::jobs::{self, EngineJobCommon};
use crate::k8s::client as kclient;
use crate::k8s::eks::{TF_DIR, push};
use crate::{git, tofu, ui};

/// In-pod output directory for the recorder's local artifacts (staging
/// directory, fidelity report, packed `archive.dwarfs`), on the eval
/// Job's `/work` scratch emptyDir ([`jobs::WORK_DIR`]). The engine
/// appends `<hydra-eval-id>/<recipe-short-digest>/` itself and publishes
/// the packed archive to the bucket named by the Job's
/// `RIO_REPLAY_S3_BUCKET` env.
const ENGINE_OUT_DIR: &str = "/work/evalsets";

#[derive(Args)]
pub struct EvalArgs {
    /// Hydra evaluation id (e.g. 1824219).
    #[arg(long)]
    pub eval: u64,
    /// System(s) to evaluate. Repeatable.
    #[arg(long = "system", default_values_t = [String::from("x86_64-linux")])]
    pub systems: Vec<String>,
    /// Scope selector, passed through to the engine verbatim:
    /// `constituents:<aggregate-job>` (e.g. `constituents:tested`) or
    /// `jobs:<job1,job2,…>`. The engine also accepts `jobs-file:<path>`
    /// (the path must exist inside the eval pod) and `full` (currently
    /// refused until full-evaluation sets are supported).
    #[arg(long)]
    pub scope: String,
    /// project/jobset override for when the Hydra eval JSON lacks the
    /// keys (e.g. nixos/trunk-combined).
    #[arg(long)]
    pub jobset: Option<String>,
    /// Size the Job for a full nixpkgs/NixOS evaluation
    /// (r8a.48xlarge-class: ~160 vCPU / 1.2Ti / 400Gi scratch).
    #[arg(long)]
    pub full_scale: bool,
    /// Tell the engine to record a new archive even if this recipe has
    /// already been recorded (the engine salts the recipe digest, which
    /// bypasses the by-recipe idempotency skip; published archives are
    /// write-once, so the re-record lands under a fresh archive id). The
    /// Job name does NOT change: re-running an identical request while
    /// the previous Job still exists collides on it deliberately (delete
    /// that Job or wait for its 24h TTL) so double-evaluations are always
    /// explicit.
    #[arg(long)]
    pub force: bool,
    /// RUST_LOG for the eval pod.
    #[arg(long, default_value = "info,rio_replay=debug")]
    pub log_level: String,
}

/// Engine argv for the eval Job. Flag names live only here and in the
/// engine's clap definition (`rio-replay/src/cmd/eval.rs`): the engine
/// takes `--hydra-eval` / `--systems` / a mandatory `--scope` /
/// `--out-dir`; the S3 bucket comes from the Job's
/// `RIO_REPLAY_S3_BUCKET` env, not the argv.
pub fn engine_args(a: &EvalArgs) -> Vec<String> {
    let mut args = vec!["eval".into(), "--hydra-eval".into(), a.eval.to_string()];
    for s in &a.systems {
        args.extend(["--systems".into(), s.clone()]);
    }
    args.extend(["--scope".into(), a.scope.clone()]);
    if let Some(jobset) = &a.jobset {
        args.extend(["--jobset".into(), jobset.clone()]);
    }
    // Artifacts (drv archive included) land on the Job's sized /work
    // scratch volume; the engine creates the directory itself.
    args.extend(["--out-dir".into(), ENGINE_OUT_DIR.into()]);
    if a.force {
        args.push("--force".into());
    }
    args
}

/// Job name: `replay-eval-<eval>-<8 hex of the request digest>`. The
/// digest covers what the operator asked for (systems/scope/jobset) plus
/// the image tag, so "same request" re-runs collide on the existing Job
/// (409 → guidance) instead of silently double-evaluating, while a new
/// scope or new engine build gets a fresh Job. Systems are sorted and
/// deduplicated before hashing so flag order can't mint a second Job for
/// the same request. `--force` deliberately does NOT change the name:
/// a forced rebuild of the same request still collides with a leftover
/// Job (the 409 guidance says what to delete; finished Jobs expire after
/// 24h via ttlSecondsAfterFinished). K8s names cap at 63 chars; this
/// stays well under.
pub fn job_name(a: &EvalArgs, image_tag: &str) -> String {
    let mut systems: Vec<&str> = a.systems.iter().map(String::as_str).collect();
    systems.sort_unstable();
    systems.dedup();
    let mut h = Sha256::new();
    h.update(systems.join(","));
    h.update("\0");
    h.update(&a.scope);
    h.update("\0");
    h.update(a.jobset.as_deref().unwrap_or(""));
    h.update("\0");
    h.update(image_tag);
    let digest = hex::encode(&h.finalize()[..4]);
    format!("replay-eval-{}-{digest}", a.eval)
}

pub async fn run(a: EvalArgs) -> Result<()> {
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let ecr = tf.get("ecr_registry")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let role_arn = tf.get("replay_iam_role_arn")?;

    // The Job pulls <ecr>/rio-replay:<tag> for the CURRENT tree; refuse
    // before creating anything if that tag was never pushed.
    let tag = git::image_tag(&git::open()?)?;
    ui::step("rio-replay image in ECR", || {
        push::assert_in_ecr("rio-replay", &tag, &region)
    })
    .await?;

    let client = kclient::client().await?;
    ui::step("rio-replay namespace + ServiceAccount", || {
        jobs::ensure_base(&client, &role_arn)
    })
    .await?;

    let common = EngineJobCommon {
        image: format!("{ecr}/rio-replay:{tag}"),
        s3_bucket: bucket.clone(),
        region,
        log_level: a.log_level.clone(),
    };
    let name = job_name(&a, &tag);
    let job = jobs::eval_job(&common, &name, &engine_args(&a), a.full_scale)?;
    ui::step(&format!("apply Job {name}"), || {
        jobs::create_job(&client, &job)
    })
    .await?;

    let ns = super::NS_REPLAY;
    let prefix = super::s3::archives_prefix();
    tracing::info!(
        "eval Job applied.\n  \
         follow logs:     kubectl -n {ns} logs -f job/{name}\n  \
         wait for it:     kubectl -n {ns} wait --for=condition=complete job/{name} --timeout=3h\n  \
         archives land under s3://{bucket}/{prefix}\n  \
         find it: aws s3 ls s3://{bucket}/{prefix} — a prefix is complete once complete.json \
         exists; `replay launch --eval {}` discovers it from the archive's provenance (use \
         --eval-digest <recipe digest> to disambiguate)\n  \
         full-scope evals need ~1-2h on an r8a.48xlarge-class node (~$15-31), and with \
         --full-scale the pod sits Pending for a few minutes while Karpenter provisions that \
         node — that is normal; scoped evals take minutes",
        a.eval
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> EvalArgs {
        EvalArgs {
            eval: 1824219,
            systems: vec!["x86_64-linux".into()],
            scope: "constituents:tested".into(),
            jobset: Some("nixos/trunk-combined".into()),
            full_scale: false,
            force: false,
            log_level: "info".into(),
        }
    }

    #[test]
    fn engine_args_round_trip_the_request() {
        let got = engine_args(&args());
        assert_eq!(
            got,
            vec![
                "eval",
                "--hydra-eval",
                "1824219",
                "--systems",
                "x86_64-linux",
                "--scope",
                "constituents:tested",
                "--jobset",
                "nixos/trunk-combined",
                "--out-dir",
                ENGINE_OUT_DIR,
            ]
        );
        // --force is request-changing and only appended when asked for.
        let mut forced = args();
        forced.force = true;
        assert_eq!(
            engine_args(&forced).last().map(String::as_str),
            Some("--force")
        );
    }

    #[test]
    fn engine_out_dir_is_on_the_scratch_volume() {
        // The recorder's staging directory and packed archive image must
        // land on the sized /work emptyDir, not the container layer.
        assert!(ENGINE_OUT_DIR.starts_with(&format!("{}/", jobs::WORK_DIR)));
    }

    #[test]
    fn job_name_is_stable_and_request_sensitive() {
        let a = args();
        let n1 = job_name(&a, "abc123");
        let n2 = job_name(&a, "abc123");
        assert_eq!(n1, n2);
        assert!(n1.starts_with("replay-eval-1824219-"), "{n1}");
        assert!(n1.len() <= 63, "{n1}");
        // Different scope or different image tag ⇒ different Job name.
        let mut b = args();
        b.scope = "jobs:nixpkgs.hello.x86_64-linux".into();
        assert_ne!(job_name(&b, "abc123"), n1);
        assert_ne!(job_name(&a, "def456"), n1);
        // Same request, different --system order/duplication ⇒ SAME Job
        // name: the digest is over the system set, not the flag spelling.
        let mut c = args();
        c.systems = vec!["aarch64-linux".into(), "x86_64-linux".into()];
        let mut d = args();
        d.systems = vec![
            "x86_64-linux".into(),
            "aarch64-linux".into(),
            "aarch64-linux".into(),
        ];
        assert_eq!(job_name(&c, "abc123"), job_name(&d, "abc123"));
        assert_ne!(job_name(&c, "abc123"), n1);
        // --force keeps the name (collide-and-instruct is deliberate).
        let mut forced = args();
        forced.force = true;
        assert_eq!(job_name(&forced, "abc123"), n1);
    }
}
