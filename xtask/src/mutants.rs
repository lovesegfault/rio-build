//! Run cargo-mutants with the scoped config and print a summary.
//!
//! `--in-place` mutates the working tree — commit or stash first.
//! cargo-mutants restores source after each mutation, but a ^C
//! mid-run can leave a mutated file behind.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

use crate::sh::{cmd, repo_root, shell};

#[derive(Deserialize)]
struct Outcomes {
    outcomes: Vec<Outcome>,
}

#[derive(Deserialize)]
struct Outcome {
    summary: String,
}

pub fn run() -> Result<()> {
    let sh = shell()?;
    crate::sh::run_interactive(cmd!(
        sh,
        "cargo mutants --in-place --no-shuffle --config .config/mutants.toml"
    ))?;

    let path = repo_root().join("mutants.out/outcomes.json");
    let json: Outcomes = serde_json::from_reader(
        std::fs::File::open(&path).context("mutants.out/outcomes.json not written")?,
    )?;

    let caught = json
        .outcomes
        .iter()
        .filter(|o| o.summary == "CaughtMutant")
        .count();
    let missed = json
        .outcomes
        .iter()
        .filter(|o| o.summary == "MissedMutant")
        .count();

    info!("Caught: {caught}");
    info!("Missed: {missed}");
    info!("See mutants.out/missed.txt for details");
    Ok(())
}
