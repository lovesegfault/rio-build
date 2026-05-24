//! `xtask k8s replay` — replay a recorded build-load archive against a rio
//! deployment at the recorded cadence and compare outcomes against what the
//! recording says was built.
//!
//! See `docs/dev/2026-05-24-xtask-k8s-replay-design.md` for the design and
//! the archive-format compatibility contract. The replay engine lands in
//! sibling modules over the next tasks; this module owns the CLI surface and
//! orchestration.

use std::path::PathBuf;

use crate::config::XtaskConfig;
use crate::k8s::provider::{Provider, ProviderKind};

// The archive reader lands ahead of its consumers — the supply/prewarm/
// timeline modules wire it into `run` next; the allow goes away with the
// first real caller.
#[allow(dead_code)]
mod archive;
// The SSH transport + daemon-channel pool also lands ahead of its consumers —
// the prewarm/timeline modules open channels per request; the allow goes away
// with them.
#[allow(dead_code)]
mod client;
// Substituter access (narinfo probe + NAR fetch over HTTPS/S3) also lands
// ahead of its consumers — the supply planner and prewarm phases are the
// first callers; the allow goes away with them.
#[allow(dead_code)]
mod substituter;
// The supply planner (workload set, closure walk, source ladder, upload
// ordering, cross-request upload claims) also lands ahead of its consumers —
// the prewarm and timeline modules drive it; the allow goes away with them.
#[allow(dead_code)]
mod supply;

/// Exit-code policy for a replay run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FailOn {
    /// Always exit 0 (unless the run itself errored).
    None,
    /// Exit nonzero if any regression, upload rejection, or request error occurred.
    Regression,
    /// Exit nonzero if any divergence at all occurred.
    Divergence,
}

#[derive(Debug, clap::Args)]
pub struct ReplayArgs {
    /// Path to the replay archive: a `.dwarfs` image or an unpacked archive
    /// directory.
    #[arg(long)]
    pub archive: PathBuf,

    /// Time-compression factor (> 0). 2.0 replays a 1-hour window in 30 min.
    #[arg(long, default_value_t = 1.0)]
    pub speedup: f64,

    /// Maximum concurrent in-flight requests (one SSH channel + daemon
    /// session each).
    #[arg(long, default_value_t = 32)]
    pub max_sessions: usize,

    /// SSH connections to spread channels over. Default: ceil(max_sessions/4)
    /// (the gateway allows 4 concurrent channels per connection).
    #[arg(long)]
    pub connections: Option<usize>,

    /// Substituters the target can reach on its own; paths covered by any of
    /// these are not uploaded. Repeatable.
    #[arg(long = "target-substituter", default_values_t = vec!["https://cache.nixos.org".to_string()])]
    pub target_substituters: Vec<String>,

    /// Consecutive failed rebuilds required before a recorded-success build
    /// failure is reported as a regression.
    #[arg(long, default_value_t = 3)]
    pub confirm_regressions: u32,

    /// Skip the bulk pre-supply phase; dependencies are then uploaded
    /// per-request inside the timeline (lower timing fidelity).
    #[arg(long)]
    pub no_prewarm: bool,

    /// Do not replay recorded client disconnects; wait for those builds to
    /// finish instead.
    #[arg(long)]
    pub no_disconnect_replay: bool,

    /// Resolve everything and run the timeline without connecting to any
    /// cluster or network.
    #[arg(long)]
    pub dry_run: bool,

    /// Replay only the first N requests (by recorded offset).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Print a scheduler-metrics line every 30s during the run.
    #[arg(long)]
    pub watch: bool,

    /// Bypass the provider tunnel and connect to this `ssh-ng://host:port`
    /// endpoint instead.
    #[arg(long)]
    pub store: Option<String>,

    /// SSH private key for `--store` targets (default: the deploy key).
    #[arg(long)]
    pub ssh_key: Option<PathBuf>,

    /// Pinned host key (path to a public-key file or a `SHA256:…`
    /// fingerprint) for non-loopback `--store` targets.
    #[arg(long)]
    pub ssh_host_key: Option<String>,

    /// Exit-code policy.
    #[arg(long, value_enum, default_value_t = FailOn::None)]
    pub fail_on: FailOn,

