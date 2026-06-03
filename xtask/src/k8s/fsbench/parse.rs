//! PHASE/PERF line parser for the fsbench build log.
//!
//! The log is `nix build -L` output: every line the bench binary
//! prints arrives prefixed `fsbench-run-<seed>> ` and interleaved with
//! nix's own progress chatter (`copying path …`, `building …`).
//! Parsing is tolerant — unrecognized lines are skipped, a PHASE start
//! without its end is dropped — because the failure mode to avoid is
//! "nix changed its log framing and the whole run is unreadable"; the
//! raw-JSON twin in `$out` remains the recovery path.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};

/// One `PERF <name> k=v …` line.
#[derive(Debug, Clone)]
pub struct PerfLine {
    pub name: String,
    pub kv: BTreeMap<String, String>,
}

impl PerfLine {
    pub fn f64(&self, key: &str) -> Option<f64> {
        self.kv.get(key)?.parse().ok()
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        self.kv.get(key).map(String::as_str)
    }
}

/// A paired `PHASE <name> start/end` window.
#[derive(Debug, Clone)]
pub struct PhaseWindow {
    pub name: String,
    pub rep: u32,
    pub start_epoch_ms: u64,
    pub end_epoch_ms: u64,
}

#[derive(Debug, Default)]
pub struct ParsedRun {
    /// The `PERF meta …` keys (seed, dataset_bytes, kernel, …).
    pub meta: BTreeMap<String, String>,
    /// All other PERF lines, in emission order.
    pub perf: Vec<PerfLine>,
    pub phases: Vec<PhaseWindow>,
    /// The bench drv's `echo "FSBENCH seed=…"` banner — present even
    /// if the binary crashed before its meta line.
    pub echoed_seed: Option<String>,
}

impl ParsedRun {
    /// PERF lines for `name`, in rep order (emission order).
    pub fn perf_named(&self, name: &str) -> Vec<&PerfLine> {
        self.perf.iter().filter(|p| p.name == name).collect()
    }

    pub fn window(&self, name: &str, rep: u32) -> Option<&PhaseWindow> {
        self.phases.iter().find(|w| w.name == name && w.rep == rep)
    }
}

/// Strip everything up to and including the `nix -L` per-drv prefix.
/// The prefix is `<drv-name>> ` — but a token can also legitimately
/// start the line (local runs, captured fixtures), so: take the
/// substring from the LAST `"> "` before the first marker token, or
/// the whole line.
fn payload(line: &str) -> &str {
    for marker in ["PHASE ", "PERF ", "FSBENCH "] {
        if let Some(idx) = line.find(marker) {
            // Only accept at line start or right after the nix `> `
            // prefix — an arbitrary mid-sentence "PERF " (e.g. quoted
            // in an error message) stays unparsed.
            if idx == 0 || line[..idx].ends_with("> ") {
                return &line[idx..];
            }
        }
    }
    line
}

