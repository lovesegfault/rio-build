//! `cargo xtask replay record` — run the recorder Job for one Hydra evaluation.
//!
//! The in-cluster engine (`rio-replay eval`) does the actual work and
//! publishes a v1 replay archive under
//! `s3://<chunk-bucket>/replay/archives/<archive-id-short>/` (plus the
//! recorder's by-recipe idempotency pointer under
//! `replay/archives/by-recipe/`). xtask verifies prerequisites (image
//! pushed, namespace + IRSA ServiceAccount), applies the one-shot Job
//! built by [`super::jobs::eval_job`], and then — by default — follows
//! the recorder's logs to completion and summarizes the published
//! archive (`--detach` restores the fire-and-forget behavior). The
//! follow is watch-only: interrupting it never cancels the in-cluster
//! Job, and re-running `record` re-attaches to it (the Job name encodes
//! the request digest, so an existing same-name Job IS this request).

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Args;
use rio_replay::archive::s3::{ARCHIVE_COMPLETE_OBJECT, ARCHIVE_IMAGE_OBJECT, CompleteMarker};
use sha2::{Digest, Sha256};

use super::jobs::{self, EngineJobCommon};
use super::{launch, s3};
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

/// How long the log follow waits for the eval pod to start (per attach):
/// the Job is sized for an r8a.48xlarge-class node, so the pod sits
/// Pending while Karpenter provisions one (a few minutes) and then pulls
/// the engine image; 30min covers the bad tail without hanging forever
/// when the pod can never schedule.
const POD_START_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    /// Tell the engine to record a new archive even if this recipe has
    /// already been recorded (the engine salts the recipe digest, which
    /// bypasses the by-recipe idempotency skip; published archives are
    /// write-once, so the re-record lands under a fresh archive id). The
    /// Job name does NOT change: re-running an identical request while
    /// the previous Job still runs re-attaches to it, so double-
    /// evaluations are never minted silently (delete that Job or wait
    /// for its 24h TTL to actually re-evaluate).
    #[arg(long)]
    pub force: bool,
    /// Create the Job and exit immediately with the manual follow/wait
    /// commands instead of following its logs to completion (the
    /// pre-follow fire-and-forget behavior).
    #[arg(long)]
    pub detach: bool,
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
        region: region.clone(),
        log_level: a.log_level.clone(),
    };
    let ns = super::NS_REPLAY;
    let name = job_name(&a, &tag);
    let job = jobs::eval_job(&common, &name, &engine_args(&a))?;
    let outcome = ui::step(&format!("apply Job {name}"), || {
        jobs::try_create_job(&client, &job)
    })
    .await?;
    if outcome == jobs::CreateOutcome::AlreadyExists {
        // The Job name encodes the request digest, so an existing Job with
        // this name IS this request — created by an earlier `record` that
        // detached or was interrupted. Re-attach instead of erroring.
        tracing::info!("re-attaching to existing recording Job {ns}/{name}");
    }

    if a.detach {
        let prefix = s3::archives_prefix();
        tracing::info!(
            "eval Job applied (--detach).\n  \
             follow logs:     kubectl -n {ns} logs -f job/{name}\n  \
             wait for it:     kubectl -n {ns} wait --for=condition=complete job/{name} --timeout=3h\n  \
             archives land under s3://{bucket}/{prefix}\n  \
             find it: aws s3 ls s3://{bucket}/{prefix} — a prefix is complete once complete.json \
             exists; `replay launch --eval {}` discovers it from the archive's provenance (use \
             --eval-digest <recipe digest> to disambiguate)\n  \
             eval Jobs are sized for a full evaluation (r8a.48xlarge-class), so the pod sits \
             Pending for a few minutes while Karpenter provisions that node — that is normal. \
             Full-scope evals need ~1-2h on it (~$15-31); scoped evals take minutes",
            a.eval
        );
        return Ok(());
    }

    // Follow the recorder to completion. Watch-only: Ctrl-C detaches
    // without cancelling the in-cluster Job, and re-running `record`
    // re-attaches to it.
    tracing::info!(
        "following recorder logs (Ctrl-C detaches; the Job keeps running and `replay record` \
         re-attaches — pass --detach for fire-and-forget)"
    );
    let succeeded = kclient::follow_job_logs(&client, ns, &name, POD_START_TIMEOUT).await?;
    ensure!(
        succeeded,
        "recording Job {ns}/{name} failed (its logs are above) — inspect with `kubectl -n {ns} \
         describe job {name}`, then delete it with `kubectl -n {ns} delete job {name}` before \
         re-running"
    );

    // The recorder published the archive (or found this recipe already
    // recorded — the archive may then predate this run) before the Job
    // completed; summarize what is in S3 now.
    let summary = ui::step("fetch archive summary", || {
        archive_summary(&region, &bucket, a.eval)
    })
    .await?;
    tracing::info!("{summary}");
    Ok(())
}

