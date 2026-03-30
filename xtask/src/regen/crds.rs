//! Regenerate `infra/helm/crds/*.yaml` from the crdgen binary.

use anyhow::Result;

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

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &yaml)?;

    // Same python the drift check runs. serde_yml re-serialize (previous
    // approach) produced `description: |-` where PyYAML emits quoted
    // scalars; drift-check diff -r saw every description field as changed.
    let tmp_s = tmp.path().to_str().unwrap();
    let out_s = out.to_str().unwrap();
    sh::run(cmd!(sh, "python3 scripts/split-crds.py {tmp_s} {out_s}")).await?;

    let n = std::fs::read_dir(&out)?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "yaml"))
        .count();
    ui::set_message(&format!("wrote {n} CRDs"));
    Ok(())
}