    /// Directory for run artifacts (log, divergences.jsonl, summary.json).
    /// Default: `.stress-test/replay/<unix-ts>/`
    #[arg(long)]
    pub report_dir: Option<PathBuf>,
}

pub async fn run(
    args: ReplayArgs,
    _provider: &dyn Provider,
    _kind: ProviderKind,
    _cfg: &XtaskConfig,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.speedup > 0.0 && args.speedup.is_finite(),
        "--speedup must be a positive number"
    );
    anyhow::ensure!(
        args.ssh_host_key.is_none() || args.store.is_some(),
        "--ssh-host-key only makes sense together with --store"
    );
    anyhow::bail!(
        "replay: not implemented yet (archive={})",
        args.archive.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: ReplayArgs,
    }

    #[test]
    fn defaults_parse() {
        let h = Harness::parse_from(["x", "--archive", "/tmp/a"]);
        assert_eq!(h.args.speedup, 1.0);
        assert_eq!(h.args.max_sessions, 32);
        assert!(h.args.connections.is_none());
        assert_eq!(h.args.target_substituters, vec!["https://cache.nixos.org"]);
        assert_eq!(h.args.confirm_regressions, 3);
        assert_eq!(h.args.fail_on, FailOn::None);
        assert!(!h.args.no_prewarm);
        assert!(!h.args.dry_run);
        assert!(h.args.limit.is_none());
    }

    #[test]
    fn fail_on_values_parse() {
        for (s, v) in [
            ("none", FailOn::None),
            ("regression", FailOn::Regression),
            ("divergence", FailOn::Divergence),
        ] {
            let h = Harness::parse_from(["x", "--archive", "/tmp/a", "--fail-on", s]);
            assert_eq!(h.args.fail_on, v);
        }
    }

    /// Documents how `tests/fixtures/replay/basic/` was produced and keeps it
    /// honest against the production parsers:
    ///
    /// - NAR-serializes the embedded source store path and prints the
    ///   `NarHash`/`NarSize` values that `narinfo/<hash>.narinfo` must carry
    ///   (`sha256:<nixbase32>` — the encoding `NarInfo` stores verbatim).
    /// - Parses all four fixture `.drv` files with the rio-nix ATerm parser.
    /// - Parses the committed narinfo and asserts it matches the recomputed
    ///   hash/size.
    ///
    /// Run with:
    /// `cargo nextest run -p xtask --run-ignored all -E 'test(fixture)'`
    #[test]
    #[ignore = "fixture generator"]
    fn fixture_archive_matches_rio_nix_parsers() {
        use sha2::{Digest, Sha256};

        let basic =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay/basic");

        // Embedded source store path → NAR hash/size for the narinfo.
        let src = basic.join("nix/store/b1111111111111111111111111111111-src.txt");
        let mut nar = Vec::new();
        let nar_size =
            rio_nix::nar::dump_path_streaming(&src, &mut nar).expect("NAR-serialize fixture src");
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let nar_hash = format!("sha256:{}", rio_nix::store_path::nixbase32::encode(&digest));
        println!("NarHash: {nar_hash}");
        println!("NarSize: {nar_size}");

        // The fixture .drv files must parse with the production parser.
        for drv in [
            "a1111111111111111111111111111111-dep.drv",
            "a2222222222222222222222222222222-app.drv",
            "a3333333333333333333333333333333-impure.drv",
            "a4444444444444444444444444444444-cached.drv",
        ] {
            let text = std::fs::read_to_string(basic.join("nix/store").join(drv))
                .unwrap_or_else(|e| panic!("read {drv}: {e}"));
            rio_nix::derivation::Derivation::parse(&text)
                .unwrap_or_else(|e| panic!("{drv} must parse: {e}"));
        }

        // The committed narinfo must parse and carry the real hash/size.
        let narinfo_text =
            std::fs::read_to_string(basic.join("narinfo/b1111111111111111111111111111111.narinfo"))
                .expect("read fixture narinfo");
        let narinfo =
            rio_nix::narinfo::NarInfo::parse(&narinfo_text).expect("fixture narinfo must parse");
        assert_eq!(narinfo.nar_hash, nar_hash);
        assert_eq!(narinfo.nar_size, nar_size);
    }
}
