//! OpenTofu wrappers. All paths are relative to repo root.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::config::XtaskConfig;
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

/// Plan then apply. Skips apply (and its noisy output) if the plan
/// shows no changes. `-detailed-exitcode` makes `tofu plan` exit 0 for
/// no-diff, 2 for diff, 1 for error.
///
/// `envs` are set on the spawned process (not passed as `-var=`), so
/// secrets routed through `TF_VAR_*` stay out of `ps` listings.
pub async fn apply(
    dir: &str,
    auto: bool,
    vars: &[(&str, &str)],
    envs: &[(&str, &str)],
) -> Result<()> {
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
        .envs(envs.iter().copied())
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
        // Applying a plan file skips tofu's own prompt (it treats the
        // file as pre-approved), so we show the diff and gate here.
        let sh = shell()?;
        let pp = &plan_path;
        #[allow(clippy::disallowed_methods)]
        cmd!(sh, "tofu -chdir={dir} show {pp}").quiet().run()?;
        if !ui::confirm_held("Apply these changes?")? {
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
#[derive(Debug)]
pub struct Outputs(HashMap<String, String>);

impl Outputs {
    /// Look up one output by name. Same friendly error as the old
    /// per-key `output()` for missing keys / empty state.
    pub fn get(&self, name: &str) -> Result<String> {
        self.0.get(name).cloned().with_context(|| {
            format!(
                "tofu output '{name}' missing or state empty — \
                 run `cargo xtask k8s -p eks up --provision` first?"
            )
        })
    }

    /// Optional output: `None` if absent from state OR present-but-empty
    /// (terraform's idiom for "feature disabled" — see
    /// `gateway_dns_fqdn`). Lets deploy run against a state file that
    /// predates the output without forcing a re-provision.
    pub fn get_opt(&self, name: &str) -> Option<String> {
        self.0.get(name).filter(|v| !v.is_empty()).cloned()
    }
}

/// tfstate bucket: `cfg.tfstate_bucket` override, else
/// `rio-tfstate-{account_id}` via `lookup_account`. Factored so the
/// sync ([`ensure_backend_init`] → `aws sts` CLI) and async
/// ([`state_bucket`] → SDK) paths share one naming convention — only
/// the account-id lookup differs. The closure isn't called when the
/// override is set, so both paths skip the network round-trip.
fn resolve_bucket<F: FnOnce() -> Result<String>>(
    cfg: &XtaskConfig,
    lookup_account: F,
) -> Result<String> {
    if let Some(b) = &cfg.tfstate_bucket {
        return Ok(b.clone());
    }
    Ok(format!("rio-tfstate-{}", lookup_account()?))
}

/// Lazy backend init for read-only callers ([`outputs`]). A fresh git
/// worktree has no `infra/eks/.terraform/`, so `tofu output` fails with
/// "Backend initialization required" even though the cluster exists in
/// S3 state. `--ami` / `--deploy` only need to READ outputs and
/// shouldn't require a full `--provision` (= `tofu apply`) just to wire
/// up the backend. `-reconfigure` is safe here — it connects to the S3
/// backend and downloads providers; no state mutation.
///
/// Sync sibling of [`state_bucket`]: shells `aws sts` instead of the
/// SDK so [`outputs`] can stay sync. Bucket-naming convention lives in
/// [`resolve_bucket`].
fn ensure_backend_init(dir: &str) -> Result<()> {
    // `.terraform/` can exist without `terraform.tfstate` (the backend
    // cache) after a partial/failed init — check for the cache file
    // itself, not the directory.
    if sh::repo_root()
        .join(dir)
        .join(".terraform/terraform.tfstate")
        .is_file()
    {
        return Ok(());
    }
    let cfg = XtaskConfig::load()?;
    let bucket = resolve_bucket(&cfg, || {
        let sh = shell()?;
        sh::read(cmd!(
            sh,
            "aws sts get-caller-identity --query Account --output text"
        ))
        .context("resolve AWS account ID for tfstate bucket")
    })?;
    let backend = Backend {
        bucket,
        region: cfg.tfstate_region,
    };
    tracing::debug!(
        "tofu init (fresh worktree, backend s3://{})",
        backend.bucket
    );
    init(dir, &backend)
}

/// `tofu output -json` parsed into a map. ONE process spawn, one S3
/// state read, one AWS-SDK credential resolve. Replaces N×`output -raw`
/// which under an S3 backend hits SSO once per call — at ~10 calls in
/// quick succession that 429s with `TooManyRequestsException` (I-087).
///
/// Auto-runs `tofu init` if `{dir}/.terraform/` is missing (fresh
/// worktree) so read-only phases work without `--provision`. A
/// post-init `output` failure means state is genuinely empty → the
/// "run --provision first" hint is then correct.
pub fn outputs(dir: &str) -> Result<Outputs> {
    ensure_backend_init(dir)?;
    let sh = shell()?;
    let raw = sh::read(cmd!(sh, "tofu -chdir={dir} output -no-color -json"))
        .context("tofu output -json failed — run `cargo xtask k8s -p eks up --provision` first?")?;
    parse_outputs(&raw)
}

/// Parse `tofu output -no-color -json` into the string map [`Outputs`].
///
/// Every scalar (string, number, bool) is coerced to its string form —
/// callers `.parse()` to the type they expect. Composite outputs
/// (arrays/objects) are a HARD ERROR naming the key, never a silent
/// drop: the pre-fix string-only `filter_map` made the numeric
/// `pg_max_connections` / `log_retention_days` outputs invisible, so
/// [`Outputs::get`] reported "missing or state empty — run --provision
/// first?" against a state file that had them — sending the operator
/// at the provisioning layer for a parsing bug. Missing-key errors are
/// only trustworthy if present keys can never vanish in the parse.
fn parse_outputs(raw: &str) -> Result<Outputs> {
    #[derive(serde::Deserialize)]
    struct Out {
        value: serde_json::Value,
    }
    let parsed: HashMap<String, Out> =
        serde_json::from_str(raw.trim()).context("parse tofu output -json")?;
    let map = parsed
        .into_iter()
        .map(|(k, o)| {
            let v = match o.value {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                other => {
                    let kind = match other {
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Object(_) => "object",
                        _ => "null",
                    };
                    bail!(
                        "tofu output '{k}' is a non-scalar ({kind}) — xtask reads scalar \
                         outputs only; flatten it in outputs.tf or add dedicated parsing"
                    )
                }
            };
            Ok((k, v))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    Ok(Outputs(map))
}

/// Single-key convenience wrapper. Spawns one tofu process — fine for
/// isolated lookups; for ≥2 keys, call [`outputs`] once and `.get()`.
pub fn output(dir: &str, name: &str) -> Result<String> {
    outputs(dir)?.get(name)
}

/// Resolve the tfstate bucket: RIO_TFSTATE_BUCKET or rio-tfstate-${account_id}.
pub async fn state_bucket(cfg: &XtaskConfig, aws: &aws_config::SdkConfig) -> Result<String> {
    // Short-circuit on override before the SDK call; resolve_bucket
    // would handle it too but only after constructing the client.
    if cfg.tfstate_bucket.is_some() {
        return resolve_bucket(cfg, || unreachable!("override set"));
    }
    let sts = aws_sdk_sts::Client::new(aws);
    let ident = sts.get_caller_identity().send().await?;
    let account = ident.account().context("no AWS account ID")?.to_owned();
    resolve_bucket(cfg, || Ok(account))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scalar outputs of every JSON type must be readable. Pre-fix,
    /// `parse_outputs` kept only strings and silently dropped the
    /// rest, so the numeric `pg_max_connections` / `log_retention_days`
    /// outputs read as "missing or state empty" at deploy while
    /// sitting right there in the state file.
    #[test]
    fn parse_outputs_reads_all_scalar_types() {
        let raw = r#"{
            "pg_max_connections": {"sensitive": false, "type": "number", "value": 2000},
            "log_retention_days": {"value": 30},
            "deletion_protection": {"value": true},
            "cluster_name": {"value": "rio-eks"}
        }"#;
        let out = parse_outputs(raw).unwrap();
        assert_eq!(out.get("pg_max_connections").unwrap(), "2000");
        assert_eq!(out.get("log_retention_days").unwrap(), "30");
        assert_eq!(out.get("deletion_protection").unwrap(), "true");
        assert_eq!(out.get("cluster_name").unwrap(), "rio-eks");
    }

    /// A composite output must be a hard error naming the key — never
    /// a silent drop. `Outputs::get`'s missing-key error has to mean
    /// MISSING, or its "run --provision first?" hint sends the
    /// operator at the wrong layer.
    #[test]
    fn parse_outputs_rejects_composites_naming_the_key() {
        let raw = r#"{"private_subnet_ids": {"value": ["subnet-a", "subnet-b"]}}"#;
        let err = parse_outputs(raw).unwrap_err().to_string();
        assert!(err.contains("private_subnet_ids"), "got: {err}");
        assert!(err.contains("array"), "got: {err}");
    }
}
