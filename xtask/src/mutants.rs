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
    // Mutant compiles are write-only churn for the shared kache store:
    // every viable mutant is unique source bytes (mutated crate + all
    // dependents + relinked test binaries under cache_executables), and
    // .config/mutants.toml's cap_lints makes cargo-mutants inject
    // --cap-lints=warn via CARGO_ENCODED_RUSTFLAGS — a hashed kache key
    // input — so even the unmutated baseline lives in a keyspace no
    // normal build reads. Nothing here is ever a useful hit; don't let
    // it evict artifacts that are.
    //
    // No read-only-restore pre-clean is needed here (unlike regen
    // sqlx): the cap-lints flag set changes cargo's -Cmetadata, so the
    // baseline writes NEW filenames rather than overwriting kache's
    // restored ones, and same-name rebuilds have cargo unlink stale
    // outputs first — verified empirically on both shell toolchains.
    let _env = sh.push_env("KACHE_DISABLED", "1");
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
