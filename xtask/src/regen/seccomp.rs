//! Diff the worker seccomp profile against upstream moby.
//!
//! Moby's default.json has conditional blocks keyed on capabilities.
//! Flatten for the caps the worker HAS, remove the syscalls we deny
//! (per security.typ r[builder.seccomp.localhost-profile+3]), then diff.
//!
//! The flattening is approximate — moby's format has arch-specific
//! blocks, minKernel conditionals. Produces a diff for HUMAN REVIEW.

use anyhow::{Result, bail};
use serde_json::Value;
use tracing::info;

use crate::sh::{cmd, repo_root, shell};

/// Checked-in builder seccomp profile (ADR-021: baked into the NixOS
/// node AMI; the helm `files/` copy was removed).
const PROFILE_PATH: &str = "nix/nixos-node/seccomp/rio-builder.json";

const WORKER_CAPS: &[&str] = &["CAP_SYS_ADMIN", "CAP_SYS_CHROOT"];

/// Syscalls the builder profile MUST NOT allow (per security.typ
/// r[builder.seccomp.localhost-profile+3]). Single source of truth —
/// `regen seccomp` strips these from the moby diff baseline, and
/// `lint seccomp-allowlist` asserts they're absent from the checked-in
/// profile's ALLOW blocks.
///
/// The `io_uring_setup`/`io_uring_enter`/`io_uring_register` trio is
/// likewise deliberately NOT denied in the builder profile: the worker's
/// castore-FUSE serves exclusively over Linux 6.14 fuse-over-io_uring
/// (RuntimeDefault has denied io_uring since Docker v24, so it must be
/// re-allowed here or no castore mount can come up). The
/// builder pod runs one container — the FUSE-serving worker and the
/// untrusted Nix build share it — so this exposes io_uring to build
/// code, not only to the trusted worker (Kubernetes seccomp is
/// per-container, and rio-builder installs no nested filter on the build
/// child). Same residual-risk acceptance as the ptrace note below: the
/// cluster is single-tenant today — revisit (e.g. a nested seccomp
/// filter on the build child, or splitting the FUSE serve into its own
/// process/profile) before onboarding untrusted tenants. The fetcher
/// profile keeps the trio denied (`FETCHER_EXTRA_DENIED` in lint.rs).
///
/// `ptrace` and `process_vm_readv` are deliberately NOT in this set —
/// they are allowed (and `lint seccomp-allowlist` asserts they STAY in
/// an ALLOW block). Denying them breaks every build whose check phase
/// traces its own processes: LeakSanitizer's at-exit stop-the-world
/// attaches a tracer to the leaking process (all rio-fuzz-* checks died
/// with "LeakSanitizer has encountered a fatal error"), and strace-/
/// gdb-driven test suites fork-and-trace. The mitigating control is the
/// Yama LSM (active on the builder nodes, `kernel.yama.ptrace_scope=1`
/// pinned in nix/nixos-node/hardening.nix): a process may only trace
/// its own descendants, which is exactly what a check phase needs and
/// close to nothing for lateral movement. The write side
/// (`process_vm_writev`) stays denied — no test harness needs to write
/// another process's memory. Residual risk accepted: the kernel's
/// ptrace code paths become reachable from untrusted build code; the
/// cluster is single-tenant today — revisit before onboarding untrusted
/// tenants. The fetcher profile keeps both denied
/// (`FETCHER_EXTRA_DENIED` in lint.rs): FOD fetch scripts have no check
/// phase and fetchers face the open internet.
pub(crate) const DENIED: &[&str] = &["bpf", "setns", "process_vm_writev"];

pub async fn run(tag: &str) -> Result<()> {
    let ours = repo_root().join(PROFILE_PATH);
    anyhow::ensure!(
        ours.exists(),
        "checked-in profile not found at {} — path moved? update PROFILE_PATH",
        ours.display()
    );
    let url =
        format!("https://raw.githubusercontent.com/moby/moby/{tag}/profiles/seccomp/default.json");

    info!("fetching moby {tag} default.json");
    let mut v: Value = reqwest::get(&url).await?.error_for_status()?.json().await?;

    // Flatten: keep syscall blocks whose includes.caps ⊆ WORKER_CAPS.
    let syscalls = v["syscalls"].as_array_mut().expect("moby format changed");
    syscalls.retain(|block| {
        block["includes"]["caps"]
            .as_array()
            .map(|caps| {
                caps.iter()
                    .all(|c| WORKER_CAPS.contains(&c.as_str().unwrap_or("")))
            })
            .unwrap_or(true)
    });

    // Remove denied syscalls from every names array.
    for block in syscalls.iter_mut() {
        if let Some(names) = block["names"].as_array_mut() {
            names.retain(|n| !DENIED.contains(&n.as_str().unwrap_or("")));
        }
    }

    let theirs = serde_json::to_string_pretty(&v)? + "\n";
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &theirs)?;

    let sh = shell()?;
    let ours_s = ours.to_str().unwrap();
    let tmp_s = tmp.path().to_str().unwrap();
    if crate::sh::run_interactive(cmd!(sh, "diff -u {ours_s} {tmp_s}")).is_err() {
        bail!(
            "DRIFT: moby {tag} differs from checked-in profile.\n\
             Review the diff. If moby added safe syscalls, update {}.\n\
             If moby removed syscalls, check whether worker builds need them.",
            ours.display()
        );
    }
    info!("no drift vs moby {tag}");
    Ok(())
}

// No path-exists unit test: crate2nix per-crate builds copy only the
// xtask/ subtree into the test sandbox, so `repo_root().join(...)` never
// resolves there. The `ensure!(ours.exists())` above is the runtime guard.
