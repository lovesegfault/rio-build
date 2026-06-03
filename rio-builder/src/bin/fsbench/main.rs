//! fsbench — castore-FUSE micro-benchmark (P0594).
//!
//! Runs AS A BUILD on the cluster: the bench derivation gets its
//! `/nix/store` through the production mount path (scheduler-assigned
//! build, mountd fd-handover, overlay assembly — zero new privileges)
//! and this binary measures cold/warm read, open, and randread
//! behavior against a same-pod `$TMPDIR` local baseline.
//!
//! Output contract (parsed by `xtask k8s fsbench`):
//! * `PHASE <name> start|end epoch_ms=… rep=…` markers — phase windows
//!   for metric-delta attribution and perf-trace slicing;
//! * `PERF <name> k=v …` lines — the measurement source of truth;
//! * `--out FILE` — the same data plus raw sample arrays as JSON
//!   (lands in `$out`, recoverable via `nix copy --from` if log
//!   parsing ever degrades).
//!
//! The dataset is a re-rooted real closure (see `dataset.rs`); "cold"
//! means the bench node's local cache is empty, which the honesty gate
//! verifies against the manifest's unique-chunk byte counts rather
//! than assumes.
//!
//! NOT production code (same status as `spike_mountd_client` /
//! `xfstests_runner`).

mod dataset;
mod phases;
mod stats;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use dataset::{FileEntry, Manifest};
use phases::{RANDREAD_IO_BYTES, RANDREAD_IOS};
use stats::Summary;

/// Bumped when phase semantics change (what a phase measures, rep
/// counts, IO sizes). Distinct from `dataset::WORKLOAD_VERSION`, which
/// tracks the dataset construction only.
const FSBENCH_REV: u32 = 1;

/// Warm phases repeat to expose run-to-run noise (`rep_spread`
/// downstream). Cold phases are single-rep by construction — their
/// uniqueness is consumed by the first read.
const WARM_REPS: u32 = 3;

#[derive(Parser)]
#[command(
    name = "fsbench",
    about = "castore-FUSE micro-benchmark (NOT production code)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Re-root the harvest roots into the dataset (workload v2 — see
    /// dataset.rs for why there are no composition knobs).
    Gen {
        /// Layout seed; file contents come from the harvest.
        #[arg(long)]
        seed: String,
        /// Harvest roots (the dataset drv passes a pinned nixpkgs
        /// package, e.g. ghc).
        #[arg(long, required = true)]
        harvest: Vec<PathBuf>,
        /// Dataset root to create (the dataset drv passes `$out`).
        #[arg(long)]
        out: PathBuf,
    },
    /// Run the fixed phase list against a generated dataset.
    Run {
        /// Dataset root (a store path on the castore mount).
        #[arg(long)]
        dataset: PathBuf,
        /// A real closure for the open-storm walk (python3 by
        /// default in bench.nix).
        #[arg(long)]
        closure: PathBuf,
        /// Same-pod scratch dir for the local baseline (`$TMPDIR`).
        #[arg(long)]
        scratch: PathBuf,
        /// jq source tarball for the jq_build compile phases (the
        /// bench drv passes the pinned `jq.src`); absent → phases
        /// skipped (unit tests / fixture mode).
        #[arg(long)]
        jq_src: Option<PathBuf>,
        /// Where to write the raw-JSON twin of the PERF stream.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Gen { seed, harvest, out } => {
            let m = dataset::generate(&seed, &harvest, &out)?;
            println!(
                "fsbench dataset: {} files, {} bytes ({} unique chunk bytes), seed={}",
                m.files.len(),
                m.total_bytes,
                m.unique_chunk_bytes,
                m.seed
            );
            Ok(())
        }
        Cmd::Run {
            dataset,
            closure,
            scratch,
            jq_src,
            out,
        } => run(
            &dataset,
            &closure,
            &scratch,
            jq_src.as_deref(),
            &out,
            RANDREAD_IOS,
        ),
    }
}

