//! Regenerate the workspace-hack crate.
//!
//! The workspace-hack crate declares the union of all feature flags any
//! workspace member enables, so `cargo build -p foo` sees the same
//! feature set as `cargo build --workspace` and doesn't recompile
//! shared deps with a narrower resolution.

use anyhow::Result;

use crate::sh::{cmd, shell};

pub fn run() -> Result<()> {
    let sh = shell()?;
    cmd!(sh, "cargo hakari generate").run()?;
    cmd!(sh, "cargo hakari manage-deps --yes").run()?;
    Ok(())
}
