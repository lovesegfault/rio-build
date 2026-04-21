//! `qa --lint`: local config-shape asserts. No cluster required.
//!
//! Seeds with the I-150/151/… class — helm-values regressions that
//! `helm-lint` (CI) catches at template time but where the failure mode
//! was historically "deploy succeeds, pods can't schedule". This stage
//! renders each `values/*.yaml` and asserts the output is non-empty;
//! finer-grained shape checks (resource sizing fits k3s, etc.) follow.

use anyhow::{Context, Result};
use tracing::info;

use crate::sh::{self, cmd, repo_root};

pub fn run() -> Result<()> {
    let chart = repo_root().join("infra/helm/rio-build");
    let sh = sh::shell()?;
    let values: Vec<_> = std::fs::read_dir(chart.join("values"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();

    for vf in &values {
        let chart = chart.display().to_string();
        let v = vf.display().to_string();
        let out = sh::read(cmd!(sh, "helm template rio {chart} -f {v}"))
            .with_context(|| format!("helm template -f {v}"))?;
        anyhow::ensure!(
            !out.trim().is_empty(),
            "helm template -f {v} produced no output"
        );
    }
    info!("lint: {} values files render", values.len());
    Ok(())
}