/// `randread_ios` is a function parameter, not a CLI knob — the
/// no-knobs rule protects the compare-keyed CLI surface; the CLI
/// always passes the production [`RANDREAD_IOS`]. The parameter exists
/// for the phase-coverage unit test: at the production count the full
/// run is ~half a million psync IOs across the randread phases, which
/// is fine on tmpfs (where O_DIRECT is unsupported and silently falls
/// back to page-cache reads) but real disk traffic in the nix build
/// sandbox — enough to blow the 90s nextest timeout on a loaded
/// builder.
fn run(
    dataset_root: &Path,
    closure: &Path,
    scratch: &Path,
    jq_src: Option<&Path>,
    out: &Path,
    randread_ios: u64,
) -> Result<()> {
    let manifest = Manifest::load(dataset_root)?;
    let mut rec = Recorder::default();

    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut meta = vec![
        ("seed", manifest.seed.clone()),
        ("dataset_bytes", manifest.total_bytes.to_string()),
        ("files", manifest.files.len().to_string()),
        ("kernel", kernel.clone()),
        ("fsbench_rev", FSBENCH_REV.to_string()),
        // The workload version: xtask refuses to assemble a result
        // whose version it doesn't know.
        ("workload_version", manifest.workload_version.to_string()),
        // Identity + honesty references (see dataset.rs).
        ("dataset_digest", manifest.dataset_digest.clone()),
        (
            "unique_chunk_bytes",
            manifest.unique_chunk_bytes.to_string(),
        ),
        (
            "unique_chunk_bytes_storm",
            manifest.unique_chunk_bytes_storm.to_string(),
        ),
    ];
    if let Some(jq) = jq_src {
        // jq source + toolchain identity: a bump to either makes
        // compile-phase numbers incomparable, so both join the
        // baseline identity key. Store-path basenames carry the nix
        // hash.
        meta.push(("jq_src", store_basename(jq)));
        meta.push((
            "toolchain",
            resolve_cc().unwrap_or_else(|| "unknown".into()),
        ));
    }
    rec.perf("meta", &meta);

    let storm_files = manifest.read_storm_files();
    let reserve = manifest
        .randread_reserve()
        .context("manifest reserve path missing from files — corrupt manifest")?;
    // Deterministic per run-seed so identical seeds replay identical
    // offset sequences.
    let prng_seed = u64::from_le_bytes(
        blake3::hash(manifest.seed.as_bytes()).as_bytes()[..8]
            .try_into()
            .expect("blake3 output is 32 bytes"),
    );

    // 1. read_storm_cold — every byte a guaranteed miss (fetch +
    //    Promote). The cold open-RTT distribution comes from here.
    {
        let ph = rec.begin("read_storm_cold", 1);
        let o = phases::read_storm(dataset_root, &storm_files, true)?;
        rec.end_read_storm(ph, "read_storm_cold", o, true);
    }

    // 2. read_storm_warm — passthrough + page cache.
    for rep in 1..=WARM_REPS {
        let ph = rec.begin("read_storm_warm", rep);
        let o = phases::read_storm(dataset_root, &storm_files, false)?;
        rec.end_read_storm(ph, "read_storm_warm", o, false);
    }

    // 3. jq_build — a real compiler workload through the mount: many
    //    small toolchain reads, header opens, cc/ld execs; writes go
    //    to scratch only. Placed here, BEFORE open_storm, because the
    //    toolchain shares closure pieces (glibc) with the open-storm
    //    walk — running it first keeps the cold rep genuinely cold
    //    (the read storms above touch only the dataset). Skipped when
    //    no jq source was provided (unit tests / fixture mode).
    if let Some(jq) = jq_src {
        for (rep, state) in [(1u32, "cold"), (2, "warm")] {
            let name: &'static str = if rep == 1 {
                "jq_build_cold"
            } else {
                "jq_build_warm"
            };
            let ph = rec.begin(name, rep);
            let o = phases::jq_build(jq, scratch, rep)?;
            let total = o.configure_wall_ms + o.make_wall_ms;
            rec.finish(
                ph,
                Some("jq_build"),
                vec![
                    ("state".into(), state.to_string()),
                    ("rep".into(), rep.to_string()),
                    ("configure_wall_ms".into(), o.configure_wall_ms.to_string()),
                    ("make_wall_ms".into(), o.make_wall_ms.to_string()),
                    ("total_wall_ms".into(), total.to_string()),
                ],
                BTreeMap::new(),
            );
        }
    }

    // 4. open_storm — two passes over a real closure. Pass 1 is
    //    recorded but tagged cache_state=unknown: shared content may
    //    be node-warm from prior activity; no cold claims are made.
    for pass in 1..=2u32 {
        let ph = rec.begin("open_storm", pass);
        let o = phases::open_storm(closure)?;
        rec.end_open_storm(ph, pass, o);
    }

    // 5. randread — cold pass against the reserved (never-opened)
    //    large file races the streaming background fill on purpose:
    //    read-during-fill IS production cold behavior for streamed
    //    files. Then complete the fill sequentially, then warm reps.
    let reserve_path = dataset_root.join(&reserve.path);
    {
        let ph = rec.begin("randread_cold", 1);
        let o = phases::randread(&reserve_path, reserve.bytes, randread_ios, prng_seed)?;
        rec.end_randread(ph, "castore", "cold", 1, o);
    }
    {
        let ph = rec.begin("randread_fill", 1);
        let bytes = phases::sequential_read(&reserve_path)?;
        rec.finish(
            ph,
            None,
            vec![("bytes".into(), bytes.to_string())],
            BTreeMap::new(),
        );
    }
    for rep in 1..=WARM_REPS {
        let ph = rec.begin("randread_warm", rep);
        let o = phases::randread(
            &reserve_path,
            reserve.bytes,
            randread_ios,
            prng_seed.wrapping_add(u64::from(rep)),
        )?;
        rec.end_randread(ph, "castore", "warm", rep, o);
    }

    // 6. local_baseline — same pod, same node disk: the speed-of-light
    //    reference all slowdown ratios are computed against.
    let local_root = scratch.join("fsbench-local");
    let all_files: Vec<&FileEntry> = manifest.files.iter().collect();
    {
        let ph = rec.begin("copy_to_local", 1);
        let o = phases::copy_tree(dataset_root, &all_files, &local_root)?;
        let mib_s = mib_s(o.bytes, o.wall_ms);
        rec.finish(
            ph,
            Some("copy_to_local"),
            vec![
                ("bytes".into(), o.bytes.to_string()),
                ("wall_ms".into(), o.wall_ms.to_string()),
                ("mib_s".into(), format!("{mib_s:.1}")),
            ],
            BTreeMap::new(),
        );
    }
    {
        let ph = rec.begin("read_storm_local", 1);
        let o = phases::read_storm(&local_root, &storm_files, false)?;
        rec.end_read_storm(ph, "read_storm_local", o, false);
    }
    for rep in 1..=WARM_REPS {
        let ph = rec.begin("read_storm_local_warm", rep);
        let o = phases::read_storm(&local_root, &storm_files, false)?;
        rec.end_read_storm(ph, "read_storm_local_warm", o, false);
    }
    let local_reserve = local_root.join(&reserve.path);
    for rep in 1..=WARM_REPS {
        let ph = rec.begin("randread_local_warm", rep);
        let o = phases::randread(
            &local_reserve,
            reserve.bytes,
            randread_ios,
            prng_seed.wrapping_add(u64::from(rep)),
        )?;
        rec.end_randread(ph, "local", "warm", rep, o);
    }

    rec.write_raw(out, &manifest, &kernel)?;
    Ok(())
}

