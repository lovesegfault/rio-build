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

#[derive(Debug, Deserialize, Default)]
pub struct XtaskConfig {
    /// Path to the SSH pubkey used for gateway authorized_keys.
    /// Default: `~/.ssh/id_ed25519.pub`.
    pub ssh_pubkey: Option<PathBuf>,

    /// If set, used as the authorized_keys comment (tenant name).
    /// Default: empty (single-tenant mode).
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
}

fn default_tfstate_region() -> String {
    "us-east-2".into()
}
fn default_log_level() -> String {
    "debug".into()
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
