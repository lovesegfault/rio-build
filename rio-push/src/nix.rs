//! Nix subprocess wrappers for store path discovery and NAR dumping.
//!
//! Shells out to `nix path-info --json --recursive` for closure discovery
//! and `nix store dump-path` for NAR serialization. Both commands are
//! available in the dev shell and in any environment with a Nix installation.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rio_nix::store_path::StorePath;
use rio_proto::validated::ValidatedPathInfo;
use serde::Deserialize;
use tracing::debug;

/// Timeout for `nix path-info` (closure discovery can be slow for large closures).
const PATH_INFO_TIMEOUT: Duration = Duration::from_secs(300);

/// Timeout for `nix store dump-path` (generous for large NARs).
const DUMP_NAR_TIMEOUT: Duration = Duration::from_secs(600);

/// Path metadata from `nix path-info --json`.
///
/// The JSON output is a `HashMap<String, NixPathInfo>` keyed by store path.
/// The store path itself is the map key, not a field inside the value.
/// Fields match the Nix JSON schema for path-info.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NixPathInfo {
    /// Store path -- populated from the map key by [`discover_closure`],
    /// not deserialized from the JSON value.
    #[serde(skip)]
    pub path: String,
    /// SRI format: `sha256-<base64>`.
    pub nar_hash: String,
    pub nar_size: u64,
    #[serde(default)]
    pub deriver: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub signatures: Vec<String>,
    #[serde(default)]
    pub ca: Option<String>,
}

impl NixPathInfo {
    /// Parse the SRI `narHash` field (`sha256-<base64>`) into raw bytes.
    fn parse_nar_hash(&self) -> anyhow::Result<[u8; 32]> {
        let hash_str = self
            .nar_hash
            .strip_prefix("sha256-")
            .with_context(|| format!("narHash missing sha256- prefix: {:?}", self.nar_hash))?;
        let bytes = BASE64
            .decode(hash_str)
            .with_context(|| format!("narHash base64 decode failed: {hash_str:?}"))?;
        let hash: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("narHash is {} bytes, expected 32", v.len()))?;
        Ok(hash)
    }

    /// Convert to a [`ValidatedPathInfo`] suitable for `chunk_nar_for_put`.
    pub fn to_validated(&self) -> anyhow::Result<ValidatedPathInfo> {
        let store_path = StorePath::parse(&self.path)
            .with_context(|| format!("invalid store path: {:?}", self.path))?;
        let nar_hash = self.parse_nar_hash()?;

        let references = self
            .references
            .iter()
            .map(|r| StorePath::parse(r).with_context(|| format!("invalid reference path: {r:?}")))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let deriver = self
            .deriver
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| StorePath::parse(d).with_context(|| format!("invalid deriver: {d:?}")))
            .transpose()?;

        Ok(ValidatedPathInfo {
            store_path,
            store_path_hash: vec![],
            deriver,
            nar_hash,
            nar_size: self.nar_size,
            references,
            registration_time: 0,
            ultimate: false,
            signatures: self.signatures.clone(),
            content_address: self.ca.clone(),
        })
    }
}

/// Discover the full closure of `paths` by running
/// `nix path-info --recursive --json`.
pub async fn discover_closure(paths: &[String]) -> anyhow::Result<Vec<NixPathInfo>> {
    if paths.is_empty() {
        return Ok(vec![]);
    }

    let mut cmd = tokio::process::Command::new("nix");
    cmd.arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("path-info")
        .arg("--recursive")
        .arg("--json")
        .arg("--")
        .args(paths);

    debug!(?paths, "running nix path-info --recursive --json");
    let output = tokio::time::timeout(PATH_INFO_TIMEOUT, cmd.output())
        .await
        .context("`nix path-info` timed out")?
        .context("failed to run `nix path-info`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`nix path-info` exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let map: HashMap<String, NixPathInfo> =
        serde_json::from_slice(&output.stdout).context("failed to parse nix path-info JSON")?;

    Ok(map
        .into_iter()
        .map(|(k, mut v)| {
            v.path = k;
            v
        })
        .collect())
}

/// Dump a store path as a NAR by running `nix store dump-path`.
pub async fn dump_nar(store_path: &str) -> anyhow::Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new("nix");
    cmd.arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("store")
        .arg("dump-path")
        .arg(store_path);

    debug!(store_path, "running nix store dump-path");
    let output = tokio::time::timeout(DUMP_NAR_TIMEOUT, cmd.output())
        .await
        .context("`nix store dump-path` timed out")?
        .context("failed to run `nix store dump-path`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`nix store dump-path {store_path}` exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sri_hash() {
        let npi = NixPathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0".into(),
            nar_hash: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            nar_size: 1024,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        };
        let hash = npi.parse_nar_hash().unwrap();
        assert_eq!(hash, [0u8; 32]);
    }

    #[test]
    fn parse_sri_hash_missing_prefix() {
        let npi = NixPathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0".into(),
            nar_hash: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            nar_size: 1024,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        };
        assert!(npi.parse_nar_hash().is_err());
    }

    #[test]
    fn to_validated_happy_path() {
        let npi = NixPathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0".into(),
            nar_hash: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            nar_size: 1024,
            deriver: Some("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello.drv".into()),
            references: vec!["/nix/store/cccccccccccccccccccccccccccccccc-glibc-2.40".into()],
            signatures: vec!["cache.nixos.org-1:abc".into()],
            ca: None,
        };
        let v = npi.to_validated().unwrap();
        assert_eq!(v.store_path.as_str(), npi.path);
        assert_eq!(v.nar_hash, [0u8; 32]);
        assert_eq!(v.nar_size, 1024);
        assert!(v.deriver.is_some());
        assert_eq!(v.references.len(), 1);
        assert_eq!(v.signatures, vec!["cache.nixos.org-1:abc"]);
    }

    #[test]
    fn to_validated_no_deriver() {
        let npi = NixPathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0".into(),
            nar_hash: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            nar_size: 512,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        };
        let v = npi.to_validated().unwrap();
        assert!(v.deriver.is_none());
    }

    #[test]
    fn to_validated_bad_path() {
        let npi = NixPathInfo {
            path: "/tmp/evil".into(),
            nar_hash: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            nar_size: 512,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        };
        assert!(npi.to_validated().is_err());
    }
}
