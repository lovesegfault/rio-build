//! Shell-out helpers. Verbosity-aware command execution with
//! last-line tailing into the current span's message.
//!
//! The only module allowed to call `xshell::Cmd::run/read/output`
//! directly — every other callsite MUST go through these wrappers so
//! output is captured/suspended and doesn't land on the spinner line.
//! Enforced by clippy disallowed-methods.
//!
//! Raw `std/tokio::process::Command` is still permitted where these
//! wrappers can't fit: long-lived children under [`ProcessGuard`]
//! (port-forward, SSM tunnel), detached spawns with `pre_exec`/setsid
//! (stress-test builds), piped stdin (`skopeo login --password-stdin`),
//! and best-effort probes whose error becomes part of the output value
//! rather than a bail. Those sites carry an inline comment naming the
//! constraint; one-shot fire-and-read commands should use [`run`] /
//! [`run_read`] / [`try_read`].
//!
//! [`ProcessGuard`]: crate::k8s::shared::ProcessGuard
#![allow(clippy::disallowed_methods)]

use std::future::Future;
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
/// with `\[workspace\]`). `RIO_REPO_ROOT` env override wins (used by
/// `nix/docs.nix` to point the crate2nix-built binary at a runCommand
/// `$src` tree — the compile-time `CARGO_MANIFEST_DIR` is a store
/// path there). Otherwise computed from CARGO_MANIFEST_DIR at build
/// time.
pub fn repo_root() -> &'static Path {
    REPO_ROOT.get_or_init(|| {
        if let Ok(p) = std::env::var("RIO_REPO_ROOT") {
            return PathBuf::from(p);
        }
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
///
/// Not `async fn` — `Cmd<'_>` borrows the `Shell`, and an `async fn`
/// signature would propagate that lifetime into the returned future,
/// making callers unable to hold `Shell` across `.await` (breaks
/// `tokio::spawn`). Convert to owned `Command` synchronously here so
/// the returned future has no borrow.
pub fn run(cmd: xshell::Cmd<'_>) -> impl Future<Output = Result<()>> + Send + use<> {
    let argv = cmd.to_string();
    let std_cmd: std::process::Command = cmd.quiet().into();
    async move { run_inner(argv, std_cmd, false).await.map(|_| ()) }
}

/// Like [`run`] but returns captured stdout. Stderr still tails into
/// the spinner. For commands that print a result on stdout while
/// logging progress on stderr (e.g. `nix build --print-out-paths -L`).
pub fn run_read(cmd: xshell::Cmd<'_>) -> impl Future<Output = Result<String>> + Send + use<> {
    let argv = cmd.to_string();
    let std_cmd: std::process::Command = cmd.quiet().into();
    run_inner(argv, std_cmd, true)
}

async fn run_inner(
    argv: String,
    mut std_cmd: std::process::Command,
    read_stdout: bool,
) -> Result<String> {
    debug!("exec: {argv}");

    if ui::is_verbose() {
        if read_stdout {
            // run_read in verbose: inherit stderr (live), capture
            // stdout. I-198: std::process::Command::output() blocks
            // the calling thread. run_read is awaited inside spawned
            // phase tasks — blocking here ties up a runtime worker for
            // the child's lifetime. spawn_blocking offloads to the
            // blocking pool and yields, same as the non-verbose
            // tokio::process path below. No bail-text contract here —
            // run_read callers want stdout, not the error.
            let out =
                tokio::task::spawn_blocking(move || std_cmd.stderr(Stdio::inherit()).output())
                    .await??;
            if !out.status.success() {
                bail!("{argv}: {}", out.status);
            }
            return Ok(String::from_utf8(out.stdout)?.trim_end().to_string());
        }
        // run in verbose: inherit stdout (live), TEE stderr (live AND
        // buffered). The bail must carry the stderr tail so
        // text-matching retry classifiers (qa's
        // is_transient_gateway_err) and verdict checks (iso03) work
        // under `-v` — same Error contract b4b7f29c7 established for
        // the non-verbose path. Plain inherit can't capture;
        // piping-without-printing would buffer a long `nix build`'s
        // progress to nowhere; tee gives both.
        std_cmd.stdin(Stdio::null());
        std_cmd.stdout(Stdio::inherit());
        std_cmd.stderr(Stdio::piped());
        let mut child = tokio::process::Command::from(std_cmd)
            .spawn()
            .with_context(|| format!("failed to spawn: {argv}"))?;
        let stderr = child.stderr.take().expect("set via Stdio::piped() above");
        let mut lines = BufReader::new(stderr).lines();
        let mut err_buf = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            ui::eprint(format_args!("{line}\n"));
            err_buf.push_str(&line);
            err_buf.push('\n');
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("{argv}: {status}: {}", fold_stderr_tail(&err_buf));
        }
        return Ok(String::new());
    }

    std_cmd.stdin(Stdio::null());
    let mut child = tokio::process::Command::from(std_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn: {argv}"))?;

    let stdout = child.stdout.take().expect("set via Stdio::piped() above");
    let stderr = child.stderr.take().expect("set via Stdio::piped() above");
    let (out_buf, err_buf) = if read_stdout {
        tokio::join!(
            async {
                use tokio::io::AsyncReadExt;
                let mut s = String::new();
                let _ = BufReader::new(stdout).read_to_string(&mut s).await;
                s
            },
            tail(stderr, &argv),
        )
    } else {
        tokio::join!(tail(stdout, &argv), tail(stderr, &argv))
    };
    let status = child.wait().await?;

    if !status.success() {
        let out_lines = if read_stdout { "" } else { &out_buf };
        for line in out_lines.lines().chain(err_buf.lines()) {
            ui::eprint(format_args!("  {} {line}\n", style("│").dim()));
        }
        bail!("{argv}: {status}: {}", fold_stderr_tail(&err_buf));
    }
    Ok(out_buf.trim_end().to_string())
}

/// Fold the last few stderr lines into a `bail!`-able fragment so
/// callers matching on failure text (qa's `is_transient_gateway_err`,
/// anything else that needs to discriminate transient vs permanent)
/// actually see it. Both the verbose and non-verbose [`run`] paths use
/// this so a `-v` run carries the same Error contract — previously the
/// verbose bail was `{argv}: exit status N` only, making the qa
/// JWT-retry a no-op under `-v`.
fn fold_stderr_tail(err_buf: &str) -> String {
    err_buf
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Run `cmd`, capturing stdout+stderr. `Ok(())` on success OR if
/// `benign(combined_output)` returns true. Otherwise dumps captured
/// output and bails. For best-effort teardown commands where specific
/// failure text means "already done" — e.g. `kubectl delete` →
/// `NotFound`, `helm uninstall` → `cluster unreachable`.
///
/// [`run`]'s error chain now includes the last 5 stderr lines, but only
/// the TAIL — for matching on a specific line that may be earlier, this
/// helper captures the FULL output into the predicate's input. Output is
/// teed (echoed live at `info!` AND accumulated) so long `--wait
/// --timeout=...` commands still show progress.
pub fn run_benign_if(
    cmd: xshell::Cmd<'_>,
    benign: fn(&str) -> bool,
) -> impl Future<Output = Result<()>> + Send + use<> {
    let argv = cmd.to_string();
    let std_cmd: std::process::Command = cmd.quiet().into();
    async move {
        debug!("exec: {argv}");
        let mut child = tokio::process::Command::from(std_cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn: {argv}"))?;
        // Tee: echo live (long waits print incremental progress) AND
        // accumulate for the benign-match.
        async fn tee(r: impl tokio::io::AsyncRead + Unpin) -> String {
            let mut lines = BufReader::new(r).lines();
            let mut buf = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!("{line}");
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        }
        let (stdout, stderr) = tokio::join!(
            tee(child.stdout.take().expect("set via Stdio::piped() above")),
            tee(child.stderr.take().expect("set via Stdio::piped() above")),
        );
        let status = child.wait().await?;
        if status.success() {
            return Ok(());
        }
        let combined = format!("{stdout}{stderr}");
        if benign(&combined) {
            tracing::info!("(benign failure) {argv}");
            return Ok(());
        }
        for line in combined.lines() {
            ui::eprint(format_args!("  {} {line}\n", style("│").dim()));
        }
        bail!("{argv}: {status}");
    }
}

/// Run `cmd`, capture stdout+stderr, and return `(status, combined)`
/// regardless of exit code or verbosity. For test assertions on a
/// command's failure output: [`run`] folds the last 5 stderr lines
/// into the `Err` only on the non-verbose path (`b4b7f29c7`), so a
/// `-v` QA run gives a caller `{argv}: exit status: 1` and nothing
/// else. This helper bypasses the verbose/inherit short-circuit
/// entirely — it always pipes both streams and never `bail!`s on a
/// non-zero exit (the caller decides what a non-zero exit means).
pub fn run_capture(
    cmd: xshell::Cmd<'_>,
) -> impl Future<Output = Result<(std::process::ExitStatus, String)>> + Send + use<> {
    let argv = cmd.to_string();
    let mut std_cmd: std::process::Command = cmd.quiet().into();
    async move {
        debug!("exec(capture): {argv}");
        std_cmd.stdin(Stdio::null());
        let out = tokio::process::Command::from(std_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("failed to spawn: {argv}"))?;
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Ok((out.status, combined))
    }
}

/// Run a command that must interact with a tty (prompts for input).
/// Inherits stdout/stderr regardless of verbosity, but xshell sets the
/// child's stdin to null when no stdin data is supplied — children
/// that need the real terminal on stdin must spawn a raw `Command`.
pub fn run_interactive(cmd: xshell::Cmd<'_>) -> Result<()> {
    let argv = cmd.to_string();
    debug!("exec (interactive): {argv}");
    cmd.quiet().run().map_err(anyhow::Error::from)
}

/// Blocking variant of [`run`] for sync contexts. Same verbosity
/// handling, but uses std::process instead of tokio (no last-line
/// tail — just captured-then-dump-on-failure).
pub fn run_sync(cmd: xshell::Cmd<'_>) -> Result<()> {
    let argv = cmd.to_string();
    debug!("exec: {argv}");

    if ui::is_verbose() {
        return cmd.quiet().run().map_err(anyhow::Error::from);
    }

    let out = cmd.quiet().ignore_status().output()?;
    if !out.status.success() {
        for line in std::str::from_utf8(&out.stdout)
            .unwrap_or("")
            .lines()
            .chain(std::str::from_utf8(&out.stderr).unwrap_or("").lines())
        {
            ui::eprint(format_args!("  {} {line}\n", style("│").dim()));
        }
        bail!("{argv}: {}", out.status);
    }
    Ok(())
}

/// Captured-output backend for [`try_read`] and [`read`]'s non-verbose
/// path. xshell `Cmd` doesn't support stderr redirect — convert to
/// `std::process::Command`, capture both pipes. On failure, optionally
/// dumps each line to the terminal (`dump_on_err`) then bails with the
/// 512-char HEAD of combined stdout+stderr (head not tail: rio-cli
/// puts the message first then a multi-KB backtrace). The head goes in
/// the error string either way so callers can match on it (idempotent
/// "already exists" checks).
fn read_captured(cmd: xshell::Cmd<'_>, dump_on_err: bool) -> Result<String> {
    let mut std_cmd: std::process::Command = cmd.quiet().into();
    let out = std_cmd.stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        let stdout = std::str::from_utf8(&out.stdout).unwrap_or("");
        let stderr = std::str::from_utf8(&out.stderr).unwrap_or("");
        if dump_on_err {
            for line in stdout.lines().chain(stderr.lines()) {
                ui::eprint(format_args!("  {} {line}\n", style("│").dim()));
            }
        }
        let combined = format!("{stdout}{stderr}");
        let head: String = combined.chars().take(512).collect();
        bail!("command failed: {}: {}", out.status, head.trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

/// Capture stdout as a String. On failure, returns Err with the
/// combined stdout+stderr head in the message (for caller matching)
/// but does NOT dump to the terminal — use [`read`] for that.
/// Intended for callers that inspect the error for idempotent ops.
pub fn try_read(cmd: xshell::Cmd<'_>) -> Result<String> {
    debug!("exec (try_read): {}", cmd);
    read_captured(cmd, false)
}

/// Capture stdout as a String. At default verbosity, stderr is
/// suppressed; at -v+ it streams through (so cargo build progress
/// shows). The output IS the return value — always captured.
pub fn read(cmd: xshell::Cmd<'_>) -> Result<String> {
    debug!("exec (read): {}", cmd);
    if ui::is_verbose() {
        cmd.quiet().read().map_err(anyhow::Error::from)
    } else {
        read_captured(cmd, true)
    }
}

/// Line-read a child stream, updating the span message with each line
/// and returning the full captured buffer.
async fn tail<R: tokio::io::AsyncRead + Unpin>(r: R, prefix: &str) -> String {
    let mut lines = BufReader::new(r).lines();
    let mut buf = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!("{prefix}: {line}");
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
///   `cargo xtask k8s up --kubeconfig` from polluting the user's own
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

/// Repo-local kubeconfig. `k8s up --kubeconfig` writes here; kube-rs, helm,
/// and kubectl read from here via the KUBECONFIG env var `init_env` sets.
pub fn kubeconfig_path() -> PathBuf {
    repo_root().join(".kube/config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_benign_if_swallows_matching_stderr() {
        let sh = Shell::new().unwrap();
        let r = run_benign_if(
            cmd!(
                sh,
                "sh -c 'echo Kubernetes cluster unreachable >&2; exit 1'"
            ),
            |e| e.contains("Kubernetes cluster unreachable"),
        )
        .await;
        assert!(r.is_ok(), "{r:?}");
    }

    #[tokio::test]
    async fn run_benign_if_propagates_nonmatching() {
        let sh = Shell::new().unwrap();
        let r = run_benign_if(
            cmd!(
                sh,
                "sh -c 'echo Kubernetes cluster unreachable >&2; exit 1'"
            ),
            |e| e.contains("zebra"),
        )
        .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn run_benign_if_ok_on_success() {
        let sh = Shell::new().unwrap();
        let r = run_benign_if(cmd!(sh, "true"), |_| false).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn run_capture_returns_status_and_combined_output() {
        // Shell::new(), not shell(): shell() change_dir()s to
        // repo_root(), which falls back to env!("CARGO_MANIFEST_DIR")'s
        // parent when RIO_REPO_ROOT is unset. crate2nix bakes the build
        // sandbox path (/build/xtask → /build) at compile time; that
        // path only exists at *test* runtime if the sandbox is
        // configured with `sandbox-build-dir = /build` (NixOS default,
        // not the determinate nix-installer default). The cwd is
        // irrelevant to run_capture's behavior — same convention as the
        // run_benign_if_* tests above.
        let s = Shell::new().unwrap();
        // Command that writes to both streams and exits 1 — exactly the
        // shape iso03 needs to assert on.
        let (status, out) = run_capture(cmd!(
            s,
            "sh -c 'echo to-stdout; echo to-stderr >&2; exit 1'"
        ))
        .await
        .unwrap();
        assert!(!status.success());
        assert!(out.contains("to-stdout"), "stdout missing: {out:?}");
        assert!(out.contains("to-stderr"), "stderr missing: {out:?}");
    }

    #[tokio::test]
    async fn run_verbose_error_carries_stderr() {
        // The verbose path's bail must include the child's stderr so
        // text-matching retry classifiers (qa's is_transient_gateway_err)
        // and verdict-shape checks (iso03) work under `-v`. b4b7f29c7
        // fixed this for the non-verbose path; this asserts the verbose
        // path matches.
        //
        // The marker is split across two `printf`s so it never appears
        // contiguous in argv — `bail!` always includes `{argv}`, so a
        // literal `echo signal-line` would make the assert tautological.
        crate::ui::set_verbose_for_test(true);
        // Shell::new(), not shell() — see run_capture_returns_status_… above.
        let s = Shell::new().unwrap();
        let err = run(cmd!(
            s,
            "sh -c 'printf rio-tee- >&2; printf marker >&2; exit 1'"
        ))
        .await
        .unwrap_err();
        crate::ui::set_verbose_for_test(false);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rio-tee-marker"),
            "verbose bail dropped stderr: {msg:?}"
        );
    }
}
