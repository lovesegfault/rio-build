//! Shell-out helpers. Verbosity-aware command execution with
//! last-line tailing into the current span's message.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use console::style;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::debug;
use xshell::Shell;

pub use xshell::cmd;

use crate::ui;

static REPO_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Absolute path to the workspace root (the dir containing Cargo.toml
/// with [workspace]). Computed from CARGO_MANIFEST_DIR at build time.
pub fn repo_root() -> &'static Path {
    REPO_ROOT.get_or_init(|| {
        // xtask/Cargo.toml → parent = repo root
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent dir")
            .to_path_buf()
    })
}

/// Shell rooted at the repo root.
pub fn shell() -> Result<Shell> {
    let sh = Shell::new().context("failed to create xshell")?;
    sh.change_dir(repo_root());
    Ok(sh)
}

/// Run a command with verbosity-aware output handling.
///
/// - default/`-q`: capture both streams; tail the last line into the
///   current span's message; on failure, dump captured output with a
///   `  │ ` prefix.
/// - `-v`+: inherit stdio; progress bars are suspended while the child
///   runs so output prints cleanly.
pub async fn run(cmd: xshell::Cmd<'_>) -> Result<()> {
    let argv = cmd.to_string();
    debug!("exec: {argv}");

    if ui::is_verbose() {
        return ui::suspend(|| cmd.quiet().run().map_err(anyhow::Error::from));
    }

    // Captured mode. xshell's Cmd doesn't expose spawn-with-pipes, so
    // convert to std::process::Command (xshell supports From).
    let mut std_cmd: std::process::Command = cmd.quiet().into();
    std_cmd.stdin(Stdio::null());
    let mut child = tokio::process::Command::from(std_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn: {argv}"))?;

    let (out_buf, err_buf) = tokio::join!(
        tail(child.stdout.take().unwrap(), &argv),
        tail(child.stderr.take().unwrap(), &argv),
    );
    let status = child.wait().await?;

    if !status.success() {
        // Dump captured output so the user can see what failed.
        for line in out_buf.lines().chain(err_buf.lines()) {
            tracing_indicatif::indicatif_eprintln!("  {} {line}", style("│").dim());
        }
        bail!("{argv}: {status}");
    }
    Ok(())
}

/// Run a command that must interact with a tty (prompts for input).
/// Always inherits stdio, regardless of verbosity.
pub fn run_interactive(cmd: xshell::Cmd<'_>) -> Result<()> {
    let argv = cmd.to_string();
    debug!("exec (interactive): {argv}");
    ui::suspend(|| cmd.quiet().run().map_err(anyhow::Error::from))
}

/// Blocking variant of [`run`] for sync contexts. Same verbosity
/// handling, but uses std::process instead of tokio (no last-line
/// tail — just captured-then-dump-on-failure).
pub fn run_sync(cmd: xshell::Cmd<'_>) -> Result<()> {
    let argv = cmd.to_string();
    debug!("exec: {argv}");

    if ui::is_verbose() {
        return ui::suspend(|| cmd.quiet().run().map_err(anyhow::Error::from));
    }

    let out = cmd.quiet().ignore_status().output()?;
    if !out.status.success() {
        for line in std::str::from_utf8(&out.stdout)
            .unwrap_or("")
            .lines()
            .chain(std::str::from_utf8(&out.stderr).unwrap_or("").lines())
        {
            tracing_indicatif::indicatif_eprintln!("  {} {line}", style("│").dim());
        }
        bail!("{argv}: {}", out.status);
    }
    Ok(())
}

/// Capture stdout as a String. At default verbosity, stderr is
/// suppressed; at -v+ it streams through (so cargo build progress
/// shows). The output IS the return value — always captured.
pub fn read(cmd: xshell::Cmd<'_>) -> Result<String> {
    debug!("exec (read): {}", cmd);
    if ui::is_verbose() {
        cmd.quiet().read().map_err(anyhow::Error::from)
    } else {
        // xshell's Cmd doesn't support stderr redirect directly;
        // go via std::process::Command.
        let mut std_cmd: std::process::Command = cmd.quiet().into();
        let out = std_cmd.stderr(Stdio::piped()).output()?;
        if !out.status.success() {
            for line in std::str::from_utf8(&out.stderr).unwrap_or("").lines() {
                tracing_indicatif::indicatif_eprintln!("  {} {line}", style("│").dim());
            }
            bail!("command failed: {}", out.status);
        }
        Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
    }
}

/// Line-read a child stream, updating the span message with each line
/// and returning the full captured buffer.
async fn tail<R: tokio::io::AsyncRead + Unpin>(r: R, prefix: &str) -> String {
    let mut lines = BufReader::new(r).lines();
    let mut buf = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        ui::set_message(&format!("{prefix}: {line}"));
        buf.push_str(&line);
        buf.push('\n');
    }
    buf
}

/// Strip inherited `CARGO_*` env vars. Call once from main() before
/// the tokio runtime starts (remove_var is unsafe with threads).
///
/// When cargo runs the xtask binary it sets CARGO_MANIFEST_DIR,
/// CARGO_PKG_*, etc. If we shell out to a nested `cargo run`, those
/// leak into the child build's fingerprint — ring's build.rs tracks
/// CARGO_MANIFEST_DIR via rerun-if-env-changed, so the next top-level
/// `cargo build` (where the var is absent) triggers a full rebuild
/// from ring up. Stripping here makes nested cargo start clean.
///
/// # Safety
/// Must be called before any threads are spawned.
pub unsafe fn scrub_cargo_env() {
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str()
            && k.starts_with("CARGO_")
            && k != "CARGO_HOME"
        {
            unsafe { std::env::remove_var(k) };
        }
    }
}
