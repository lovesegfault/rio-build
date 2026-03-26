//! Run a fuzz target without remembering which crate's fuzz/ dir it lives in.

use std::fmt;

use anyhow::Result;
use clap::Args;

use crate::sh::{cmd, repo_root, shell};
use crate::ui;

const TARGETS: &[(&str, &str)] = &[
    ("wire_primitives", "rio-nix/fuzz"),
    ("nar_parsing", "rio-nix/fuzz"),
    ("opcode_parsing", "rio-nix/fuzz"),
    ("derivation_parsing", "rio-nix/fuzz"),
    ("derived_path_parsing", "rio-nix/fuzz"),
    ("narinfo_parsing", "rio-nix/fuzz"),
    ("build_result_parsing", "rio-nix/fuzz"),
    ("stderr_message_parsing", "rio-nix/fuzz"),
    ("refscan", "rio-nix/fuzz"),
    ("manifest_deserialize", "rio-store/fuzz"),
];

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

struct Target(&'static str, &'static str);
impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<28} ({})", self.0, self.1)
    }
}

pub fn run(args: FuzzArgs) -> Result<()> {
    if args.list {
        println!("{:<28} crate", "target");
        println!("{:-<28} -----", "");
        for (t, d) in TARGETS {
            println!("{t:<28} {d}");
        }
        return Ok(());
    }

    let (target, dir) = match args.target {
        Some(t) => {
            let (_, dir) = TARGETS
                .iter()
                .find(|(n, _)| *n == t)
                .ok_or_else(|| anyhow::anyhow!("unknown fuzz target '{t}' — see --list"))?;
            (t, *dir)
        }
        None => {
            let opts: Vec<_> = TARGETS.iter().map(|&(n, d)| Target(n, d)).collect();
            let picked = ui::select("Fuzz target?", opts)?
                .ok_or_else(|| anyhow::anyhow!("specify a target: cargo xtask fuzz <TARGET>"))?;
            (picked.0.to_string(), picked.1)
        }
    };

    let sh = shell()?;
    sh.change_dir(repo_root().join(dir));
    let extra = &args.extra;
    cmd!(sh, "cargo fuzz run {target} {extra...}").run()?;
    Ok(())
}
