//! Run a fuzz target without remembering which crate's fuzz/ dir it lives in.
//!
//! Targets are auto-discovered: `[workspace] exclude` in the root
//! Cargo.toml lists `*/fuzz` dirs, each with `[[bin]]` entries naming
//! targets. Adding a new target means adding a `[[bin]]` (required
//! anyway) — no xtask edit.

use std::fmt;

use anyhow::Result;
use clap::Args;
use serde::Deserialize;

use crate::sh::{cmd, repo_root, shell};
use crate::ui;

#[derive(Deserialize)]
struct WorkspaceManifest {
    workspace: Workspace,
}
#[derive(Deserialize)]
struct Workspace {
    exclude: Vec<String>,
}
#[derive(Deserialize)]
struct FuzzManifest {
    #[serde(default)]
    bin: Vec<Bin>,
}
#[derive(Deserialize)]
struct Bin {
    name: String,
}

/// `*/fuzz` entries from `[workspace] exclude`. Shared with
/// `regen fuzz-lock`.
pub fn discover_dirs() -> Result<Vec<String>> {
    let root: WorkspaceManifest =
        toml::from_str(&std::fs::read_to_string(repo_root().join("Cargo.toml"))?)?;
    Ok(root
        .workspace
        .exclude
        .into_iter()
        .filter(|d| d.ends_with("/fuzz"))
        .collect())
}

fn discover_targets() -> Result<Vec<Target>> {
    let mut out = Vec::new();
    for dir in discover_dirs()? {
        let fuzz: FuzzManifest = toml::from_str(&std::fs::read_to_string(
            repo_root().join(&dir).join("Cargo.toml"),
        )?)?;
        for b in fuzz.bin {
            out.push(Target {
                name: b.name,
                dir: dir.clone(),
            });
        }
    }
    Ok(out)
}

#[derive(Args)]
pub struct FuzzArgs {
    /// Target to run (see `--list`).
    target: Option<String>,
    /// Print the target → crate table.
    #[arg(long)]
    list: bool,
    /// Extra args passed through to `cargo fuzz run`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    extra: Vec<String>,
}

#[derive(Clone)]
struct Target {
    name: String,
    dir: String,
}
impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<28} ({})", self.name, self.dir)
    }
}

pub fn run(args: FuzzArgs) -> Result<()> {
    let targets = discover_targets()?;

    if args.list {
        // No progress bars active for --list; raw stdout is fine.
        #[allow(clippy::print_stdout)]
        {
            println!("{:<28} crate", "target");
            println!("{:-<28} -----", "");
            for t in &targets {
                println!("{:<28} {}", t.name, t.dir);
            }
        }
        return Ok(());
    }

    let picked = match args.target {
        Some(name) => targets
            .into_iter()
            .find(|t| t.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown fuzz target '{name}' — see --list"))?,
        None => ui::select("Fuzz target?", targets)?
            .ok_or_else(|| anyhow::anyhow!("specify a target: cargo xtask fuzz <TARGET>"))?,
    };

    let sh = shell()?;
    sh.change_dir(repo_root().join(&picked.dir));
    let (name, extra) = (&picked.name, &args.extra);
    crate::sh::run_interactive(cmd!(sh, "cargo fuzz run {name} {extra...}"))
}