/// Find the most recently published archive recorded from `eval` and
/// render its summary. Listing-based rather than by-recipe: xtask cannot
/// recompute the recipe digest (it covers nix/engine versions only the
/// pod knows), and the recorder may have skipped the upload entirely
/// ("recipe already recorded") — in both cases the newest archive whose
/// provenance names this eval is the one this run produced or reused.
async fn archive_summary(region: &str, bucket: &str, eval: u64) -> Result<String> {
    let candidates = launch::listed_candidates(region, bucket).await?;
    let latest = candidates
        .into_iter()
        .filter(|c| c.hydra_eval_id == eval)
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
        .with_context(|| {
            format!(
                "the recorder Job succeeded but no archive under s3://{bucket}/{} has provenance \
                 naming hydra eval {eval} — check the Job logs for the upload outcome",
                s3::archives_prefix()
            )
        })?;
    // complete.json: per-object sizes + upload metadata. It is uploaded
    // strictly last, so a just-published archive always has it; a missing
    // or malformed marker degrades those summary fields to "?" rather
    // than failing a recording that otherwise succeeded.
    let complete_key = format!("{}/{ARCHIVE_COMPLETE_OBJECT}", latest.s3_prefix);
    let marker: Option<CompleteMarker> = s3::get_text(region, bucket, &complete_key)
        .await?
        .and_then(|text| serde_json::from_str(&text).ok());
    Ok(render_summary(&latest, marker.as_ref(), bucket))
}

