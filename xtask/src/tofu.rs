//! OpenTofu wrappers. All paths are relative to repo root.

use anyhow::{Context, Result};

use crate::sh::{cmd, shell};

pub struct Backend {
    pub bucket: String,
    pub region: String,
}

/// `tofu init -reconfigure` with dynamic backend config.
///
/// `-reconfigure`: tofu can't tell the dynamic -backend-config is the
/// same as last time, prompts "migrate?" even though nothing changed.
pub fn init(dir: &str, backend: &Backend) -> Result<()> {
    let sh = shell()?;
    let (b, r) = (&backend.bucket, &backend.region);
    cmd!(
        sh,
        "tofu -chdir={dir} init -reconfigure -backend-config=bucket={b} -backend-config=region={r}"
    )
    .run()?;
    Ok(())
}

/// `tofu init -backend=false` — local state, used for first-time bootstrap.
pub fn init_local(dir: &str) -> Result<()> {
    let sh = shell()?;
    cmd!(sh, "tofu -chdir={dir} init -backend=false -reconfigure").run()?;
    Ok(())
}

/// `tofu init -migrate-state` — move local state into S3 after bootstrap.
pub fn init_migrate(dir: &str, backend: &Backend) -> Result<()> {
    let sh = shell()?;
    let (b, r) = (&backend.bucket, &backend.region);
    cmd!(
        sh,
        "tofu -chdir={dir} init -migrate-state -force-copy -backend-config=bucket={b} -backend-config=region={r}"
    )
    .run()?;
    Ok(())
}

pub fn apply(dir: &str, auto: bool, vars: &[(&str, &str)]) -> Result<()> {
    let sh = shell()?;
    let flags: Vec<String> = auto
        .then_some("-auto-approve".to_string())
        .into_iter()
        .chain(vars.iter().map(|(k, v)| format!("-var={k}={v}")))
        .collect();
    cmd!(sh, "tofu -chdir={dir} apply {flags...}").run()?;
    Ok(())
}

pub fn destroy(dir: &str) -> Result<()> {
    let sh = shell()?;
    cmd!(sh, "tofu -chdir={dir} destroy").run()?;
    Ok(())
}

/// `tofu output -raw NAME` with a friendly error.
pub fn output(dir: &str, name: &str) -> Result<String> {
    let sh = shell()?;
    cmd!(sh, "tofu -chdir={dir} output -raw {name}")
        .quiet()
        .read()
        .with_context(|| {
            format!("tofu output '{name}' missing — run `cargo xtask eks apply` first?")
        })
}

/// Resolve the tfstate bucket: RIO_TFSTATE_BUCKET or rio-tfstate-${account_id}.
pub async fn state_bucket(
    cfg: &crate::config::XtaskConfig,
    aws: &aws_config::SdkConfig,
) -> Result<String> {
    if let Some(b) = &cfg.tfstate_bucket {
        return Ok(b.clone());
    }
    let sts = aws_sdk_sts::Client::new(aws);
    let ident = sts.get_caller_identity().send().await?;
    let account = ident.account().context("no AWS account ID")?;
    Ok(format!("rio-tfstate-{account}"))
}
