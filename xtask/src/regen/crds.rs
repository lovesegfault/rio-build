//! Regenerate `infra/helm/crds/*.yaml` from the crdgen binary.

use anyhow::Result;

use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

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

    // crdgen writes one `<crd-name>.yaml` per CRD directly into the
    // output dir. The crds-drift check builds the same binary
    // hermetically and `diff -r`s — single serialization path, no
    // split/reserialize step (the old PyYAML splitter existed to give
    // both callers identical bytes; running the same Rust code does
    // that by construction).
    //
    // --bin (not -p) so feature resolution stays workspace-wide and we
    // reuse the already-built rio-controller artifacts.
    let out_s = out.to_str().unwrap();
    ui::step("cargo run --bin crdgen", || async {
        sh::run(cmd!(sh, "cargo run --bin crdgen -- {out_s}")).await
    })
    .await?;

    let n = std::fs::read_dir(&out)?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "yaml"))
        .count();
    tracing::debug!("wrote {n} CRDs");
    Ok(())
}
