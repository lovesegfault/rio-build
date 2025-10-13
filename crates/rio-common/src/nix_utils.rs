//! Nix integration utilities
//!
//! This module provides functions to interact with Nix:
//! - Query Nix configuration (system, platforms, features)
//! - Run nix-eval-jobs for build evaluation

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

/// Nix configuration extracted from `nix config show`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NixConfig {
    /// Primary system platform (e.g., "x86_64-linux")
    pub system: String,

    /// Additional platforms this system can build for (e.g., ["i686-linux"])
    pub extra_platforms: Vec<String>,

    /// System features available (e.g., ["kvm", "big-parallel", "nixos-test"])
    pub system_features: Vec<String>,
}

impl NixConfig {
    /// Parse Nix configuration from the system
    ///
    /// Runs `nix config show` and extracts:
    /// - `system` - Primary platform
    /// - `extra-platforms` - Additional compatible platforms
    /// - `system-features` - Available features for build matching
    pub async fn parse() -> Result<Self> {
        let output = tokio::process::Command::new("nix")
            .args(&["config", "show"])
            .output()
            .await
            .context("Failed to run 'nix config show'")?;

        if !output.status.success() {
            anyhow::bail!(
                "nix config show failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let config_text = String::from_utf8(output.stdout)
            .context("nix config show output is not valid UTF-8")?;

        let mut system = None;
        let mut extra_platforms = Vec::new();
        let mut system_features = Vec::new();

        // Parse output format: "key = value"
        for line in config_text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some((key, value)) = line.split_once(" = ") {
                let key = key.trim();
                let value = value.trim();

                match key {
                    "system" => {
                        system = Some(value.to_string());
                    }
                    "extra-platforms" => {
                        extra_platforms = value.split_whitespace().map(|s| s.to_string()).collect();
                    }
                    "system-features" => {
                        system_features = value.split_whitespace().map(|s| s.to_string()).collect();
                    }
                    _ => {}
                }
            }
        }

        let system = system.context("'system' not found in nix config")?;

        Ok(Self {
            system,
            extra_platforms,
            system_features,
        })
    }

    /// Get all platforms this system can build for (system + extra_platforms)
    pub fn all_platforms(&self) -> Vec<String> {
        let mut platforms = vec![self.system.clone()];
        platforms.extend(self.extra_platforms.clone());
        platforms
    }
}

/// Result from nix-eval-jobs evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResult {
    /// Attribute name
    pub attr: String,

    /// Path to the derivation file (e.g., /nix/store/abc123-foo.drv)
    pub drv_path: Utf8PathBuf,

    /// Required system platform (e.g., "x86_64-linux")
    pub system: String,

    /// Required system features (e.g., ["kvm", "big-parallel"])
    #[serde(default)]
    pub required_system_features: Vec<String>,

    /// Cache status: "notBuilt", "cached", or "local"
    #[serde(default)]
    pub cache_status: String,

    /// Derivations that need building (not in cache)
    #[serde(default)]
    pub needed_builds: Vec<Utf8PathBuf>,

    /// Paths available in substituters (cache)
    #[serde(default)]
    pub needed_substitutes: Vec<Utf8PathBuf>,
}

impl EvalResult {
    /// Run nix-eval-jobs to evaluate a Nix file
    ///
    /// Returns evaluation results with cache status and build requirements.
    pub async fn from_file(nix_file: &Utf8Path) -> Result<Self> {
        let output = tokio::process::Command::new("nix-eval-jobs")
            .args(&[
                "--check-cache-status",
                "--show-required-system-features",
                "--show-input-drvs",
                nix_file.as_str(),
            ])
            .output()
            .await
            .context("Failed to run 'nix-eval-jobs'")?;

        if !output.status.success() {
            anyhow::bail!(
                "nix-eval-jobs failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json_output =
            String::from_utf8(output.stdout).context("nix-eval-jobs output is not valid UTF-8")?;

        // nix-eval-jobs outputs one JSON object per line
        // Parse the first line (top-level package)
        let first_line = json_output
            .lines()
            .next()
            .context("nix-eval-jobs produced no output")?;

        let result: Self = serde_json::from_str(first_line)
            .context("Failed to parse nix-eval-jobs JSON output")?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_nix_config() {
        let config = NixConfig::parse().await.unwrap();
        assert!(!config.system.is_empty());
        println!("System: {}", config.system);
        println!("Extra platforms: {:?}", config.extra_platforms);
        println!("Features: {:?}", config.system_features);
    }

    #[test]
    fn test_nix_config_all_platforms() {
        let config = NixConfig {
            system: "x86_64-linux".to_string(),
            extra_platforms: vec!["i686-linux".to_string()],
            system_features: vec!["kvm".to_string()],
        };

        let platforms = config.all_platforms();
        assert_eq!(platforms, vec!["x86_64-linux", "i686-linux"]);
    }
}