/// `/nix/store/<hash>-<name>` → `<hash>-<name>` (the basename carries
/// the nix hash, which is the identity we want). Store paths are
/// ASCII; a non-UTF-8 path falls back to the lossless Display form.
fn store_basename(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| p.display().to_string())
}

/// Identity of the toolchain serving the jq build: the resolved store
/// path basename of `cc` on PATH. None when there is no cc (fixture
/// mode never gets here — jq phases are gated on --jq-src).
fn resolve_cc() -> Option<String> {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v cc"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let real = std::fs::canonicalize(path.trim()).ok()?;
    Some(store_basename(&real))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("post-1970")
        .as_millis() as u64
}

fn mib_s(bytes: u64, wall_ms: u64) -> f64 {
    if wall_ms == 0 {
        return 0.0;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / (wall_ms as f64 / 1000.0)
}

/// One in-flight phase: the `begin` half of the marker pair.
struct PhaseHandle {
    name: &'static str,
    rep: u32,
    start_epoch_ms: u64,
}

#[derive(Default)]
struct Recorder {
    phases: Vec<RawPhase>,
}

#[derive(serde::Serialize)]
struct RawPhase {
    name: String,
    rep: u32,
    start_epoch_ms: u64,
    end_epoch_ms: u64,
    /// Echo of the PERF line tokens (if the phase emitted one).
    keys: BTreeMap<String, String>,
    /// Raw per-sample nanosecond arrays, for offline re-analysis.
    samples: BTreeMap<String, Vec<u64>>,
}

#[derive(serde::Serialize)]
struct RawResult<'a> {
    schema: &'static str,
    seed: &'a str,
    workload_version: u32,
    fsbench_rev: u32,
    kernel: &'a str,
    dataset_bytes: u64,
    phases: Vec<RawPhase>,
}