/// Render the post-recording summary block: archive identity and S3
/// location, image size and upload time (from the completion marker),
/// scope/counts/capabilities/fidelity (from the manifest), and the launch
/// command that consumes the archive. Pure so it is unit-testable.
fn render_summary(
    candidate: &launch::ArchiveCandidate,
    marker: Option<&CompleteMarker>,
    bucket: &str,
) -> String {
    let manifest = &candidate.manifest;
    let count = |key: &str| manifest["counts"][key].as_u64().unwrap_or(0);
    // Capabilities: the flags the manifest sets true, in document order.
    let capabilities = manifest["capabilities"]
        .as_object()
        .map(|caps| {
            caps.iter()
                .filter(|(_, set)| set.as_bool() == Some(true))
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|joined| !joined.is_empty())
        .unwrap_or_else(|| "none".to_string());
    let (image_size, uploaded_at) = match marker {
        Some(m) => (
            m.objects
                .get(ARCHIVE_IMAGE_OBJECT)
                .map(|digest| s3::human_bytes(digest.size))
                .unwrap_or_else(|| "?".to_string()),
            m.uploaded_at.to_string(),
        ),
        None => ("?".to_string(), "?".to_string()),
    };
    format!(
        "recording complete.\n  \
         archive:        {short} (full id {full})\n  \
         location:       s3://{bucket}/{prefix}/\n  \
         archive.dwarfs: {image_size}, uploaded {uploaded_at}\n  \
         scope:          {scope} ({systems})\n  \
         created:        {created}\n  \
         counts:         {units} workload units, {requests} requests, {outcomes} expected \
         outcomes, {drvs} embedded drvs, {paths} embedded store paths\n  \
         capabilities:   {capabilities}\n  \
         fidelity:       {fidelity}\n  \
         next:           cargo xtask replay launch --eval {eval}",
        short = candidate.archive_id_short,
        full = candidate.archive_id,
        prefix = candidate.s3_prefix,
        scope = candidate.scope_summary(),
        systems = candidate.systems_summary(),
        created = candidate.created_at,
        units = count("workload_units"),
        requests = count("requests"),
        outcomes = count("expected_outcomes"),
        drvs = count("embedded_drvs"),
        paths = count("embedded_store_paths"),
        fidelity = candidate.fidelity_summary(),
        eval = candidate.hydra_eval_id,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rio_replay::archive::schema::MemberDigest;
    use serde_json::json;

    use super::*;

    fn args() -> EvalArgs {
        EvalArgs {
            eval: 1824219,
            systems: vec!["x86_64-linux".into()],
            scope: "constituents:tested".into(),
            jobset: Some("nixos/trunk-combined".into()),
            force: false,
            detach: false,
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
        // --force keeps the name (same request ⇒ same Job ⇒ re-attach).
        let mut forced = args();
        forced.force = true;
        assert_eq!(job_name(&forced, "abc123"), n1);
    }

    /// Candidate shaped like what the recorder publishes for the summary
    /// renderer (the launch tests own the filtering-field coverage).
    fn published_candidate() -> launch::ArchiveCandidate {
        let archive_id = "deadbeef".repeat(8);
        let archive_id_short = archive_id[..16].to_string();
        launch::ArchiveCandidate {
            s3_prefix: s3::archive_prefix(&archive_id_short),
            archive_id: archive_id.clone(),
            archive_id_short,
            recipe_digest: "feedc0de".repeat(8),
            hydra_eval_id: 1824219,
            created_at: "2026-06-01T10:00:00Z".into(),
            manifest: json!({
                "created_at": "2026-06-01T10:00:00Z",
                "capabilities": {
                    "expected_outcomes": true,
                    "output_hashes": true,
                    "dependency_closures": true,
                    "impure_env": false,
                },
                "counts": {
                    "requests": 12,
                    "workload_units": 12,
                    "expected_outcomes": 12,
                    "embedded_drvs": 340,
                    "embedded_store_paths": 7,
                },
                "provenance": {
                    "recipe_digest": "feedc0de".repeat(8),
                    "source": {"kind": "hydra", "hydra_eval_id": 1824219},
                    "fidelity": {"checked": 12, "matched": 12, "divergent": false},
                    "systems": ["x86_64-linux"],
                    "scope": {"kind": "constituents", "aggregate_job": "tested"},
                },
            }),
        }
    }

    #[test]
    fn summary_renders_identity_size_counts_and_next_command() {
        let candidate = published_candidate();
        let marker = CompleteMarker {
            archive_id: candidate.archive_id.clone(),
            archive_id_short: candidate.archive_id_short.clone(),
            objects: BTreeMap::from([
                (
                    ARCHIVE_IMAGE_OBJECT.to_string(),
                    MemberDigest {
                        sha256: "ab".repeat(32),
                        size: 3 * 1024 * 1024 * 1024,
                    },
                ),
                (
                    "manifest.json".to_string(),
                    MemberDigest {
                        sha256: "cd".repeat(32),
                        size: 4096,
                    },
                ),
            ]),
            uploaded_at: "2026-06-01T11:00:00Z".parse().unwrap(),
            uploader: "rio-replay-eval/0.1.0".into(),
        };
        let summary = render_summary(&candidate, Some(&marker), "rio-build-chunks-deadbeef");
        // Identity: short + full id, and the exact S3 location.
        assert!(
            summary.contains("deadbeefdeadbeef (full id deadbeef"),
            "{summary}"
        );
        assert!(
            summary.contains("s3://rio-build-chunks-deadbeef/replay/archives/deadbeefdeadbeef/"),
            "{summary}"
        );
        // Image size (human units) + upload time from the marker.
        assert!(summary.contains("3.0 GiB"), "{summary}");
        assert!(
            summary.contains("uploaded 2026-06-01T11:00:00Z"),
            "{summary}"
        );
        // Scope, systems, counts, capabilities, fidelity from the manifest.
        assert!(
            summary.contains("constituents:tested (x86_64-linux)"),
            "{summary}"
        );
        assert!(
            summary.contains(
                "12 workload units, 12 requests, 12 expected outcomes, 340 embedded drvs, \
                 7 embedded store paths"
            ),
            "{summary}"
        );
        assert!(
            summary.contains("expected_outcomes, output_hashes, dependency_closures"),
            "{summary}"
        );
        // impure_env is false in the manifest → not listed as a capability.
        assert!(!summary.contains("impure_env"), "{summary}");
        assert!(summary.contains("fidelity:       12/12"), "{summary}");
        // The next command pins the eval id the operator just recorded.
        assert!(
            summary.contains("cargo xtask replay launch --eval 1824219"),
            "{summary}"
        );

        // Without a (readable) completion marker the summary still renders,
        // degrading the marker-derived fields to "?".
        let degraded = render_summary(&candidate, None, "rio-build-chunks-deadbeef");
        assert!(
            degraded.contains("archive.dwarfs: ?, uploaded ?"),
            "{degraded}"
        );
        assert!(
            degraded.contains("cargo xtask replay launch --eval 1824219"),
            "{degraded}"
        );
    }
}
