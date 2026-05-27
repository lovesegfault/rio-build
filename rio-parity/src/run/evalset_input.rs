//! Readers for the eval-set artifacts the campaign consumes:
//! `evalset.json`, `manifest.jsonl`, `dep-closure.jsonl`.
//!
//! The on-disk formats are the contract with the eval-set builder
//! (`crate::evalset`). The readers deserialize only the fields the
//! engine needs and ignore everything else, so the builder can keep
//! adding fields without breaking older campaign engines.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// evalset.json — only the fields the campaign needs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvalSetMeta {
    pub hydra_eval_id: u64,
    pub key_digest: String,
    pub systems: Vec<String>,
    /// SHA-256 (hex) of manifest.jsonl. Optional: the current eval-set
    /// builder does not record it, so when absent the engine computes the
    /// digest at plan time and records it in campaign.json (the resume /
    /// report mismatch gate applies either way).
    pub manifest_sha256: Option<String>,
}

/// One line of manifest.jsonl: one record per in-scope job, mirroring the
/// camelCase field names nix-eval-jobs emits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub job: String,
    pub system: String,
    #[serde(default)]
    pub attr: String,
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    /// output name → store path
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    #[serde(rename = "requiredFeatures", default)]
    pub required_features: Vec<String>,
}

/// One line of dep-closure.jsonl: per-target proper dependency closure in
/// adjacency form (each dep drv with its declared outputs). Extra keys
/// (`caOutputs`, …) are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepClosureEntry {
    pub job: String,
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    #[serde(default)]
    pub deps: Vec<DepDrvOutputs>,
    /// Legacy flat-format key. Never written by the current eval-set
    /// builder; captured only so the plan stage can detect an eval set
    /// produced with a pre-adjacency dep-closure format and fail loudly
    /// instead of silently computing an empty warm set (see
    /// `plan::run_plan`).
    #[serde(rename = "depOutputPaths", default, skip_serializing)]
    pub legacy_dep_output_paths: Vec<String>,
}

/// One dependency derivation and the output paths it declares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepDrvOutputs {
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    #[serde(rename = "outputPaths", default)]
    pub output_paths: Vec<String>,
}

/// Read and parse `<dir>/evalset.json`.
pub fn load_meta(dir: &Path) -> Result<EvalSetMeta> {
    let path = dir.join("evalset.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn load_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line.with_context(|| format!("read {} line {}", path.display(), i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(&line)
                .with_context(|| format!("{} line {}", path.display(), i + 1))?,
        );
    }
    Ok(out)
}

/// Read every record of `<dir>/manifest.jsonl`.
pub fn load_manifest(dir: &Path) -> Result<Vec<ManifestEntry>> {
    load_jsonl(&dir.join("manifest.jsonl"))
}

