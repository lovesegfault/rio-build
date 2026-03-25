//! Run a fuzz target without remembering which crate's fuzz/ dir it lives in.

use anyhow::{Result, bail};
use clap::Args;

use crate::sh::{cmd, repo_root, shell};

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

pub fn run(args: FuzzArgs) -> Result<()> {
    if args.list || args.target.is_none() {
        println!("{:<28} crate", "target");
        println!("{:-<28} -----", "");
        for (t, d) in TARGETS {
            println!("{t:<28} {d}");
        }
        if args.target.is_none() && !args.list {
            bail!("specify a target (or --list)");
        }
        return Ok(());
    }

    let target = args.target.unwrap();
    let Some((_, dir)) = TARGETS.iter().find(|(t, _)| *t == target) else {
        eprintln!("unknown target '{target}'. available:");
        for (t, _) in TARGETS {
            eprintln!("  {t}");
        }
        bail!("unknown fuzz target");
    };

    let sh = shell()?;
    sh.change_dir(repo_root().join(dir));
    let extra = &args.extra;
    cmd!(sh, "cargo fuzz run {target} {extra...}").run()?;
    Ok(())
}
