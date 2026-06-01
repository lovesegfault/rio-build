//! Privileged end-to-end tests of [`rio_exec::execute`]: real
//! namespaces, real mounts, a real `pivot_root`, a real privilege drop.
//!
//! # Running
//!
//! Every test is `#[ignore]`d so the ordinary unprivileged
//! `cargo nextest run` stays green; they additionally skip themselves
//! (passing, with a message) when not run as root. They run for real
//! with
//!
//! ```text
//! sudo -E cargo nextest run -p rio-exec --run-ignored all
//! ```
//!
//! and inside the project's privileged VM-test harness in a later
//! milestone.
//!
//! # The program under test
//!
//! There is no fixture binary: each test runs the *host's* `sh` with a
//! small script. The shell, the coreutils the scripts call, and their
//! library closures are made visible inside the sandbox by resolving
//! each tool with `command -v`, canonicalizing it, and read-only
//! bind-mounting the host directories that contain the canonical paths
//! (`/nix/store` on a Nix host; `/usr`, `/bin`, `/lib`, `/lib64` on an
//! FHS host). `PATH` inside the sandbox is the set of canonical tool
//! directories, so the scripts run identically on either host flavor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rio_exec::{
    ExecError, ExecEvent, ExecutionOutcome, ExecutionRequest, ExitOutcome, HostLayout, Isolation,
    Limits, LogStream, Mount, OutputCapture, Personality, SandboxIdentity, SetupPhase, execute,
};

/// uid/gid the sandboxed process runs as. Arbitrary unprivileged ids;
/// the isolation-properties test asserts they are observed verbatim.
const SANDBOX_UID: u32 = 1000;
const SANDBOX_GID: u32 = 100;
const HOSTNAME: &str = "rio-test-sandbox";

/// Skip (pass) when the test is not running as root. The sandbox needs
/// CAP_SYS_ADMIN/CAP_SYS_CHROOT/CAP_SETUID/... which plain `sudo`
/// provides; user namespaces are deliberately not used.
macro_rules! require_root {
    () => {
        if !nix::unistd::geteuid().is_root() {
            eprintln!("skipping: this test requires root (run via the privileged harness)");
            return;
        }
    };
}

/// The tools every test script may call (shell builtins like `echo`,
/// `pwd`, and `[` excluded). Resolved on the host and used to derive
/// both the sandbox `PATH` and the read-only provider mounts.
const TOOLS: &[&str] = &[
    "sh", "cat", "ls", "id", "touch", "chmod", "mkdir", "sleep", "tr",
];

struct TestEnv {
    /// Writable scratch backing `/work` (kept alive for host-side
    /// inspection of outputs).
    work: tempfile::TempDir,
    /// The resolved host `sh` (canonical path), used as `program`.
    sh: PathBuf,
    /// Read-only mounts that make `sh` + tools usable inside the
    /// sandbox.
    provider_mounts: Vec<Mount>,
    /// `PATH` value pointing at the canonical tool directories.
    path_env: String,
}