pub fn parse_log(text: &str) -> ParsedRun {
    let mut run = ParsedRun::default();
    let mut open: BTreeMap<(String, u32), u64> = BTreeMap::new();
    for raw in text.lines() {
        let line = payload(raw);
        if let Some(rest) = line.strip_prefix("FSBENCH seed=") {
            run.echoed_seed = Some(rest.split_whitespace().next().unwrap_or("").to_string());
        } else if let Some(rest) = line.strip_prefix("PHASE ") {
            let mut it = rest.split_whitespace();
            let (Some(name), Some(edge)) = (it.next(), it.next()) else {
                continue;
            };
            let kv: BTreeMap<&str, &str> = it.filter_map(|t| t.split_once('=')).collect();
            let Some(ms) = kv.get("epoch_ms").and_then(|v| v.parse().ok()) else {
                continue;
            };
            let rep: u32 = kv.get("rep").and_then(|v| v.parse().ok()).unwrap_or(1);
            match edge {
                "start" => {
                    open.insert((name.to_string(), rep), ms);
                }
                "end" => {
                    if let Some(start) = open.remove(&(name.to_string(), rep)) {
                        run.phases.push(PhaseWindow {
                            name: name.to_string(),
                            rep,
                            start_epoch_ms: start,
                            end_epoch_ms: ms,
                        });
                    }
                }
                _ => {}
            }
        } else if let Some(rest) = line.strip_prefix("PERF ") {
            let mut it = rest.split_whitespace();
            let Some(name) = it.next() else { continue };
            let kv: BTreeMap<String, String> = it
                .filter_map(|t| t.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if name == "meta" {
                run.meta = kv;
            } else {
                run.perf.push(PerfLine {
                    name: name.to_string(),
                    kv,
                });
            }
        }
    }
    run
}

/// The structural floor a run must clear before any number is quoted.
/// UNSEEDED is the load-bearing check: a seedless submission means
/// FSBENCH_SEED never reached the eval — the run drv's nonce is then
/// almost certainly STATIC too, so what built may be a stale reused
/// drv rather than this submission's benchmark.
pub fn validate(run: &ParsedRun) -> Result<()> {
    let seed = run
        .meta
        .get("seed")
        .map(String::as_str)
        .or(run.echoed_seed.as_deref());
    match seed {
        None => bail!("no seed in log (no PERF meta line, no FSBENCH banner) — build crashed?"),
        Some("UNSEEDED") => bail!(
            "run was submitted UNSEEDED (FSBENCH_SEED not threaded through eval) — \
             refusing to parse this into a result"
        ),
        Some(_) => {}
    }
    for required in [
        "read_storm_cold",
        "read_storm_warm",
        "jq_build",
        "open_storm",
        "randread",
        "copy_to_local",
        "read_storm_local",
        "read_storm_local_warm",
    ] {
        ensure!(
            run.perf.iter().any(|p| p.name == required),
            "no PERF {required} line — phase missing or build died mid-run"
        );
    }
    ensure!(
        run.window("read_storm_cold", 1).is_some(),
        "read_storm_cold has no paired PHASE window — metric-delta attribution impossible"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured shape of a real `nix build -L` log: drv prefix,
    /// interleaved nix chatter, the FSBENCH banner from bench.nix.
    const FIXTURE: &str = "\
these 3 derivations will be built:
fsbench-run-ab12cd> FSBENCH seed=ab12cd
copying path '/nix/store/xxx-python3-3.12.8' from 'ssh-ng://rio@localhost:39907'...
fsbench-run-ab12cd> PERF meta seed=ab12cd dataset_bytes=2483027968 unique_chunk_bytes=1900000000 unique_chunk_bytes_storm=1500000000 dataset_digest=1f2e3d files=4354 kernel=6.12.20 fsbench_rev=1 workload_version=1 jq_src=hash-jq-1.7.1.tar.gz toolchain=hash-gcc-wrapper-14
fsbench-run-ab12cd> PHASE read_storm_cold start epoch_ms=1000 rep=1
fsbench-run-ab12cd> PHASE read_storm_cold end epoch_ms=61000 rep=1
fsbench-run-ab12cd> PERF read_storm_cold rep=1 files=4353 bytes=1409286144 wall_ms=60000 mib_s=22.4 open_ns_p50=812000 open_ns_p99=92000000 open_ns_max=210000000 read_ns_p50=400000 read_ns_p99=1200000 checksum_ok=4353
fsbench-run-ab12cd> PHASE read_storm_warm start epoch_ms=61001 rep=1
fsbench-run-ab12cd> PHASE read_storm_warm end epoch_ms=62001 rep=1
fsbench-run-ab12cd> PERF read_storm_warm rep=1 files=4353 bytes=1409286144 wall_ms=1000 mib_s=1344.0 open_ns_p50=9000 open_ns_p99=41000 read_ns_p50=210000 read_ns_p99=600000
fsbench-run-ab12cd> PHASE jq_build_cold start epoch_ms=62002 rep=1
fsbench-run-ab12cd> PHASE jq_build_cold end epoch_ms=92002 rep=1
fsbench-run-ab12cd> PERF jq_build state=cold rep=1 configure_wall_ms=9000 make_wall_ms=21000 total_wall_ms=30000
fsbench-run-ab12cd> PHASE jq_build_warm start epoch_ms=92003 rep=2
fsbench-run-ab12cd> PHASE jq_build_warm end epoch_ms=110003 rep=2
fsbench-run-ab12cd> PERF jq_build state=warm rep=2 configure_wall_ms=5000 make_wall_ms=13000 total_wall_ms=18000
fsbench-run-ab12cd> PERF randread target=castore state=cold rep=1 direct=0 ios=65536 io_ns_p50=480000 io_ns_p99=9000000 io_ns_p999=30000000 io_ns_max=90000000 iops=2048 mib_s=8.0
fsbench-run-ab12cd> PERF open_storm pass=2 cache_state=warm files=7421 open_ns_p50=8000 open_ns_p99=30000 open_ns_max=2000000 fstat_ns_p50=900
fsbench-run-ab12cd> PERF copy_to_local bytes=2483027968 wall_ms=2100 mib_s=1127.0
fsbench-run-ab12cd> PERF read_storm_local rep=1 files=4353 bytes=1409286144 wall_ms=900 mib_s=1493.0 open_ns_p50=4000 open_ns_p99=12000 read_ns_p50=180000 read_ns_p99=410000
fsbench-run-ab12cd> PERF read_storm_local_warm rep=1 files=4353 bytes=1409286144 wall_ms=880 mib_s=1527.0 open_ns_p50=4000 open_ns_p99=11000 read_ns_p50=175000 read_ns_p99=400000
building '/nix/store/yyy.drv'...
";

    #[test]
    fn parses_prefixed_lines_and_pairs_windows() {
        let run = parse_log(FIXTURE);
        assert_eq!(run.echoed_seed.as_deref(), Some("ab12cd"));
        assert_eq!(run.meta.get("seed").map(String::as_str), Some("ab12cd"));
        assert_eq!(run.meta.get("files").map(String::as_str), Some("4354"));

        let cold = run.perf_named("read_storm_cold");
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].f64("mib_s"), Some(22.4));
        assert_eq!(cold[0].f64("checksum_ok"), Some(4353.0));

        let w = run.window("read_storm_cold", 1).unwrap();
        assert_eq!((w.start_epoch_ms, w.end_epoch_ms), (1000, 61000));

        let rr = run.perf_named("randread");
        assert_eq!(rr[0].str("target"), Some("castore"));
        assert_eq!(rr[0].str("state"), Some("cold"));

        // jq_build compile phases: one line per state, wall splits
        // present, windows paired under the per-state phase names.
        let jq = run.perf_named("jq_build");
        assert_eq!(jq.len(), 2);
        assert_eq!(jq[0].str("state"), Some("cold"));
        assert_eq!(jq[0].f64("configure_wall_ms"), Some(9000.0));
        assert_eq!(jq[0].f64("total_wall_ms"), Some(30000.0));
        assert_eq!(jq[1].str("state"), Some("warm"));
        assert_eq!(jq[1].f64("make_wall_ms"), Some(13000.0));
        assert!(run.window("jq_build_cold", 1).is_some());
        assert!(run.window("jq_build_warm", 2).is_some());

        // v2 identity keys ride the meta line.
        assert_eq!(
            run.meta.get("dataset_digest").map(String::as_str),
            Some("1f2e3d")
        );
        assert_eq!(
            run.meta.get("unique_chunk_bytes_storm").map(String::as_str),
            Some("1500000000")
        );
        assert_eq!(
            run.meta.get("toolchain").map(String::as_str),
            Some("hash-gcc-wrapper-14")
        );
    }

    #[test]
    fn unpaired_phase_start_is_dropped_not_fatal() {
        let run = parse_log(
            "x> PHASE read_storm_cold start epoch_ms=5 rep=1\n\
             x> PERF read_storm_cold rep=1 files=1 bytes=1 wall_ms=1 mib_s=1.0",
        );
        // The PERF line survives; the half-open window does not.
        assert_eq!(run.perf_named("read_storm_cold").len(), 1);
        assert!(run.window("read_storm_cold", 1).is_none());
    }

    #[test]
    fn mid_sentence_marker_is_not_parsed() {
        // nix error text quoting "PERF " must not become a metric.
        let run = parse_log("error: builder failed: expected PERF meta line but got EOF");
        assert!(run.meta.is_empty());
        assert!(run.perf.is_empty());
    }

    #[test]
    fn validate_refuses_unseeded() {
        let run =
            parse_log("x> PERF meta seed=UNSEEDED dataset_bytes=1 files=1 kernel=k fsbench_rev=1");
        let err = validate(&run).unwrap_err().to_string();
        assert!(err.contains("UNSEEDED"), "got: {err}");
    }

    #[test]
    fn validate_passes_full_fixture_and_catches_missing_phase() {
        let run = parse_log(FIXTURE);
        validate(&run).unwrap();

        // Drop randread from the fixture → validate names the gap.
        let trimmed: String = FIXTURE
            .lines()
            .filter(|l| !l.contains("PERF randread"))
            .map(|l| format!("{l}\n"))
            .collect();
        let err = validate(&parse_log(&trimmed)).unwrap_err().to_string();
        assert!(err.contains("randread"), "got: {err}");
    }
}