impl Recorder {
    /// `PERF <name> k=v …` — the parser's source of truth. Values must
    /// not contain whitespace (everything emitted here is numeric, a
    /// hex digest, or a kernel release string).
    fn perf(&mut self, name: &str, kvs: &[(&str, String)]) {
        let line = kvs
            .iter()
            .map(|(k, v)| {
                debug_assert!(
                    !v.chars().any(char::is_whitespace),
                    "PERF value with whitespace breaks the k=v stream: {k}={v}"
                );
                format!("{k}={v}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        println!("PERF {name} {line}");
    }

    fn begin(&mut self, name: &'static str, rep: u32) -> PhaseHandle {
        let start_epoch_ms = epoch_ms();
        println!("PHASE {name} start epoch_ms={start_epoch_ms} rep={rep}");
        PhaseHandle {
            name,
            rep,
            start_epoch_ms,
        }
    }

    /// Close a phase: end marker, optional PERF line, raw record.
    fn finish(
        &mut self,
        ph: PhaseHandle,
        perf_name: Option<&str>,
        kvs: Vec<(String, String)>,
        samples: BTreeMap<String, Vec<u64>>,
    ) {
        let end = epoch_ms();
        println!("PHASE {} end epoch_ms={end} rep={}", ph.name, ph.rep);
        if let Some(pn) = perf_name {
            let borrowed: Vec<(&str, String)> =
                kvs.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            self.perf(pn, &borrowed);
        }
        self.phases.push(RawPhase {
            name: ph.name.to_string(),
            rep: ph.rep,
            start_epoch_ms: ph.start_epoch_ms,
            end_epoch_ms: end,
            keys: kvs.into_iter().collect(),
            samples,
        });
    }

    fn end_read_storm(
        &mut self,
        ph: PhaseHandle,
        perf_name: &str,
        mut o: phases::ReadStormOut,
        cold: bool,
    ) {
        let open = stats::summarize(&mut o.open_ns);
        let read = stats::summarize(&mut o.read_ns);
        let mut kvs = vec![
            ("rep".into(), ph.rep.to_string()),
            ("files".into(), o.files.to_string()),
            ("bytes".into(), o.bytes.to_string()),
            ("wall_ms".into(), o.wall_ms.to_string()),
            ("mib_s".into(), format!("{:.1}", mib_s(o.bytes, o.wall_ms))),
        ];
        push_summary(&mut kvs, "open_ns", &open, cold);
        push_summary(&mut kvs, "read_ns", &read, false);
        if cold {
            kvs.push(("checksum_ok".into(), o.checksum_ok.to_string()));
        }
        let samples = BTreeMap::from([
            ("open_ns".to_string(), o.open_ns),
            ("read_ns".to_string(), o.read_ns),
        ]);
        self.finish(ph, Some(perf_name), kvs, samples);
    }

    fn end_open_storm(&mut self, ph: PhaseHandle, pass: u32, mut o: phases::OpenStormOut) {
        let open = stats::summarize(&mut o.open_ns);
        let fstat = stats::summarize(&mut o.fstat_ns);
        // Pass 1 walks shared content that may be node-warm from prior
        // activity — recorded, never quoted as cold.
        let cache_state = if pass == 1 { "unknown" } else { "warm" };
        let mut kvs = vec![
            ("pass".into(), pass.to_string()),
            ("cache_state".into(), cache_state.into()),
            ("files".into(), o.files.to_string()),
        ];
        push_summary(&mut kvs, "open_ns", &open, true);
        kvs.push(("fstat_ns_p50".into(), fstat.p50.to_string()));
        let samples = BTreeMap::from([
            ("open_ns".to_string(), o.open_ns),
            ("fstat_ns".to_string(), o.fstat_ns),
        ]);
        self.finish(ph, Some("open_storm"), kvs, samples);
    }

    fn end_randread(
        &mut self,
        ph: PhaseHandle,
        target: &str,
        state: &str,
        rep: u32,
        mut o: phases::RandreadOut,
    ) {
        let io = stats::summarize(&mut o.io_ns);
        let iops = if o.wall_ms == 0 {
            0.0
        } else {
            o.ios as f64 / (o.wall_ms as f64 / 1000.0)
        };
        let mut kvs = vec![
            ("target".into(), target.to_string()),
            ("state".into(), state.to_string()),
            ("rep".into(), rep.to_string()),
            ("direct".into(), if o.direct { "1" } else { "0" }.into()),
            ("ios".into(), o.ios.to_string()),
            ("io_ns_p50".into(), io.p50.to_string()),
        ];
        if let Some(p99) = io.p99 {
            kvs.push(("io_ns_p99".into(), p99.to_string()));
        }
        if let Some(p999) = io.p999 {
            kvs.push(("io_ns_p999".into(), p999.to_string()));
        }
        kvs.push(("io_ns_max".into(), io.max.to_string()));
        kvs.push(("iops".into(), format!("{iops:.0}")));
        kvs.push((
            "mib_s".into(),
            format!("{:.1}", mib_s(o.ios * RANDREAD_IO_BYTES, o.wall_ms)),
        ));
        let samples = BTreeMap::from([("io_ns".to_string(), o.io_ns)]);
        self.finish(ph, Some("randread"), kvs, samples);
    }

    fn write_raw(self, out: &Path, manifest: &Manifest, kernel: &str) -> Result<()> {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = RawResult {
            schema: "fsbench-raw/v1",
            seed: &manifest.seed,
            workload_version: manifest.workload_version,
            fsbench_rev: FSBENCH_REV,
            kernel,
            dataset_bytes: manifest.total_bytes,
            phases: self.phases,
        };
        std::fs::write(out, serde_json::to_vec_pretty(&raw)?)
            .with_context(|| format!("write raw result {}", out.display()))?;
        Ok(())
    }
}

/// Append `<prefix>_p50/p99[/max]` keys; p99 only above its floor
/// (the absent key tells the parser the floor wasn't met — explicit
/// absence beats a fake zero). `with_max` adds the max for cold/open
/// distributions where the single worst open is itself interesting.
fn push_summary(kvs: &mut Vec<(String, String)>, prefix: &str, s: &Summary, with_max: bool) {
    kvs.push((format!("{prefix}_p50"), s.p50.to_string()));
    if let Some(p99) = s.p99 {
        kvs.push((format!("{prefix}_p99"), p99.to_string()));
    }
    if with_max {
        kvs.push((format!("{prefix}_max"), s.max.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end on a tiny tree: every PERF/PHASE consumer downstream
    /// (xtask parse.rs) keys off this binary's stdout contract, so the
    /// raw-JSON twin must carry the same phases the markers announced.
    #[test]
    fn run_emits_all_phases_into_raw_json() {
        let src = tempfile::tempdir().unwrap();
        dataset::test_fixture_tree(src.path());
        let data = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let outf = scratch.path().join("raw.json");
        dataset::generate("seed-t", &[src.path().to_path_buf()], data.path()).unwrap();
        // Closure stand-in: the dataset tree itself is a real file
        // tree. No --jq-src (the compile phases are live-only — they
        // need a real toolchain). 512 IOs, not RANDREAD_IOS: this test
        // asserts phase COVERAGE, not performance — at the production
        // count it does ~500k real psync IOs in the nix build sandbox
        // (the dev shell hides them: $TMPDIR is tmpfs, where O_DIRECT
        // is unsupported and falls back to page cache) and times out
        // nextest.
        run(data.path(), data.path(), scratch.path(), None, &outf, 512).unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&outf).unwrap()).unwrap();
        assert_eq!(raw["schema"], "fsbench-raw/v1");
        let names: Vec<&str> = raw["phases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        for required in [
            "read_storm_cold",
            "read_storm_warm",
            "open_storm",
            "randread_cold",
            "randread_fill",
            "randread_warm",
            "copy_to_local",
            "read_storm_local",
            "read_storm_local_warm",
            "randread_local_warm",
        ] {
            assert!(names.contains(&required), "missing phase {required}");
        }
        // No --jq-src → the compile phases are skipped, not failed.
        assert!(!names.contains(&"jq_build_cold"));
        assert!(!names.contains(&"jq_build_warm"));
        // Warm phases carry WARM_REPS reps each.
        assert_eq!(
            names.iter().filter(|n| **n == "read_storm_warm").count(),
            WARM_REPS as usize
        );
        // Phase windows are well-formed (perf slicing relies on this).
        for p in raw["phases"].as_array().unwrap() {
            assert!(p["end_epoch_ms"].as_u64() >= p["start_epoch_ms"].as_u64());
        }
    }
}
