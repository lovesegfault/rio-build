// Derivation parsing utilities

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;

/// Information extracted from a Nix derivation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivationInfo {
    /// The system/platform this derivation targets (e.g., "x86_64-linux")
    pub system: String,
    /// Output paths mapped by output name
    pub outputs: HashMap<String, DerivationOutput>,
    /// Input derivations
    pub input_derivations: HashMap<String, Vec<String>>,
    /// Input source paths
    pub input_sources: Vec<String>,
    /// The builder executable
    pub builder: String,
    /// Arguments to the builder
    pub args: Vec<String>,
    /// Derivation name
    pub name: String,
}

/// Output information for a derivation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivationOutput {
    /// Store path of this output
    pub path: String,
}

/// Parsed JSON from `nix derivation show`
#[derive(Debug, Deserialize)]
struct NixDerivationShow {
    #[serde(flatten)]
    derivations: HashMap<String, NixDerivation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NixDerivation {
    system: String,
    outputs: HashMap<String, NixOutput>,
    #[serde(default)]
    input_drvs: HashMap<String, Vec<String>>,
    #[serde(default)]
    input_srcs: Vec<String>,
    builder: String,
    args: Vec<String>,
    name: String,
}

#[derive(Debug, Deserialize)]
struct NixOutput {
    path: String,
}

impl DerivationInfo {
    /// Parse a derivation file using `nix derivation show`
    pub async fn from_file(drv_path: &str) -> Result<Self> {
        let output = Command::new("nix")
            .args(["derivation", "show", drv_path])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("Failed to execute nix derivation show")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "nix derivation show failed with status {}: {}",
                output.status,
                stderr
            );
        }

        let json_str = String::from_utf8(output.stdout)
            .context("nix derivation show output was not valid UTF-8")?;

