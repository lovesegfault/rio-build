//! xtask configuration, loaded from `.env.local` + process env via figment.
//!
//! Same pattern as rio-* binaries (figment + RIO_ prefix), so one
//! `.env.local` serves both xtask and `process-compose up`.

use std::path::PathBuf;

use anyhow::Result;
use figment::Figment;
use figment::providers::Env;
use serde::Deserialize;

use crate::sh;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtaskConfig {
    /// Path to the SSH pubkey used for gateway authorized_keys.
    /// Default: `~/.ssh/id_ed25519.pub`.
    pub ssh_pubkey: Option<PathBuf>,

    /// If set, used as the authorized_keys comment (tenant name).
    /// Default: `ssh::DEFAULT_TENANT` ("default"). Overridden by
    /// `--deploy-tenant` on `k8s up`.
    pub ssh_tenant: Option<String>,

    /// S3 bucket for tofu state. Default: `rio-tfstate-${account_id}`.
    pub tfstate_bucket: Option<String>,

    /// Region for tofu state bucket. Default: `us-east-2`.
    #[serde(default = "default_tfstate_region")]
    pub tfstate_region: String,

    /// Log level passed to helm `--set global.logLevel=...`.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Remote nix store (ssh-ng://...) for offloading docker image builds.
    pub remote_store: Option<String>,

    /// Source CIDRs allowed to reach the gateway NLB directly. Non-
    /// empty makes deploy emit `aws-load-balancer-scheme: internet-
    /// facing` + `loadBalancerSourceRanges`. Comma-separated in
    /// `.env.local` (`RIO_PUBLIC_CIDRS=1.2.3.4/32,5.6.7.8/32`).
    /// Overridden by `--public-cidr` on `k8s up`.
    #[serde(default, deserialize_with = "csv")]
    pub public_cidrs: Vec<String>,

    /// external-dns provider for the gateway's stable hostname
    /// (`"route53"` / `"cloudflare"` / unset). Passed to tofu as
    /// `var.gateway_dns.provider`; unset → external-dns not installed.
    pub dns_provider: Option<String>,

    /// Parent zone for the gateway hostname (e.g. `rio.example.test`).
    /// Passed to tofu as `var.gateway_dns.zone`.
    pub dns_zone: Option<String>,

    /// Subdomain prefix (e.g. `gw` → `gw.<zone>`); empty/unset → apex.
    /// Passed to tofu as `var.gateway_dns.prefix`.
    pub dns_prefix: Option<String>,

    /// Cloudflare API token (Zone:DNS:Edit scope). Passed to tofu via
    /// `TF_VAR_cloudflare_api_token` env (never CLI) so it stays out
    /// of process listings.
    pub cloudflare_token: Option<String>,
}

/// Deserialize a comma-separated string into `Vec<String>`. figment's
/// Env provider hands over the raw env var as a string; this splits it
/// so `.env.local` can express a list without JSON.
fn csv<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(String::deserialize(d)?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

fn default_tfstate_region() -> String {
    "us-east-2".into()
}

/// `RUST_LOG` directive: info baseline, debug for rio crates only.
///
/// EKS stress testing (I-003) found that a bare `"debug"` captures h2
/// frame-by-frame, rustls handshakes, hyper connection pool churn, sqlx
/// per-query, and kube-client per-request — thousands of lines per second
/// under load, burying the actual rio signal. This directive keeps those
/// infra crates at info while giving full debug visibility into rio code.
pub const RIO_DEBUG: &str = "info,rio_gateway=debug,rio_scheduler=debug,rio_store=debug,rio_builder=debug,rio_controller=debug,rio_common=debug,rio_nix=debug,rio_proto=debug,rio_crds=debug";

fn default_log_level() -> String {
    RIO_DEBUG.into()
}

impl XtaskConfig {
    pub fn load() -> Result<Self> {
        // dotenvy loads .env.local into process env; figment then reads
        // from env with the RIO_ prefix stripped.
        let _ = dotenvy::from_path(sh::repo_root().join(".env.local"));
        Ok(Figment::new()
            .merge(Env::prefixed("RIO_"))
            .extract::<Self>()?)
    }
}

#[cfg(test)]
// figment::Error is 208B, API-fixed — Jail closure signature is set
// by the library (same allow as rio-test-support's macros).
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    /// Regression: with no `RIO_LOG_LEVEL` set, `log_level` must
    /// resolve to `RIO_DEBUG` (per-crate debug), not empty/"info".
    /// Live cluster on 2026-04-22 showed `global.logLevel: info`
    /// despite no flag/env override — this test pins the default.
    #[test]
    fn log_level_defaults_to_rio_debug_when_unset() {
        figment::Jail::expect_with(|jail| {
            // Other RIO_* vars present (typical `.env.local` shape),
            // RIO_LOG_LEVEL deliberately absent.
            jail.set_env("RIO_K8S_PROVIDER", "eks");
            jail.set_env("RIO_PUBLIC_CIDRS", "192.0.2.1/32");
            // Exercise the same figment path as load() (minus the
            // dotenvy side-effect, which Jail can't sandbox).
            let cfg: XtaskConfig = Figment::new().merge(Env::prefixed("RIO_")).extract()?;
            assert_eq!(
                cfg.log_level, RIO_DEBUG,
                "serde default_log_level should fire when RIO_LOG_LEVEL is unset"
            );
            assert_eq!(cfg.tfstate_region, "us-east-2");
            Ok(())
        });
    }

    #[test]
    fn log_level_honors_explicit_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_LOG_LEVEL", "warn");
            let cfg: XtaskConfig = Figment::new().merge(Env::prefixed("RIO_")).extract()?;
            assert_eq!(cfg.log_level, "warn");
            Ok(())
        });
    }
}
