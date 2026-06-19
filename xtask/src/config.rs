//! xtask configuration, loaded from `.env.local` + process env.
//!
//! Same RIO_-prefix convention as the rio-* binaries, so one `.env.local`
//! serves both xtask and `process-compose up`.

use std::path::PathBuf;

use anyhow::Result;
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

    /// Single-arch dev mode (issue #58): when set, `up` skips the
    /// other arch's docker images, AMI targets, and NodePools — drops
    /// the cross-arch nix build + coldsnap upload from `--wipe`'s
    /// critical path. Value is the nix CPU: `x86_64` or `aarch64`.
    /// Unset/unrecognized → multi-arch (production default). Read via
    /// [`XtaskConfig::dev_arch()`], not this field — every consumer
    /// MUST agree on which arch is dropped, and the parsed enum is
    /// the single point of truth. (Field stays `pub` only so struct-
    /// update `..Default::default()` works at call sites.)
    pub dev_arch: Option<String>,

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

/// Deserialize a comma-separated string into `Vec<String>`. The env
/// source hands over the raw env var as a string; this splits it
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

/// Parsed `RIO_DEV_ARCH` (issue #58). All consumers — push (docker
/// images), ami (build/register/assert), deploy (pools[]) — match on
/// this enum so an unrecognized env value degrades to multi-arch
/// EVERYWHERE rather than dropping all arches in one place and none
/// in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevArch {
    X86_64,
    Aarch64,
}

impl DevArch {
    /// Nix system tuple this arch keeps.
    pub const fn nix_system(self) -> &'static str {
        match self {
            DevArch::X86_64 => "x86_64-linux",
            DevArch::Aarch64 => "aarch64-linux",
        }
    }
}

impl XtaskConfig {
    pub fn load() -> Result<Self> {
        // dotenvy loads .env.local into process env; the env source then
        // reads everything RIO_-prefixed (prefix stripped, key lowercased).
        let _ = dotenvy::from_path(sh::repo_root().join(".env.local"));
        Self::from_process_env()
    }

    /// Build from the current process environment only. Split out so the
    /// tests exercise exactly the path `load()` uses, minus the dotenvy
    /// side effect (which a jailed test cannot sandbox). `pub(crate)` so
    /// `Jail`-ed tests in other modules can construct a config without
    /// the repo-root `.env.local` leaking in.
    pub(crate) fn from_process_env() -> Result<Self> {
        Ok(::config::Config::builder()
            // Flat, string-only struct: unlike rio-common's nested Configs,
            // this source deliberately skips `.separator("__")` (no nested
            // fields to address) and `.try_parsing(true)` (every field is a
            // string shape; literal values are what we want).
            .add_source(::config::Environment::with_prefix("RIO").prefix_separator("_"))
            .build()?
            .try_deserialize::<Self>()?)
    }

    /// Parsed `RIO_DEV_ARCH`. Unset or unrecognized → `None` (multi-
    /// arch, production default). Consumers MUST read this rather
    /// than the raw string so they all agree on which arch is dropped.
    pub fn dev_arch(&self) -> Option<DevArch> {
        match self.dev_arch.as_deref() {
            Some("x86_64") => Some(DevArch::X86_64),
            Some("aarch64") => Some(DevArch::Aarch64),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: with no `RIO_LOG_LEVEL` set, `log_level` must
    /// resolve to `RIO_DEBUG` (per-crate debug), not empty/"info".
    /// Live cluster on 2026-04-22 showed `global.logLevel: info`
    /// despite no flag/env override — this test pins the default.
    #[test]
    fn log_level_defaults_to_rio_debug_when_unset() {
        rio_test_support::Jail::expect_with(|jail| {
            // Other RIO_* vars present (typical `.env.local` shape),
            // RIO_LOG_LEVEL deliberately absent.
            jail.set_env("RIO_K8S_PROVIDER", "eks");
            jail.set_env("RIO_PUBLIC_CIDRS", "192.0.2.1/32");
            let cfg = XtaskConfig::from_process_env()?;
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
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("RIO_LOG_LEVEL", "warn");
            let cfg = XtaskConfig::from_process_env()?;
            assert_eq!(cfg.log_level, "warn");
            Ok(())
        });
    }

    #[test]
    fn dev_arch_parses_consistently() {
        rio_test_support::Jail::expect_with(|jail| {
            assert_eq!(XtaskConfig::from_process_env()?.dev_arch(), None);
            jail.set_env("RIO_DEV_ARCH", "x86_64");
            assert_eq!(
                XtaskConfig::from_process_env()?.dev_arch(),
                Some(DevArch::X86_64)
            );
            // Unrecognized → None (multi-arch), NOT a partial filter.
            // Every consumer reads this method, so push/ami/deploy
            // can never disagree on what was dropped.
            jail.set_env("RIO_DEV_ARCH", "amd64");
            assert_eq!(XtaskConfig::from_process_env()?.dev_arch(), None);
            Ok(())
        });
    }
}
