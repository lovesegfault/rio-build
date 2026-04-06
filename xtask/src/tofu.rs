//! OpenTofu wrappers. All paths are relative to repo root.

use anyhow::{Context, Result, bail};

use crate::sh::{self, cmd, shell};
use crate::ui;

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
    sh::run_sync(cmd!(
        sh,
        "tofu -chdir={dir} init -reconfigure -backend-config=bucket={b} -backend-config=region={r}"
    ))
}

/// `tofu init -backend=false` — local state, used for first-time bootstrap.
pub fn init_local(dir: &str) -> Result<()> {
    let sh = shell()?;
    sh::run_sync(cmd!(
        sh,
        "tofu -chdir={dir} init -backend=false -reconfigure"
    ))
}

/// `tofu init -migrate-state` — move local state into S3 after bootstrap.
pub fn init_migrate(dir: &str, backend: &Backend) -> Result<()> {
    let sh = shell()?;
    let (b, r) = (&backend.bucket, &backend.region);
    sh::run_sync(cmd!(
        sh,
        "tofu -chdir={dir} init -migrate-state -force-copy -backend-config=bucket={b} -backend-config=region={r}"
    ))
}

/// tofu plan always runs; tofu apply only on diff. Declared as 2
/// (diff path). No-diff undercounts by 1 — the bar finishes <100%
/// but drift detection tolerates it (early-return, not an error).
pub const APPLY_STEPS: u64 = 2;

/// Plan then apply. Skips apply (and its noisy output) if the plan
/// shows no changes. `-detailed-exitcode` makes `tofu plan` exit 0 for
/// no-diff, 2 for diff, 1 for error.
pub async fn apply(dir: &str, auto: bool, vars: &[(&str, &str)]) -> Result<()> {
    let varflags: Vec<String> = vars.iter().map(|(k, v)| format!("-var={k}={v}")).collect();

    let plan = tempfile::NamedTempFile::new()?;
    let plan_path = plan.path().to_str().unwrap().to_string();

    let has_diff = ui::step("tofu plan", || async {
        let sh = shell()?;
        let vf = &varflags;
        let pp = &plan_path;
        // -detailed-exitcode gives 0/1/2 — need the code, not just
        // pass/fail, so use output() directly (sh.rs allows this via
        // the one intentional escape hatch).
        #[allow(clippy::disallowed_methods)]
        let out = cmd!(
            sh,
            "tofu -chdir={dir} plan -detailed-exitcode -out={pp} {vf...}"
        )
        .quiet()
        .ignore_status()
        .output()?;
        match out.status.code() {
            Some(0) => Ok(false),
            Some(2) => Ok(true),
            _ => {
                #[allow(clippy::disallowed_methods)]
                let err = String::from_utf8_lossy(&out.stderr);
                bail!("tofu plan failed:\n{err}")
            }
        }
    })
    .await?;

    if !has_diff {
        ui::step_skip("tofu apply", "no changes");
        return Ok(());
    }

    if !auto {
        // ONE suspend() across show+confirm. Releasing between them lets
        // the concurrent build spinner (under `k8s up`'s tokio::join!)
        // redraw with ANSI cursor-up and clobber both the diff and the
        // prompt. Applying a plan file skips tofu's own prompt (it
        // treats the file as pre-approved), so we gate here instead.
        let sh = shell()?;
        let pp = &plan_path;
        let approved = ui::suspend(|| -> Result<bool> {
            // Direct .run() because we're already inside suspend() —
            // sh::run_interactive would nest a second one (deadlock).
            #[allow(clippy::disallowed_methods)]
            cmd!(sh, "tofu -chdir={dir} show {pp}")
                .quiet()
                .run()
                .map_err(anyhow::Error::from)?;
            ui::confirm_held("Apply these changes?")
        })?;
        if !approved {
            bail!("tofu apply cancelled");
        }
    }

    ui::step("tofu apply", || async {
        let sh = shell()?;
        let pp = &plan_path;
        sh::run_sync(cmd!(sh, "tofu -chdir={dir} apply {pp}"))
    })
    .await
}

/// Destroy without prompting — the caller (`k8s destroy`) gates with
/// `ui::confirm_destroy` before reaching here.
pub fn destroy(dir: &str) -> Result<()> {
    let sh = shell()?;
    sh::run_sync(cmd!(sh, "tofu -chdir={dir} destroy -auto-approve"))
}

/// `tofu output -raw NAME` with a friendly error.
///
/// `-no-color` because tracing escapes ANSI bytes when these values
/// land in error contexts. With an empty state, `tofu output -raw`
/// exits 0 and prints a "No outputs found" warning to STDOUT (yes,
/// stdout) — treat that as missing rather than returning the blob.
pub fn output(dir: &str, name: &str) -> Result<String> {
    let sh = shell()?;
    let val = sh::read(cmd!(sh, "tofu -chdir={dir} output -no-color -raw {name}")).with_context(
        || format!("tofu output '{name}' missing — run `cargo xtask k8s provision -p eks` first?"),
    )?;
    let val = val.trim();
    if val.is_empty() || val.contains('\n') || val.contains("No outputs found") {
        bail!(
            "tofu output '{name}' missing or state empty — \
             run `cargo xtask k8s provision -p eks` first?"
        );
    }
    Ok(val.to_owned())
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
