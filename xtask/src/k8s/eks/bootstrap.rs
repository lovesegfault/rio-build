//! Create/update the S3 tofu state bucket.
//!
//! Self-referential: state lives in the bucket this creates. Chicken-
//! and-egg solved by detecting whether the state object exists in S3:
//! if not, init with -backend=false (local state), apply to create the
//! bucket, then migrate local → S3. Idempotent.

use anyhow::Result;
use tracing::info;

use crate::config::XtaskConfig;
use crate::sh::repo_root;
use crate::tofu::{self, Backend};

const DIR: &str = "infra/eks/bootstrap";

pub async fn run(cfg: &XtaskConfig) -> Result<()> {
    let aws = aws_config::load_from_env().await;
    let backend = Backend {
        bucket: tofu::state_bucket(cfg, &aws).await?,
        region: cfg.tfstate_region.clone(),
    };

    let s3 = aws_sdk_s3::Client::new(&aws);
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
        tofu::apply(DIR, false, &vars)?;
    } else {
        info!("no state in S3 — first-time setup (local apply → migrate)");
        tofu::init_local(DIR)?;
        tofu::apply(DIR, false, &vars)?;
        info!("bucket created — migrating local state → S3");
        tofu::init_migrate(DIR, &backend)?;
        let root = repo_root();
        let _ = std::fs::remove_file(root.join(DIR).join("terraform.tfstate"));
        let _ = std::fs::remove_file(root.join(DIR).join("terraform.tfstate.backup"));
        info!(
            "done — state at s3://{}/bootstrap/terraform.tfstate",
            backend.bucket
        );
    }
    Ok(())
}
