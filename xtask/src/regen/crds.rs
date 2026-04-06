//! Regenerate `infra/helm/crds/*.yaml` from the crdgen binary.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

/// Inner `ui::step` count.
pub const STEPS: u64 = 1;

pub async fn run() -> Result<()> {
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
    let yaml = ui::step("cargo run --bin crdgen", || async {
        sh::read(cmd!(sh, "cargo run --bin crdgen"))
    })
    .await?;

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
        ui::set_message(&format!("wrote {name}.yaml"));
        n += 1;
    }
    ui::set_message(&format!("wrote {n} CRDs"));
    Ok(())
}
