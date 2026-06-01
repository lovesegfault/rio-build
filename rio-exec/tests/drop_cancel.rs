//! Unprivileged integration tests of [`rio_exec::execute`]'s
//! cancellation safety: dropping the future at any point must never
//! leak the forked process tree — no running orphan, no unreaped
//! zombie.
//!
//! These run WITHOUT root. The executions here never construct a
//! working sandbox (unprivileged `unshare` fails with `EPERM`); that is
//! fine — the property under test is about the process tree the
//! executor forks, not about the sandbox the children would have built.
//!
//! The assertions observe `/proc` instead of calling `waitpid(-1, …)`:
//! reaping is the *executor's* job (its guard or its wait task), and a
//! test that reaps would mask exactly the zombie leak it exists to
//! catch.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rio_exec::{
    ExecutionRequest, HostLayout, Isolation, Limits, Mount, OutputCapture, Personality, execute,
};

/// Pids of all current children (running or zombie) of this process,
/// scraped from `/proc/<pid>/stat` ppid fields.
fn child_pids() -> Vec<i32> {
    let self_pid = std::process::id() as i32;
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // /proc pid directories are pure ASCII digits; anything else
        // (including non-UTF-8 names) is not a pid dir.
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // stat: "pid (comm) state ppid ..." — comm may contain spaces
        // and parens, so split after the LAST ')'.
        let Some(after_comm) = stat.rsplit(')').next() else {
            continue;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // fields[0] = state, fields[1] = ppid
        if fields.len() >= 2 && fields[1].parse::<i32>() == Ok(self_pid) {
            pids.push(pid);
        }
    }
    pids
}

/// Assert that within `deadline` this process has no children at all —
/// the executor's own machinery must have killed AND reaped everything
/// it forked.
fn assert_no_children_within(deadline: Duration, context: &str) {
    let start = Instant::now();
    loop {
        let children = child_pids();
        if children.is_empty() {
            return;
        }
        assert!(
            start.elapsed() < deadline,
            "{context}: child processes still exist {deadline:?} after the drop \
             (leaked or unreaped): {children:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A request that passes validation and reaches the fork. It does not
/// need to produce a working sandbox.
fn minimal_request(work: &Path) -> ExecutionRequest {
    ExecutionRequest {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("sleep 30"),
        ],
        env: vec![],
        cwd: PathBuf::from("/work"),
        mounts: vec![Mount {
            source: work.to_path_buf(),
            target: PathBuf::from("/work"),
            writable: true,
            optional: false,
        }],
        extra_devices: vec![],
        inline_files: vec![],
        declared_outputs: vec![],
        capture: OutputCapture::MergedPty,
        isolation: Isolation {
            network: false,
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            personality: Personality::Native,
            hostname: "drop-cancel-test".to_string(),
            deny_setuid_and_xattrs: false,
        },
        limits: Limits {
            timeout: Some(Duration::from_secs(30)),
            max_silent: None,
            max_log_bytes: None,
            cgroup: None,
        },
    }
}

/// Dropping the execute() future mid-flight — at whatever await point
/// the abort happens to land on, including the fork's own
/// spawn_blocking await — must never leak the process tree. Before the
/// ProcessTreeGuard restructure, an abort during the fork await leaked
/// the freshly forked intermediate (no guard was armed yet); with the
/// fd keep-set fix it would exit on go-pipe EOF but stay an unreaped
/// zombie forever.
// r[verify builder.exec.tree-ownership]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_execute_mid_flight_never_leaks_the_process_tree() {
    for i in 0u64..10 {
        let work = tempfile::tempdir().expect("workdir");
        let chroot = tempfile::tempdir().expect("chroot dir");
        let host = HostLayout {
            chroot_dir: chroot.path().to_path_buf(),
        };
        let request = minimal_request(work.path());
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        let task = tokio::spawn(async move {
            // The outcome is irrelevant (unprivileged setup fails);
            // only the process-tree hygiene matters.
            let _ = execute(&request, &host, tx).await;
        });

        // Vary the drop timing relative to the fork: iteration 0
        // aborts essentially immediately (racing the spawn_blocking
        // submission), later iterations land progressively further
        // into the execution.
        tokio::time::sleep(Duration::from_millis(i * 3)).await;
        task.abort();
        let _ = task.await;
        drop(rx);

        // The executor's own guard / wait task must clean up: no
        // running child, no zombie. (Detached blocking tasks finish
        // their kill+reap after the abort; the deadline gives them
        // time.)
        assert_no_children_within(Duration::from_secs(10), &format!("iteration {i}"));
    }
}

/// Dropping the future before it is ever polled must fork nothing.
// r[verify builder.exec.tree-ownership]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_execute_before_polling_leaks_nothing() {
    let work = tempfile::tempdir().expect("workdir");
    let chroot = tempfile::tempdir().expect("chroot dir");
    let host = HostLayout {
        chroot_dir: chroot.path().to_path_buf(),
    };
    let request = minimal_request(work.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);

    let fut = execute(&request, &host, tx);
    drop(fut);

    assert!(
        child_pids().is_empty(),
        "an unpolled execute() future must not have forked anything"
    );
}
