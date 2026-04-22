//! Create/update the S3 tofu state bucket.
//!
//! Self-referential: state lives in the bucket this creates. Chicken-
//! and-egg solved by detecting whether the state object exists in S3:
//! if not, write a transient `backend_override.tf` forcing the local
//! backend, apply to create the bucket, drop the override, then `init
//! -migrate-state` into S3. Idempotent.

use anyhow::Result;
use tracing::info;

use crate::config::XtaskConfig;
use crate::sh::{self, cmd, repo_root, shell};
use crate::tofu::{self, Backend};

const DIR: &str = "infra/eks/bootstrap";

pub async fn run(cfg: &XtaskConfig) -> Result<()> {
    let aws = crate::aws::config(None).await;
    let backend = Backend {
        bucket: tofu::state_bucket(cfg, aws).await?,
        region: cfg.tfstate_region.clone(),
    };

    let s3 = aws_sdk_s3::Client::new(aws);
    let state_exists = s3
        .head_object()
        .bucket(&backend.bucket)
        .key("bootstrap/terraform.tfstate")
        .send()
        .await
        .is_ok();

    let vars = [
        ("bucket_name", backend.bucket.as_str()),
        ("region", backend.region.as_str()),
    ];

    if state_exists {
        info!(
            "state exists at s3://{}/bootstrap/ — normal apply",
            backend.bucket
        );
        tofu::init(DIR, &backend)?;
        tofu::apply(DIR, false, &vars, &[]).await?;
    } else {
        info!("no state in S3 — first-time setup (local apply → migrate)");
        let sh = shell()?;
        let dir = repo_root().join(DIR);

        // Clean slate: a previous account / crashed bootstrap may have
        // left a backend cache or half-migrated local state behind.
        let _ = std::fs::remove_dir_all(dir.join(".terraform"));
        let _ = std::fs::remove_file(dir.join("terraform.tfstate"));
        let _ = std::fs::remove_file(dir.join("terraform.tfstate.backup"));

        // `init -backend=false` does NOT satisfy a declared `backend
        // "s3" {}` — `plan` still demands backend init. An override
        // file replaces the backend block entirely, so tofu genuinely
        // uses local state for the create-bucket apply.
        let override_tf = scopeguard::guard(dir.join("backend_override.tf"), |p| {
            let _ = std::fs::remove_file(p);
        });
        std::fs::write(&*override_tf, "terraform {\n  backend \"local\" {}\n}\n")?;

        sh::run_sync(cmd!(sh, "tofu -chdir={DIR} init -upgrade"))?;
        tofu::apply(DIR, false, &vars, &[]).await?;

        info!("bucket created — migrating local state → S3");
        // Drop override BEFORE migrate so tofu sees the s3 backend again.
        drop(override_tf);
        let (b, r) = (&backend.bucket, &backend.region);
        sh::run_sync(cmd!(
            sh,
            "tofu -chdir={DIR} init -migrate-state -force-copy -backend-config=bucket={b} -backend-config=region={r}"
        ))?;
        let _ = std::fs::remove_file(dir.join("terraform.tfstate"));
        let _ = std::fs::remove_file(dir.join("terraform.tfstate.backup"));
        info!(
            "done — state at s3://{}/bootstrap/terraform.tfstate",
            backend.bucket
        );
    }
    Ok(())
}