/// Read every record of `<dir>/dep-closure.jsonl`; an absent file is an
/// empty list. Self-hosted campaigns can run without it (no warm stage),
/// but leaf mode requires it — the plan stage enforces that.
///
/// Memory posture: every record is materialized as owned Strings, sized for
/// the scoped (constituents / explicit job-list) eval sets the engine runs
/// today. A full-evaluation campaign (hundreds of thousands of jobs) needs
/// an interning or streaming pass here before it is attempted.
pub fn load_dep_closure(dir: &Path) -> Result<Vec<DepClosureEntry>> {
    let path = dir.join("dep-closure.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    load_jsonl(&path)
}

/// SHA-256 (hex) of manifest.jsonl bytes — the digest recorded in
/// campaign.json and re-checked on resume so one campaign can never mix
/// two different eval sets.
pub fn manifest_sha256(dir: &Path) -> Result<String> {
    let path = dir.join("manifest.jsonl");
    let mut f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    use super::*;
    use std::io::Write;

    /// Deterministic, name-distinct, nixbase32-valid 32-char hash part so
    /// every synthetic store path has a unique hash part (narinfo lookups key
    /// on it).
    pub fn fake_hash(seed: &str) -> String {
        const CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let bytes = seed.as_bytes();
        (0..32)
            .map(|i| CHARS[(bytes[i % bytes.len()] as usize + i) % CHARS.len()] as char)
            .collect()
    }

    /// Write a tiny synthetic eval set into `dir`: three x86_64-linux jobs and
    /// one aarch64-linux job. `libA` is a dependency of `appB`; `kvmTest`
    /// requires the kvm feature.
    pub fn write_mini_eval_set(dir: &Path) {
        let drv = |name: &str| {
            format!(
                "/nix/store/{}-{name}.drv",
                fake_hash(&format!("{name}-drv"))
            )
        };
        let out = |name: &str| format!("/nix/store/{}-{name}", fake_hash(&format!("{name}-out")));
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("evalset.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "hydra_eval_id": 1824219u64,
                "key_digest": "deadbeef",
                "systems": ["x86_64-linux", "aarch64-linux"],
            }))
            .unwrap(),
        )
        .unwrap();
        let manifest = [
            serde_json::json!({"job": "libA.x86_64-linux", "system": "x86_64-linux", "attr": "libA",
                "drvPath": drv("libA"), "outputs": {"out": out("libA")}, "requiredFeatures": []}),
            serde_json::json!({"job": "appB.x86_64-linux", "system": "x86_64-linux", "attr": "appB",
                "drvPath": drv("appB"), "outputs": {"out": out("appB"), "dev": out("appB-dev")}, "requiredFeatures": []}),
            serde_json::json!({"job": "kvmTest.x86_64-linux", "system": "x86_64-linux", "attr": "kvmTest",
                "drvPath": drv("kvmTest"), "outputs": {"out": out("kvmTest")}, "requiredFeatures": ["kvm"]}),
            serde_json::json!({"job": "libA.aarch64-linux", "system": "aarch64-linux", "attr": "libA",
                "drvPath": drv("libA-arm"), "outputs": {"out": out("libA-arm")}, "requiredFeatures": []}),
        ];
        let mut m = std::fs::File::create(dir.join("manifest.jsonl")).unwrap();
        for j in &manifest {
            writeln!(m, "{j}").unwrap();
        }
        let depc = [
            serde_json::json!({"job": "libA.x86_64-linux", "drvPath": drv("libA"), "deps": []}),
            serde_json::json!({"job": "appB.x86_64-linux", "drvPath": drv("appB"),
                "deps": [{"drvPath": drv("libA"), "outputPaths": [out("libA")]},
                          {"drvPath": drv("stdenv"), "outputPaths": [out("stdenv")]}]}),
            serde_json::json!({"job": "kvmTest.x86_64-linux", "drvPath": drv("kvmTest"), "deps": []}),
            serde_json::json!({"job": "libA.aarch64-linux", "drvPath": drv("libA-arm"), "deps": []}),
        ];
        let mut d = std::fs::File::create(dir.join("dep-closure.jsonl")).unwrap();
        for j in &depc {
            writeln!(d, "{j}").unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_mini_eval_set() {
        let dir = tempfile::tempdir().unwrap();
        test_fixtures::write_mini_eval_set(dir.path());
        let meta = load_meta(dir.path()).unwrap();
        assert_eq!(meta.hydra_eval_id, 1824219);
        assert_eq!(meta.key_digest, "deadbeef");
        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.len(), 4);
        assert_eq!(manifest[1].outputs.len(), 2);
        let depc = load_dep_closure(dir.path()).unwrap();
        assert_eq!(depc.len(), 4);
        assert_eq!(depc[1].deps.len(), 2);
        let digest = manifest_sha256(dir.path()).unwrap();
        assert_eq!(digest.len(), 64);
        // Deterministic.
        assert_eq!(digest, manifest_sha256(dir.path()).unwrap());
    }
}
