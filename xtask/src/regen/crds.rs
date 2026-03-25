//! Regenerate `infra/helm/crds/*.yaml` from the crdgen binary.
//!
//! Replaces `scripts/split-crds.sh`'s inline Python with serde_yml:
//! run crdgen, split the multi-doc output into one file per CRD.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

use crate::sh::{cmd, repo_root, shell};

pub fn run() -> Result<()> {
    let sh = shell()?;
    let out = repo_root().join("infra/helm/crds");

    // Clear existing YAMLs so removed CRDs don't linger.
    for e in std::fs::read_dir(&out)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "yaml") {
            std::fs::remove_file(p)?;
        }
    }

    // --bin (not -p) so feature resolution stays workspace-wide and we
    // reuse the already-built rio-controller artifacts.
    let yaml = cmd!(sh, "cargo run --bin crdgen").read()?;

    let mut n = 0;
    for doc in serde_yml::Deserializer::from_str(&yaml) {
        let v = serde_yml::Value::deserialize(doc)?;
        if v.is_null() {
            continue;
        }
        let name = v["metadata"]["name"]
            .as_str()
            .context("CRD missing metadata.name")?;
        let path = out.join(format!("{name}.yaml"));
        std::fs::write(&path, serde_yml::to_string(&v)?)?;
        info!("  {name}.yaml");
        n += 1;
    }
    info!("wrote {n} CRDs to {}", out.display());
    Ok(())
}
