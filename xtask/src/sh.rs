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

/// Like [`run`] but returns captured stdout. Stderr still tails into
/// the spinner. For commands that print a result on stdout while
/// logging progress on stderr (e.g. `nix build --print-out-paths -L`).
pub async fn run_read(cmd: xshell::Cmd<'_>) -> Result<String> {
    let argv = cmd.to_string();
    debug!("exec (run+read): {argv}");

    let mut std_cmd: std::process::Command = cmd.quiet().into();
    std_cmd.stdin(Stdio::null());
    let mut child = tokio::process::Command::from(std_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn: {argv}"))?;

    let (out_buf, err_buf) = tokio::join!(
        // Don't tail stdout — it's the return value, not progress.
        async {
            use tokio::io::AsyncReadExt;
            let mut s = String::new();
            let _ = child.stdout.take().unwrap().read_to_string(&mut s).await;
            s
        },
        tail(child.stderr.take().unwrap(), &argv),
    );
    let status = child.wait().await?;

    if !status.success() {
        for line in err_buf.lines() {
            tracing_indicatif::indicatif_eprintln!("  {} {line}", style("│").dim());
        }
        bail!("{argv}: {status}");
    }
    Ok(out_buf.trim_end().to_string())
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

/// One-time process env setup. Call once from main() before the
/// tokio runtime starts (set_var/remove_var are unsafe with threads).
///
/// - Strips inherited `CARGO_*` vars: when cargo runs the xtask
///   binary it sets CARGO_MANIFEST_DIR, CARGO_PKG_*, etc. If we shell
///   out to a nested `cargo run`, those leak into the child build's
///   fingerprint — ring's build.rs tracks CARGO_MANIFEST_DIR via
///   rerun-if-env-changed, so the next top-level `cargo build`
///   triggers a full rebuild from ring up.
///
/// - Points `KUBECONFIG` at a repo-local `.kube/config`: keeps
///   `cargo xtask k8s kubeconfig` from polluting the user's own
///   kubeconfig (whether `~/.kube/config` or a custom KUBECONFIG).
///   kube-rs, helm, and kubectl all honor KUBECONFIG, so setting it
///   once here covers every child process. Unconditional — the user's
///   ambient KUBECONFIG is for their own clusters, not xtask's.
///
/// # Safety
/// Must be called before any threads are spawned.
pub unsafe fn init_env() {
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str()
            && k.starts_with("CARGO_")
            && k != "CARGO_HOME"
        {
            unsafe { std::env::remove_var(k) };
        }
    }
    unsafe { std::env::set_var("KUBECONFIG", kubeconfig_path()) };
}

/// Repo-local kubeconfig. `k8s kubeconfig` writes here; kube-rs, helm,
/// and kubectl read from here via the KUBECONFIG env var `init_env` sets.
pub fn kubeconfig_path() -> PathBuf {
    repo_root().join(".kube/config")
}