impl TestEnv {
    fn new() -> TestEnv {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut sh = None;
        for tool in TOOLS {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {tool}"))
                .output()
                .expect("resolve tool with command -v");
            assert!(
                out.status.success(),
                "tool `{tool}` not found on the test host"
            );
            let resolved = PathBuf::from(
                std::str::from_utf8(&out.stdout)
                    .expect("command -v output is UTF-8")
                    .trim(),
            );
            if !resolved.is_absolute() {
                // A shell builtin shadowing the external tool; the
                // external one is still found through PATH inside the
                // sandbox via the other tools' directories.
                assert_ne!(*tool, "sh", "sh must resolve to a real path");
                continue;
            }
            let canonical = std::fs::canonicalize(&resolved).expect("canonicalize tool path");
            if *tool == "sh" {
                sh = Some(canonical.clone());
            }
            let dir = canonical
                .parent()
                .expect("tool path has a parent")
                .to_path_buf();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        let sh = sh.expect("sh is in TOOLS");

        // Provider mounts: on a Nix host every canonical path lives
        // under /nix/store and one read-only bind covers everything
        // (including library closures). On an FHS host, bind the usual
        // suspects; `optional` because not all of them exist everywhere.
        let provider_mounts = if dirs.iter().all(|d| d.starts_with("/nix/store")) {
            vec![ro_mount("/nix/store", false)]
        } else {
            ["/usr", "/bin", "/lib", "/lib64", "/etc/alternatives"]
                .iter()
                .map(|p| ro_mount(p, true))
                .collect()
        };
        let path_env = dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(":");

        // The writable scratch. The sandbox runs as SANDBOX_UID, so the
        // directory (and the conventional `out/` subdirectory the
        // scripts write into) must be writable by that uid.
        let work = tempfile::tempdir().expect("work tempdir");
        std::fs::create_dir(work.path().join("out")).expect("create out/");
        for p in [work.path(), &work.path().join("out")] {
            std::os::unix::fs::chown(p, Some(SANDBOX_UID), Some(SANDBOX_GID))
                .expect("chown work dir to the sandbox uid");
        }

        TestEnv {
            work,
            sh,
            provider_mounts,
            path_env,
        }
    }

    /// Host-side path of the writable `/work` mount.
    fn work_host(&self) -> &Path {
        self.work.path()
    }

    /// A request running `sh -c <script>` with the standard test
    /// isolation parameters. Tests adjust fields afterwards.
    fn request(&self, script: &str) -> ExecutionRequest {
        let mut mounts = vec![Mount {
            source: self.work_host().to_path_buf(),
            target: PathBuf::from("/work"),
            writable: true,
            optional: false,
        }];
        mounts.extend(self.provider_mounts.iter().cloned());
        ExecutionRequest {
            program: self.sh.clone(),
            args: vec!["sh".into(), "-c".into(), script.into()],
            env: vec![
                ("PATH".into(), self.path_env.clone().into()),
                ("HOME".into(), "/homeless".into()),
                ("out".into(), "/work/out".into()),
            ],
            cwd: PathBuf::from("/work"),
            mounts,
            extra_devices: vec![],
            inline_files: vec![],
            declared_outputs: vec![],
            capture: OutputCapture::MergedPty,
            isolation: Isolation {
                network: false,
                uid: SANDBOX_UID,
                gid: SANDBOX_GID,
                identity: SandboxIdentity {
                    user: "itest-user".into(),
                    group: "itest-group".into(),
                    gecos: "Integration test user".into(),
                },
                personality: Personality::Native,
                hostname: HOSTNAME.to_string(),
                deny_setuid_and_xattrs: true,
            },
            limits: Limits {
                timeout: Some(Duration::from_secs(120)),
                max_silent: None,
                max_log_bytes: None,
                cgroup: None,
            },
        }
    }
}

fn ro_mount(path: &str, optional: bool) -> Mount {
    Mount {
        source: PathBuf::from(path),
        target: PathBuf::from(path),
        writable: false,
        optional,
    }
}

/// Run a request to completion, collecting every event.
async fn run(req: &ExecutionRequest) -> (Result<ExecutionOutcome, ExecError>, Vec<ExecEvent>) {
    let chroot = tempfile::tempdir().expect("chroot tempdir");
    let host = HostLayout {
        chroot_dir: chroot.path().to_path_buf(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let result = execute(req, &host, tx).await;
    let events = collector.await.expect("event collector");
    (result, events)
}

/// All captured log lines, lossily decoded and newline-joined, with
/// their stream tags discarded.
fn log_text(events: &[ExecEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Log { line, .. } => Some(
                std::str::from_utf8(line)
                    .expect("test scripts emit UTF-8 output")
                    .to_string(),
            ),
            ExecEvent::Started { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_exited(result: &Result<ExecutionOutcome, ExecError>, code: i32) -> ExecutionOutcome {
    let outcome = result
        .as_ref()
        .unwrap_or_else(|e| panic!("execution failed: {e}"));
    assert_eq!(
        outcome.exit,
        ExitOutcome::Exited(code),
        "unexpected exit outcome"
    );
    outcome.clone()
}

// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn merged_pty_captures_stdout_and_stderr_in_order() {
    require_root!();
    let env = TestEnv::new();
    let req = env.request("echo hello; echo world >&2");
    let (result, events) = run(&req).await;
    assert_exited(&result, 0);
    let text = log_text(&events);
    let hello = text.find("hello").expect("stdout line captured");
    let world = text.find("world").expect("stderr line captured");
    assert!(hello < world, "merged capture preserved write order");
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, ExecEvent::Log { stream, .. } if *stream != LogStream::Merged)),
        "merged capture must tag every line as Merged"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ExecEvent::Started { .. })),
        "a Started event is emitted once exec succeeds"
    );
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn separate_pipes_tag_streams() {
    require_root!();
    let env = TestEnv::new();
    let mut req = env.request("echo to-stdout; echo to-stderr >&2");
    req.capture = OutputCapture::SeparatePipes;
    let (result, events) = run(&req).await;
    assert_exited(&result, 0);
    let find = |needle: &str| {
        events.iter().find_map(|e| match e {
            ExecEvent::Log { stream, line, .. }
                if std::str::from_utf8(line).is_ok_and(|l| l.contains(needle)) =>
            {
                Some(*stream)
            }
            _ => None,
        })
    };
    assert_eq!(find("to-stdout"), Some(LogStream::Stdout));
    assert_eq!(find("to-stderr"), Some(LogStream::Stderr));
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn isolation_properties_are_observed_inside_the_sandbox() {
    require_root!();
    let env = TestEnv::new();
    // `ls` sorts its output by default, so the root listing is already
    // deterministic without piping through `sort`.
    let script = r#"
        {
            echo "hostname=$(cat /proc/sys/kernel/hostname)"
            echo "uid=$(id -u)"
            echo "gid=$(id -g)"
            echo "cwd=$(pwd)"
            echo "nofile-soft=$(ulimit -n)"
            echo "nofile-hard=$(ulimit -Hn)"
            echo "etc-hosts=$(cat /etc/hosts | tr '\n' ';')"
            echo "root-listing=$(ls / | tr '\n' ',')"
            echo "---passwd---"
            cat /etc/passwd
        } > /work/out/probe.txt
    "#;
    let mut req = env.request(script);
    req.declared_outputs = vec![PathBuf::from("/work/out/probe.txt")];
    let (result, _) = run(&req).await;
    let outcome = assert_exited(&result, 0);

    let report = &outcome.outputs[0];
    assert!(report.exists, "probe output must exist");
    let probe = std::fs::read_to_string(&report.host_path).expect("read probe host-side");

    assert!(probe.contains(&format!("hostname={HOSTNAME}")), "{probe}");
    assert!(probe.contains(&format!("uid={SANDBOX_UID}")), "{probe}");
    assert!(probe.contains(&format!("gid={SANDBOX_GID}")), "{probe}");
    assert!(probe.contains("cwd=/work"), "{probe}");
    // RLIMIT_NOFILE is pinned by the child setup (daemon-era 1048576),
    // independent of whatever limits this test process itself has.
    assert!(probe.contains("nofile-soft=1048576"), "{probe}");
    assert!(probe.contains("nofile-hard=1048576"), "{probe}");
    // The non-network sandbox synthesizes the same /etc/hosts CppNix
    // writes for every sandboxed build.
    assert!(
        probe.contains("etc-hosts=127.0.0.1 localhost;::1 localhost;"),
        "{probe}"
    );

    // The sandbox root contains exactly the fixed skeleton plus the
    // top-level component of every mount target that actually exists on
    // the host (optional provider mounts may be skipped) — nothing from
    // the host leaks in, and .real-root is gone.
    let mut expected: Vec<String> = vec!["tmp".into(), "etc".into(), "dev".into(), "proc".into()];
    for m in req.mounts.iter().filter(|m| m.source.exists()) {
        let top = m
            .target
            .components()
            .nth(1)
            .map(|c| {
                c.as_os_str()
                    .to_str()
                    .expect("mount targets in this test are UTF-8")
                    .to_string()
            })
            .expect("mount target has a top-level component");
        if !expected.contains(&top) {
            expected.push(top);
        }
    }
    expected.sort();
    let listing_line = probe
        .lines()
        .find(|l| l.starts_with("root-listing="))
        .expect("probe contains the root listing");
    let mut actual: Vec<String> = listing_line
        .trim_start_matches("root-listing=")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    actual.sort();
    assert_eq!(actual, expected, "unexpected sandbox root contents");

    // /etc/passwd is exactly the synthesized database.
    assert!(
        probe.contains(&format!(
            "itest-user:x:{SANDBOX_UID}:{SANDBOX_GID}:Integration test user:/work:/noshell"
        )),
        "{probe}"
    );
    assert!(probe.contains("nobody:x:65534:65534"), "{probe}");
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn network_isolation_leaves_only_loopback() {
    require_root!();
    let env = TestEnv::new();
    let mut req = env.request("cat /proc/net/dev > /work/out/netdev");
    req.declared_outputs = vec![PathBuf::from("/work/out/netdev")];
    let (result, _) = run(&req).await;
    let outcome = assert_exited(&result, 0);
    assert!(outcome.outputs[0].exists);
    let netdev =
        std::fs::read_to_string(&outcome.outputs[0].host_path).expect("read netdev host-side");
    let interfaces: Vec<&str> = netdev
        .lines()
        .skip(2) // the two header lines
        .filter_map(|l| l.split(':').next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(
        interfaces,
        vec!["lo"],
        "the network namespace must contain only loopback: {netdev}"
    );
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn read_only_inputs_reject_writes_and_writable_scratch_accepts_them() {
    require_root!();
    let env = TestEnv::new();
    // A read-only input directory with a canary file.
    let inputs = tempfile::tempdir().expect("inputs tempdir");
    std::fs::write(inputs.path().join("canary.txt"), "original").expect("write canary");
    let script = r#"
        if echo tampered > /inputs/canary.txt 2>/dev/null; then
            echo RO_WRITE_SUCCEEDED
        else
            echo RO_WRITE_FAILED
        fi
        echo scratch-write > /work/out/scratch.txt
    "#;
    let mut req = env.request(script);
    req.mounts.push(Mount {
        source: inputs.path().to_path_buf(),
        target: PathBuf::from("/inputs"),
        writable: false,
        optional: false,
    });
    req.declared_outputs = vec![PathBuf::from("/work/out/scratch.txt")];
    let (result, events) = run(&req).await;
    let outcome = assert_exited(&result, 0);

    let text = log_text(&events);
    assert!(text.contains("RO_WRITE_FAILED"), "{text}");
    assert!(!text.contains("RO_WRITE_SUCCEEDED"), "{text}");
    // The canary is untouched on the host.
    assert_eq!(
        std::fs::read_to_string(inputs.path().join("canary.txt")).expect("re-read canary"),
        "original"
    );
    // The writable mount accepted the write.
    assert!(outcome.outputs[0].exists);
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn seccomp_denies_setuid_and_setgid_modes_without_killing_the_build() {
    require_root!();
    let env = TestEnv::new();
    let script = r#"
        touch f
        if chmod 4755 f; then echo SETUID_ALLOWED; else echo SETUID_DENIED; fi
        if chmod 2755 f; then echo SETGID_ALLOWED; else echo SETGID_DENIED; fi
        if chmod 0755 f; then echo PLAIN_CHMOD_OK; fi
    "#;
    let req = env.request(script);
    let (result, events) = run(&req).await;
    assert_exited(&result, 0);
    let text = log_text(&events);
    assert!(text.contains("SETUID_DENIED"), "{text}");
    assert!(text.contains("SETGID_DENIED"), "{text}");
    assert!(text.contains("PLAIN_CHMOD_OK"), "{text}");
    assert!(!text.contains("SETUID_ALLOWED"), "{text}");
    assert!(!text.contains("SETGID_ALLOWED"), "{text}");
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn missing_declared_output_is_reported_not_judged() {
    require_root!();
    let env = TestEnv::new();
    let mut req = env.request("true");
    req.declared_outputs = vec![PathBuf::from("/work/out/never-created")];
    let (result, _) = run(&req).await;
    let outcome = assert_exited(&result, 0);
    assert_eq!(outcome.outputs.len(), 1);
    assert!(!outcome.outputs[0].exists);
    assert!(outcome.outputs[0].metadata.is_none());
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn nonzero_exit_codes_pass_through() {
    require_root!();
    let env = TestEnv::new();
    let req = env.request("exit 7");
    let (result, _) = run(&req).await;
    assert_exited(&result, 7);
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn timeout_kills_the_tree() {
    require_root!();
    let env = TestEnv::new();
    let mut req = env.request("sleep 60");
    req.limits.timeout = Some(Duration::from_secs(2));
    let started = Instant::now();
    let (result, _) = run(&req).await;
    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the kill must take effect promptly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn silence_limit_kills_the_tree() {
    require_root!();
    let env = TestEnv::new();
    let mut req = env.request("echo start; sleep 60");
    req.limits.max_silent = Some(Duration::from_secs(2));
    let started = Instant::now();
    let (result, events) = run(&req).await;
    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::Silent);
    assert!(log_text(&events).contains("start"));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the kill must take effect promptly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn log_limit_kills_the_tree() {
    require_root!();
    let env = TestEnv::new();
    let script = r#"
        i=0
        while [ "$i" -lt 100000 ]; do
            echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            i=$((i+1))
        done
        sleep 30
    "#;
    let mut req = env.request(script);
    req.limits.max_log_bytes = Some(10_000);
    let started = Instant::now();
    let (result, _) = run(&req).await;
    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::LogLimitExceeded);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the kill must take effect promptly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn timeout_applies_after_the_program_closes_its_output() {
    require_root!();
    let env = TestEnv::new();
    // The program closes its stdout and stderr (EOF on the capture
    // side) and then keeps running: limit enforcement must not stop
    // with the capture stream.
    let mut req = env.request("exec >&- 2>&-; sleep 60");
    req.limits.timeout = Some(Duration::from_secs(2));
    let started = Instant::now();
    let (result, _) = run(&req).await;
    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the kill must take effect promptly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn missing_program_surfaces_as_an_exec_setup_failure() {
    require_root!();
    let env = TestEnv::new();
    let mut req = env.request("true");
    req.program = PathBuf::from("/does-not-exist");
    req.args = vec!["does-not-exist".into()];
    let (result, _) = run(&req).await;
    match result {
        Err(ExecError::Setup(err)) => {
            assert_eq!(err.phase, SetupPhase::Exec);
            assert_eq!(err.errno, libc::ENOENT);
        }
        other => panic!("expected an Exec setup failure, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Control-plane isolation: enforcement under stalled/slow/dropped consumers.
// ---------------------------------------------------------------------------

/// THE merged_bug_019 executor pin: the wall-clock timeout fires while
/// the events receiver is alive but never drained. Pre-isolation, the
/// supervision loop parked on the awaited event send the moment the
/// channel filled, and the deadline arms parked with it — this test
/// hung until the harness gave up.
// r[verify builder.exec.limits-isolated]
#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn timeout_fires_with_stalled_receiver() {
    require_root!();
    let env = TestEnv::new();
    // Endlessly chatty: the event channel and every internal buffer
    // fill long before the deadline.
    let mut req = env.request("while true; do echo chatter; done");
    req.limits.timeout = Some(Duration::from_secs(2));

    let chroot = tempfile::tempdir().expect("chroot tempdir");
    let host = HostLayout {
        chroot_dir: chroot.path().to_path_buf(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    // The receiver is HELD, never drained, for the whole execution.
    let started = Instant::now();
    let result = execute(&req, &host, tx).await;
    drop(rx);

    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "the kill must fire near the 2s deadline with zero consumption, took {:?}",
        started.elapsed()
    );
}

/// The post-reap drain bound, pinned with a receiver that exists but
/// never consumes: a naturally exiting build must not park execute()
/// on undelivered events past the drain budget. (This is the bound
/// the round-14 drain commit deferred to a non-vacuous staging: the
/// build here produces real queued output, unlike a SetupFailed run.)
// r[verify builder.exec.limits-isolated]
#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn drain_bound_holds_with_stalled_receiver_at_exit() {
    require_root!();
    let env = TestEnv::new();
    // Chatty but FINITE: the program exits on its own with output
    // still queued upstream of the held receiver.
    let req = env.request("i=0; while [ $i -lt 5000 ]; do echo line-$i; i=$((i+1)); done");

    let chroot = tempfile::tempdir().expect("chroot tempdir");
    let host = HostLayout {
        chroot_dir: chroot.path().to_path_buf(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let started = Instant::now();
    let result = execute(&req, &host, tx).await;
    drop(rx);

    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::Exited(0));
    // Generous multiple of FINAL_DRAIN_TIMEOUT (2s): the build itself
    // takes a moment; the point is "no unbounded park".
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "execute() must return within the drain budget of the exit, took {:?}",
        started.elapsed()
    );
}

/// Nothing drops while the receiver LIVES: a slow but draining
/// consumer receives every line the program emitted, in order —
/// backpressure pauses the build, it never discards.
#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn no_drop_with_slow_draining_receiver() {
    require_root!();
    let env = TestEnv::new();
    const LINES: usize = 2000;
    let req = env.request(&format!(
        "i=0; while [ $i -lt {LINES} ]; do echo line-$i; i=$((i+1)); done"
    ));

    let chroot = tempfile::tempdir().expect("chroot tempdir");
    let host = HostLayout {
        chroot_dir: chroot.path().to_path_buf(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let collector = tokio::spawn(async move {
        let mut lines = Vec::new();
        while let Some(event) = rx.recv().await {
            // Slow consumer: a small per-event delay forces sustained
            // backpressure through the queue, channel, and pipe.
            tokio::time::sleep(Duration::from_micros(200)).await;
            if let ExecEvent::Log { line, .. } = event {
                lines.push(
                    std::str::from_utf8(&line)
                        .expect("test scripts emit UTF-8 output")
                        .to_owned(),
                );
            }
        }
        lines
    });
    let result = execute(&req, &host, tx).await;
    let lines = collector.await.expect("collector");

    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::Exited(0));
    let expected: Vec<String> = (0..LINES).map(|i| format!("line-{i}")).collect();
    assert_eq!(
        lines, expected,
        "a live receiver must observe every line, in order"
    );
}

/// A receiver dropped MID-BUILD discards events and lets the build run
/// free: chunk consumption continues (the readers never block on a
/// consumer that no longer exists) and the build completes promptly.
#[tokio::test]
#[ignore = "requires root + CAP_SYS_ADMIN; run via the privileged test harness"]
async fn receiver_dropped_build_runs_free() {
    require_root!();
    let env = TestEnv::new();
    // Enough output to overrun every buffer if consumption stopped
    // mattering, then a clean exit.
    let req = env.request(
        "i=0; while [ $i -lt 20000 ]; do echo chatter-line-$i; i=$((i+1)); done; echo done-marker",
    );

    let chroot = tempfile::tempdir().expect("chroot tempdir");
    let host = HostLayout {
        chroot_dir: chroot.path().to_path_buf(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let started = Instant::now();
    let exec = tokio::spawn({
        let req = req.clone();
        let host = HostLayout {
            chroot_dir: host.chroot_dir.clone(),
        };
        async move { execute(&req, &host, tx).await }
    });
    // Take a few events, then vanish mid-build.
    for _ in 0..3 {
        let _ = rx.recv().await;
    }
    drop(rx);

    let result = exec.await.expect("execute task");
    let outcome = result.expect("execution itself succeeds");
    assert_eq!(outcome.exit, ExitOutcome::Exited(0));
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "a dropped receiver must not throttle the build, took {:?}",
        started.elapsed()
    );
}
