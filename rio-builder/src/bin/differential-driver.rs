//! Differential-harness driver binary.
//!
//! Runs ONE derivation through the native executor stack (request glue →
//! rio-exec sandbox → result pipeline) and prints a JSON report on
//! stdout. Only meaningful inside the `vm-differential-standalone`
//! NixOS test: it needs root, namespace capabilities, and a populated
//! /nix/store with the derivation's input closure. See
//! `rio_builder::executor::differential` for what it does and which
//! production simplifications it makes.
//!
//! Usage:
//!
//! ```text
//! differential-driver --drv <path.drv> --work-dir <dir> \
//!   [--sandbox-shell <path>] [--system <sys>] [--timeout-secs <n>]
//! ```

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use rio_builder::executor::differential::{DriverConfig, run};

#[derive(Parser, Debug)]
#[command(name = "differential-driver", disable_version_flag = true)]
struct Args {
    /// Path to the .drv file to build (input closure must be valid in
    /// the local /nix/store).
    #[arg(long)]
    drv: PathBuf,
    /// Scratch directory for this build (store copy, /build, chroot).
    #[arg(long)]
    work_dir: PathBuf,
    /// Static shell to provide as /bin/sh inside the sandbox.
    #[arg(long)]
    sandbox_shell: Option<PathBuf>,
    /// Host system string.
    #[arg(long, default_value = "x86_64-linux")]
    system: String,
    /// Sandbox uid/gid.
    #[arg(long, default_value_t = 1000)]
    uid: u32,
    #[arg(long, default_value_t = 100)]
    gid: u32,
    /// Wall-clock timeout for the sandboxed build, in seconds.
    #[arg(long, default_value_t = 600)]
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let report = run(DriverConfig {
        drv_path: args.drv,
        work_dir: args.work_dir,
        sandbox_shell: args.sandbox_shell,
        host_system: args.system,
        uid: args.uid,
        gid: args.gid,
        timeout: Duration::from_secs(args.timeout_secs),
    })
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
