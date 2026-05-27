//! Batch submission to the rio gateway over ssh-ng.
//!
//! [`Submitter`] is the trait the submit and warm stages drive, so both
//! stages stay unit-testable against a scripted in-memory fake.
//! [`NixSubmitter`] is the real implementation: per batch it imports the
//! batch's derivation closures from the eval set's drv archive into the
//! local store, then runs one stock `nix build` against the gateway's
//! ssh-ng store URL, streaming the child's stderr through the
//! gateway-line parser so the rio build id and the relayed
//! per-derivation failure reasons are captured live.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::batch::Batch;
use super::stderrparse::{ParsedStderr, parse_line};

/// Result of one batch submission attempt (one `nix build` child process).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchOutcome {
    /// Build id parsed from the gateway's `rio: build <uuid>` line.
    pub build_id: Option<String>,
    /// Child exit code (`None` = killed by a signal, including the engine's
    /// own batch timeout).
    pub exit_code: Option<i32>,
    /// drv path → relayed failure reason, captured live from stderr.
    pub reasons: BTreeMap<String, String>,
    /// Last ~200 stderr lines, kept verbatim as raw evidence for
    /// batches.jsonl.
    pub stderr_tail: String,
    /// True when the engine killed the child (batch timeout / abort) rather
    /// than the child exiting on its own.
    pub engine_cancelled: bool,
}

/// One batch-submission backend. The submit and warm stages only ever talk
/// to this trait; unit tests script it with an in-memory fake while a real
/// campaign uses [`NixSubmitter`].
#[async_trait]
pub trait Submitter: Send + Sync {
    /// Submit one batch under the given store URL and wait for the child to
    /// exit (or for `timeout` to kill it).
    async fn submit_batch(
        &self,
        store_url: &str,
        batch: &Batch,
        timeout: Duration,
    ) -> Result<BatchOutcome>;
}

/// SSH options for the ssh-ng transport, exported to the `nix` children via
/// `NIX_SSHOPTS`: no host-key prompts (cluster endpoints are ephemeral),
/// client-side keepalives so long silent builds do not trip the gateway's
/// idle timeout, no connection multiplexing (a stale ControlMaster left by a
/// killed run would wedge later runs), and no ssh-agent involvement (a dead
/// forwarded agent socket hangs the handshake before key exchange). Same
/// option set as `xtask/src/k8s/shared.rs`'s `NIX_SSHOPTS_BASE`, which
/// documents the incident history behind each flag.
pub const NIX_SSHOPTS: &str = "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o ServerAliveInterval=30 -o ServerAliveCountMax=6 \
     -o ControlMaster=no -o ControlPath=none \
     -o IdentityAgent=none -o IdentitiesOnly=yes";

/// How many trailing stderr lines to keep as raw evidence.
const STDERR_TAIL_LINES: usize = 200;

/// Timeout for the pre-submission drv import. Generous on purpose: the
/// import copies tiny `.drv` text files from a local on-disk archive, so
/// even very large closures finish orders of magnitude faster than this.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(1800);

/// [`Submitter`] that shells out to a stock `nix` binary.
pub struct NixSubmitter {
    /// Untarred eval-set drv archive (an uncompressed `file://` binary-cache
    /// layout) used to import each batch's drv closures into the local store
    /// before submission.
    pub drv_archive_dir: PathBuf,
    /// Program to invoke (`nix` from `PATH` by default). Tests point this at
    /// `sh` so the child-streaming and timeout paths are covered without a
    /// nix binary or a cluster.
    pub nix_bin: String,
    /// Extra environment for the children (HOME/XDG for the non-root
    /// container come from the pod env; this is for overrides/tests).
    pub extra_env: BTreeMap<String, String>,
}

impl NixSubmitter {
    pub fn new(drv_archive_dir: PathBuf) -> Self {
        Self {
            drv_archive_dir,
            nix_bin: "nix".to_string(),
            extra_env: BTreeMap::new(),
        }
    }

    /// `nix copy --derivation --no-check-sigs --from file://<archive>
    /// <roots…>`: copies the root drvs and their derivation closures from
    /// the archive layout into the local store. Cheap (drvs are tiny text
    /// files) and idempotent (already-present paths are skipped);
    /// `--no-check-sigs` because the archive layout is unsigned.
    pub fn import_args(&self, batch: &Batch) -> Vec<String> {
        let mut args = vec![
            "copy".to_string(),
            "--extra-experimental-features".to_string(),
            "nix-command".to_string(),
            "--derivation".to_string(),
            "--no-check-sigs".to_string(),
            "--from".to_string(),
            format!("file://{}", self.drv_archive_dir.display()),
        ];
        args.extend(batch.root_drvs.iter().cloned());
        args
    }

