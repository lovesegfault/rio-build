//! OpenTofu wrappers. All paths are relative to repo root.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::sh::{self, cmd, shell};
use crate::ui;

pub struct Backend {
    pub bucket: String,
    pub region: String,
}

/// `tofu init -reconfigure -upgrade` with dynamic backend config.
///
/// `-reconfigure`: tofu can't tell the dynamic -backend-config is the
/// same as last time, prompts "migrate?" even though nothing changed.
///
/// `-upgrade`: `.terraform.lock.hcl` is gitignored — it's per-machine
/// cache. A stale lock can pin a yanked provider version (e.g. aws
/// 6.28.0 was pulled), blocking init with "version no longer
/// available". `-upgrade` re-resolves within the `~> N.M` constraints
/// in main.tf, so it never jumps majors.
pub fn init(dir: &str, backend: &Backend) -> Result<()> {
    let sh = shell()?;
    let (b, r) = (&backend.bucket, &backend.region);
    sh::run_sync(cmd!(
        sh,
        "tofu -chdir={dir} init -reconfigure -upgrade -backend-config=bucket={b} -backend-config=region={r}"
    ))
}

/// `tofu init -backend=false` — local state, used for first-time bootstrap.
pub fn init_local(dir: &str) -> Result<()> {
    let sh = shell()?;
    sh::run_sync(cmd!(
        sh,
        "tofu -chdir={dir} init -backend=false -reconfigure -upgrade"
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

/// All tofu outputs from one `-json` read. See [`outputs`].
pub struct Outputs(HashMap<String, String>);

impl Outputs {
    /// Look up one output by name. Same friendly error as the old
    /// per-key `output()` for missing keys / empty state.
    pub fn get(&self, name: &str) -> Result<String> {
        self.0.get(name).cloned().with_context(|| {
            format!(
                "tofu output '{name}' missing or state empty — \
                 run `cargo xtask k8s provision -p eks` first?"
            )
        })
    }
}

/// `tofu output -json` parsed into a map. ONE process spawn, one S3
/// state read, one AWS-SDK credential resolve. Replaces N×`output -raw`
/// which under an S3 backend hits SSO once per call — at ~10 calls in
/// quick succession that 429s with `TooManyRequestsException` (I-087).
///
/// Empty state → `{}` → `Outputs::get` bails with the standard
/// "missing or state empty" error.
pub fn outputs(dir: &str) -> Result<Outputs> {
    let sh = shell()?;
    let raw = sh::read(cmd!(sh, "tofu -chdir={dir} output -no-color -json"))
        .context("tofu output -json failed — run `cargo xtask k8s provision -p eks` first?")?;
    #[derive(serde::Deserialize)]
    struct Out {
        value: serde_json::Value,
    }
    let parsed: HashMap<String, Out> =
        serde_json::from_str(raw.trim()).context("parse tofu output -json")?;
    let map = parsed
        .into_iter()
        .filter_map(|(k, o)| match o.value {
            serde_json::Value::String(s) => Some((k, s)),
            _ => None,
        })
        .collect();
    Ok(Outputs(map))
}

/// Single-key convenience wrapper. Spawns one tofu process — fine for
/// isolated lookups; for ≥2 keys, call [`outputs`] once and `.get()`.
pub fn output(dir: &str, name: &str) -> Result<String> {
    outputs(dir)?.get(name)
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
