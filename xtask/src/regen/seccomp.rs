//! Diff the worker seccomp profile against upstream moby.
//!
//! Moby's default.json has conditional blocks keyed on capabilities.
//! Flatten for the caps the worker HAS, remove the syscalls we deny
//! (per security.md r[worker.seccomp.localhost-profile]), then diff.
//!
//! The flattening is approximate — moby's format has arch-specific
//! blocks, minKernel conditionals. Produces a diff for HUMAN REVIEW.

use anyhow::{Result, bail};
use serde_json::Value;
use tracing::info;

use crate::sh::{cmd, repo_root, shell};

const WORKER_CAPS: &[&str] = &["CAP_SYS_ADMIN", "CAP_SYS_CHROOT"];
const DENIED: &[&str] = &[
    "ptrace",
    "bpf",
    "setns",
    "process_vm_readv",
    "process_vm_writev",
];

pub async fn run(tag: &str) -> Result<()> {
    let ours = repo_root().join("infra/helm/rio-build/files/seccomp-rio-worker.json");
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
    if cmd!(sh, "diff -u {ours_s} {tmp_s}").run().is_err() {
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