    /// `nix build -L --no-link --store <url> <drv^*…>`: one submission per
    /// batch. `-L` is required so the gateway's relayed lines reach stderr
    /// in full; the installables are explicit `.drv^*` paths so nothing is
    /// evaluated client-side, and no `--eval-store` override is passed — the
    /// local store already holds the imported derivations and `--store`
    /// alone points the build at the gateway.
    pub fn build_args(store_url: &str, batch: &Batch) -> Vec<String> {
        let mut args = vec![
            "build".to_string(),
            "--extra-experimental-features".to_string(),
            "nix-command".to_string(),
            "-L".to_string(),
            "--no-link".to_string(),
            "--store".to_string(),
            store_url.to_string(),
        ];
        args.extend(batch.root_drvs.iter().map(|d| format!("{d}^*")));
        args
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.nix_bin);
        cmd.args(args)
            .env("NIX_SSHOPTS", NIX_SSHOPTS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Dropping the future mid-flight (engine abort, task cancel)
            // must not orphan a still-running child.
            .kill_on_drop(true);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Run one child, streaming its stderr through the gateway-line parser
    /// and keeping the trailing lines as raw evidence; kill it at `timeout`.
    async fn run_child(&self, args: &[String], timeout: Duration) -> Result<BatchOutcome> {
        let mut child = self.command(args).spawn().with_context(|| {
            format!(
                "spawn {} {}",
                self.nix_bin,
                args.first().cloned().unwrap_or_default()
            )
        })?;
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = BufReader::new(stderr).lines();
        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();

        let deadline = tokio::time::Instant::now() + timeout;
        let mut engine_cancelled = false;
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line? {
                        Some(line) => {
                            parse_line(&mut parsed, &line);
                            if tail.len() == STDERR_TAIL_LINES {
                                tail.pop_front();
                            }
                            tail.push_back(line);
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        "batch timeout reached; killing the nix child (in-flight builds are \
                         cancelled by the gateway on disconnect and re-run on resume)"
                    );
                    let _ = child.start_kill();
                    engine_cancelled = true;
                    break;
                }
            }
        }
        let status = child.wait().await.context("wait for nix child")?;
        Ok(BatchOutcome {
            build_id: parsed.build_id,
            exit_code: status.code(),
            reasons: parsed.reasons,
            stderr_tail: Vec::from(tail).join("\n"),
            engine_cancelled,
        })
    }
}

#[async_trait]
impl Submitter for NixSubmitter {
    async fn submit_batch(
        &self,
        store_url: &str,
        batch: &Batch,
        timeout: Duration,
    ) -> Result<BatchOutcome> {
        // Import the batch's drv closures from the archive into the local
        // store first (cheap; idempotent), then submit.
        let import = self
            .run_child(&self.import_args(batch), IMPORT_TIMEOUT)
            .await?;
        if import.exit_code != Some(0) {
            // The import child's stderr is not persisted anywhere else, so
            // carry a clipped tail of it in the error.
            let last_lines: Vec<&str> = import.stderr_tail.lines().rev().take(5).collect();
            let last_lines: Vec<&str> = last_lines.into_iter().rev().collect();
            anyhow::bail!(
                "drv import from {} failed (exit {:?}): {}",
                self.drv_archive_dir.display(),
                import.exit_code,
                crate::body_snippet(&last_lines.join(" | "))
            );
        }
        self.run_child(&Self::build_args(store_url, batch), timeout)
            .await
    }
}

