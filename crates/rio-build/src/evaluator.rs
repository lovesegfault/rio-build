//! Build evaluator using nix-eval-jobs

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rio_common::nix_utils::EvalResult;

/// Information about a build to submit
pub struct BuildInfo {
    /// Path to the top-level derivation
    pub drv_path: Utf8PathBuf,

    /// Derivation bytes (raw .drv file contents)
    pub drv_bytes: Vec<u8>,

    /// Required system platform
    pub platform: String,

    /// Required system features
    pub required_features: Vec<String>,

    /// Paths of derivations that need building (dependencies)
    pub dependency_paths: Vec<Utf8PathBuf>,
}

/// Evaluate a Nix file and prepare build information
///
/// This runs nix-eval-jobs to:
/// - Get the derivation path
/// - Check cache status
/// - Identify dependencies that need building
/// - Extract platform and feature requirements
pub async fn evaluate_build(nix_file: &Utf8Path) -> Result<BuildInfo> {
    // Run nix-eval-jobs to evaluate the expression
    let eval_result = EvalResult::from_file(nix_file)
        .await
        .context("Failed to evaluate Nix file")?;

    // Check if already built or cached
    if eval_result.cache_status == "cached" || eval_result.cache_status == "local" {
        anyhow::bail!(
            "Package is already available (cache status: {}). No remote build needed.",
            eval_result.cache_status
        );
    }

    // Read the derivation file bytes
    let drv_bytes = tokio::fs::read(&eval_result.drv_path)
        .await
        .with_context(|| format!("Failed to read derivation file: {}", eval_result.drv_path))?;

    Ok(BuildInfo {
        drv_path: eval_result.drv_path,
        drv_bytes,
        platform: eval_result.system,
        required_features: eval_result.required_system_features,
        dependency_paths: eval_result.needed_builds,
    })
}