        Self::from_json(&json_str)
    }

    /// Parse derivation from JSON output of `nix derivation show`
    pub fn from_json(json: &str) -> Result<Self> {
        let parsed: NixDerivationShow =
            serde_json::from_str(json).context("Failed to parse nix derivation show JSON")?;

        // Extract the first (and should be only) derivation
        let (_drv_path, nix_drv) = parsed
            .derivations
            .into_iter()
            .next()
            .context("No derivations found in JSON")?;

        Ok(Self {
            system: nix_drv.system,
            outputs: nix_drv
                .outputs
                .into_iter()
                .map(|(name, output)| (name, DerivationOutput { path: output.path }))
                .collect(),
            input_derivations: nix_drv.input_drvs,
            input_sources: nix_drv.input_srcs,
            builder: nix_drv.builder,
            args: nix_drv.args,
            name: nix_drv.name,
        })
    }

    /// Get the primary output path (usually "out")
    pub fn primary_output(&self) -> Option<&str> {
        self.outputs
            .get("out")
            .map(|o| o.path.as_str())
            .or_else(|| self.outputs.values().next().map(|o| o.path.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_derivation_json_basic() {
        let json = r#"{
            "/nix/store/test.drv": {
                "args": ["-c", "echo test > $out"],
                "builder": "/bin/sh",
                "env": {
                    "builder": "/bin/sh",
                    "name": "test",
                    "out": "/nix/store/output",
                    "system": "x86_64-linux"
                },
                "inputDrvs": {},
                "inputSrcs": [],
                "name": "test",
                "outputs": {
                    "out": {
                        "path": "/nix/store/output"
                    }
                },
                "system": "x86_64-linux"
            }
        }"#;

        let info = DerivationInfo::from_json(json).unwrap();
        assert_eq!(info.system, "x86_64-linux");
        assert_eq!(info.name, "test");
        assert_eq!(info.builder, "/bin/sh");
        assert_eq!(info.args, vec!["-c", "echo test > $out"]);
        assert_eq!(info.primary_output(), Some("/nix/store/output"));
    }

    #[test]
    fn test_parse_derivation_multiple_outputs() {
        let json = r#"{
            "/nix/store/multi.drv": {
                "args": [],
                "builder": "/bin/sh",
                "env": {},
                "inputDrvs": {},
                "inputSrcs": [],
                "name": "multi-output",
                "outputs": {
                    "out": {
                        "path": "/nix/store/output1"
                    },
                    "dev": {
                        "path": "/nix/store/output2-dev"
                    },
                    "doc": {
                        "path": "/nix/store/output3-doc"
                    }
                },
                "system": "aarch64-linux"
            }
        }"#;

        let info = DerivationInfo::from_json(json).unwrap();
        assert_eq!(info.system, "aarch64-linux");
        assert_eq!(info.outputs.len(), 3);
        assert!(info.outputs.contains_key("out"));
        assert!(info.outputs.contains_key("dev"));
        assert!(info.outputs.contains_key("doc"));
        assert_eq!(info.primary_output(), Some("/nix/store/output1"));
    }

    #[test]
    fn test_parse_derivation_with_input_derivations() {
        let json = r#"{
            "/nix/store/with-deps.drv": {
                "args": ["-e", "build script"],
                "builder": "/nix/store/bash",
                "env": {},
                "inputDrvs": {
                    "/nix/store/dep1.drv": ["out"],
                    "/nix/store/dep2.drv": ["out", "dev"]
                },
                "inputSrcs": [
                    "/nix/store/source1",
                    "/nix/store/source2"
                ],
                "name": "package-with-deps",
                "outputs": {
                    "out": {
                        "path": "/nix/store/final-output"
                    }
                },
                "system": "x86_64-darwin"
            }
        }"#;

        let info = DerivationInfo::from_json(json).unwrap();
        assert_eq!(info.system, "x86_64-darwin");
        assert_eq!(info.name, "package-with-deps");
        assert_eq!(info.input_derivations.len(), 2);
        assert_eq!(
            info.input_derivations.get("/nix/store/dep1.drv"),
            Some(&vec!["out".to_string()])
        );
        assert_eq!(info.input_sources.len(), 2);
        assert!(
            info.input_sources
                .contains(&"/nix/store/source1".to_string())
        );
    }

    #[test]
    fn test_parse_derivation_different_platforms() {
        let platforms = vec![
            "x86_64-linux",
            "aarch64-linux",
            "x86_64-darwin",
            "aarch64-darwin",
        ];

        for platform in platforms {
            let json = format!(
                r#"{{
                "/nix/store/test.drv": {{
                    "args": [],
                    "builder": "/bin/sh",
                    "env": {{}},
                    "inputDrvs": {{}},
                    "inputSrcs": [],
                    "name": "test",
                    "outputs": {{
                        "out": {{
                            "path": "/nix/store/output"
                        }}
                    }},
                    "system": "{}"
                }}
            }}"#,
                platform
            );

            let info = DerivationInfo::from_json(&json).unwrap();
            assert_eq!(info.system, platform);
        }
    }

    #[test]
    fn test_primary_output_fallback() {
        // Test with no "out" output - should return first available
        let json = r#"{
            "/nix/store/test.drv": {
                "args": [],
                "builder": "/bin/sh",
                "env": {},
                "inputDrvs": {},
                "inputSrcs": [],
                "name": "test",
                "outputs": {
                    "custom": {
                        "path": "/nix/store/custom-output"
                    }
                },
                "system": "x86_64-linux"
            }
        }"#;

        let info = DerivationInfo::from_json(json).unwrap();
        assert_eq!(info.primary_output(), Some("/nix/store/custom-output"));
    }

    #[test]
    fn test_parse_empty_args() {
        let json = r#"{
            "/nix/store/test.drv": {
                "args": [],
                "builder": "/nix/store/builder",
                "env": {},
                "inputDrvs": {},
                "inputSrcs": [],
                "name": "test",
                "outputs": {
                    "out": {
                        "path": "/nix/store/output"
                    }
                },
                "system": "x86_64-linux"
            }
        }"#;

        let info = DerivationInfo::from_json(json).unwrap();
        assert!(info.args.is_empty());
        assert_eq!(info.builder, "/nix/store/builder");
    }

    #[test]
    fn test_parse_invalid_json() {
        let invalid_json = "not valid json";
        let result = DerivationInfo::from_json(invalid_json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty_derivation_list() {
        let json = "{}";
        let result = DerivationInfo::from_json(json);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No derivations found")
        );
    }

    #[tokio::test]
    async fn test_parse_real_derivation() {
        // Create a simple test derivation
        let test_nix = r#"
derivation {
  name = "rio-parse-test";
  system = builtins.currentSystem;
  builder = "/bin/sh";
  args = [ "-c" "echo test > $out" ];
}
"#;

        let nix_file = "/tmp/rio-parse-test.nix";
        tokio::fs::write(nix_file, test_nix).await.unwrap();

        // Instantiate to get .drv
        let output = Command::new("nix-instantiate")
            .arg(nix_file)
            .output()
            .await
            .unwrap();

        if !output.status.success() {
            eprintln!("nix-instantiate failed, skipping test");
            return;
        }

        let drv_path = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Parse derivation
        let info = DerivationInfo::from_file(&drv_path).await.unwrap();

        assert_eq!(info.name, "rio-parse-test");
        assert_eq!(info.builder, "/bin/sh");
        assert!(!info.system.is_empty());

        // Cleanup
        let _ = tokio::fs::remove_file(nix_file).await;
    }
}