/// The per-job repro command recorded alongside each job result, so a human
/// can re-drive exactly one derivation through the same gateway. The
/// `ssh-key` query parameter is stripped from the store URL — secrets never
/// land in campaign artifacts.
pub fn repro_command(store_url: &str, drv_path: &str) -> String {
    let sanitized: String = match store_url.split_once('?') {
        Some((base, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|kv| !kv.starts_with("ssh-key="))
                .collect();
            if kept.is_empty() {
                base.to_string()
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        }
        None => store_url.to_string(),
    };
    format!("nix build -L --no-link --store '{sanitized}' '{drv_path}^*'")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Scripted [`Submitter`] for stage-level tests: pops pre-programmed
    /// outcomes and records every submitted batch.
    #[derive(Default)]
    pub struct FakeSubmitter {
        /// Outcomes are popped from the BACK, so when scripting several
        /// batches push the LAST batch's outcome first. An exhausted script
        /// yields `BatchOutcome::default()`.
        pub outcomes: Mutex<Vec<BatchOutcome>>,
        /// `(store_url, batch)` of every `submit_batch` call, in call order.
        pub submitted: Mutex<Vec<(String, Batch)>>,
    }

    #[async_trait]
    impl Submitter for FakeSubmitter {
        async fn submit_batch(
            &self,
            store_url: &str,
            batch: &Batch,
            _timeout: Duration,
        ) -> Result<BatchOutcome> {
            self.submitted
                .lock()
                .unwrap()
                .push((store_url.to_string(), batch.clone()));
            Ok(self.outcomes.lock().unwrap().pop().unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> Batch {
        Batch {
            jobs: vec!["libfoo.x86_64-linux".into(), "app.x86_64-linux".into()],
            root_drvs: vec![
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv".into(),
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv".into(),
            ],
            est_nodes: 17,
        }
    }

    #[test]
    fn import_and_build_command_shapes() {
        let sub = NixSubmitter::new(PathBuf::from("/scratch/drv-archive"));
        let import = sub.import_args(&batch());
        assert_eq!(
            import,
            vec![
                "copy",
                "--extra-experimental-features",
                "nix-command",
                "--derivation",
                "--no-check-sigs",
                "--from",
                "file:///scratch/drv-archive",
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv",
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        let build = NixSubmitter::build_args(
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/secrets/parity-leaf",
            &batch(),
        );
        assert_eq!(build[0], "build");
        assert!(
            build.contains(&"-L".to_string()),
            "must run with -L so relayed lines are captured"
        );
        assert!(build.contains(&"--no-link".to_string()));
        assert!(!build.contains(&"--eval-store".to_string()));
        assert_eq!(
            build.last().unwrap(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv^*"
        );
    }

    #[test]
    fn nix_sshopts_match_harness_requirements() {
        for needle in [
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/dev/null",
            "ServerAliveInterval=30",
            "ControlMaster=no",
            "IdentitiesOnly=yes",
        ] {
            assert!(NIX_SSHOPTS.contains(needle), "NIX_SSHOPTS missing {needle}");
        }
    }

    #[test]
    fn repro_command_strips_ssh_key() {
        let r = repro_command(
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/secrets/parity-leaf",
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv",
        );
        assert!(!r.contains("ssh-key"), "{r}");
        assert!(r.contains("compress=true"), "{r}");
        assert!(r.ends_with("-libfoo-1.0.drv^*'"), "{r}");
        // No query at all stays untouched.
        let r2 = repro_command("ssh-ng://rio@host:22", "/nix/store/x.drv");
        assert!(r2.contains("'ssh-ng://rio@host:22'"));
    }

    /// Outcome assembly from a real child process: `sh` stands in for `nix`
    /// so the streaming parse, the tail cap, and exit-code capture are
    /// covered without a nix binary or a cluster.
    #[tokio::test]
    async fn run_child_streams_and_parses_stderr() {
        let mut sub = NixSubmitter::new(PathBuf::from("/nonexistent"));
        sub.nix_bin = "sh".to_string();
        let script = concat!(
            "i=0\n",
            "while [ \"$i\" -lt 250 ]; do echo \"noise line $i\" >&2; i=$((i+1)); done\n",
            "echo \"rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a (trace 4bf92f3577b34da6a3ce929d0e0e4736)\" >&2\n",
            "echo \"derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: poison threshold reached after 3 distinct-worker failures\" >&2\n",
        );
        let out = sub
            .run_child(
                &["-c".to_string(), script.to_string()],
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.engine_cancelled);
        assert_eq!(
            out.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(out.reasons.len(), 1);
        assert_eq!(
            out.reasons["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv"],
            "poison threshold reached after 3 distinct-worker failures"
        );
        // The tail keeps only the last STDERR_TAIL_LINES of the 252 emitted
        // lines, so the first 52 noise lines must have been dropped.
        let tail: Vec<&str> = out.stderr_tail.lines().collect();
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        assert_eq!(tail[0], "noise line 52");
        assert!(tail.last().unwrap().starts_with("derivation '"));
    }

    /// Pin the [`test_support::FakeSubmitter`] scripting contract that the
    /// stage-level tests rely on: outcomes pop from the BACK, an exhausted
    /// script yields a default outcome, and every submission is recorded in
    /// call order.
    #[tokio::test]
    async fn fake_submitter_pops_outcomes_from_the_back() {
        use super::test_support::FakeSubmitter;
        let fake = FakeSubmitter::default();
        for id in ["second", "first"] {
            fake.outcomes.lock().unwrap().push(BatchOutcome {
                build_id: Some(id.to_string()),
                ..BatchOutcome::default()
            });
        }
        let b = batch();
        let timeout = Duration::from_secs(1);
        let first = fake.submit_batch("ssh-ng://x", &b, timeout).await.unwrap();
        let second = fake.submit_batch("ssh-ng://x", &b, timeout).await.unwrap();
        let drained = fake.submit_batch("ssh-ng://x", &b, timeout).await.unwrap();
        assert_eq!(first.build_id.as_deref(), Some("first"));
        assert_eq!(second.build_id.as_deref(), Some("second"));
        assert_eq!(drained, BatchOutcome::default());
        let submitted = fake.submitted.lock().unwrap();
        assert_eq!(submitted.len(), 3);
        assert_eq!(submitted[0].0, "ssh-ng://x");
        assert_eq!(submitted[0].1, b);
    }

    /// The engine deadline kills a child that outlives it and reports the
    /// kill as `engine_cancelled` with no exit code.
    #[tokio::test]
    async fn run_child_kills_the_child_at_the_timeout() {
        let mut sub = NixSubmitter::new(PathBuf::from("/nonexistent"));
        sub.nix_bin = "sh".to_string();
        let out = sub
            .run_child(
                &["-c".to_string(), "sleep 30".to_string()],
                Duration::from_millis(250),
            )
            .await
            .unwrap();
        assert!(out.engine_cancelled);
        assert_eq!(out.exit_code, None);
    }
}
