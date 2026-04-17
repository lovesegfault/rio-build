//! `RIO_BUILDER_SCRIPT` test fixture (feature `test-fixtures`).
//!
//! When the env var points at a TOML file, [`lookup`] returns scripted
//! `(wall, peak_mem, peak_cpu, cpu_seconds)` for a `(pname, cpu_limit)`
//! key. `execute_build` short-circuits the daemon lifecycle and reports
//! these as if a real build ran — letting the SLA-sizing VM scenario
//! drive the explore ladder deterministically without booting nix-daemon
//! or waiting wall-clock minutes per probe.
//!
//! Fixture format:
//! ```toml
//! [[entry]]
//! pname = "synth-amdahl"
//! cpu_limit = 4
//! wall_secs = 530
//! peak_cpu = 3.8
//! cpu_seconds = 2012
//! peak_mem = 2147483648
//! ```
//!
//! No fallback: an unmatched `(pname, cpu_limit)` returns `None` and the
//! real build path runs. This is intentional — a fixture entry typo
//! shows up as a real (slow) build in the VM test, which is loud.

use std::time::{Duration, SystemTime};

use rio_proto::types::{BuildResult as ProtoBuildResult, BuildResultStatus, ResourceUsage};
use serde::Deserialize;

use crate::executor::ExecutionResult;

#[derive(Debug, Deserialize)]
struct Script {
    #[serde(default)]
    entry: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    pname: String,
    cpu_limit: u32,
    wall_secs: f64,
    peak_cpu: f64,
    cpu_seconds: f64,
    peak_mem: u64,
    #[serde(default)]
    peak_disk: Option<u64>,
}

/// Scripted resource outcome for one `(pname, cpu_limit)` probe.
pub struct ScriptedOutcome {
    pub wall_secs: f64,
    pub peak_mem: u64,
    pub peak_cpu: f64,
    pub cpu_seconds: f64,
    pub peak_disk: Option<u64>,
}

/// Look up `(pname, cpu_limit)` in `$RIO_BUILDER_SCRIPT`. Re-reads the
/// file every call — fixture builds are rare and tiny; caching would
/// add a `OnceLock` for no measurable gain.
pub fn lookup(pname: &str, cpu_limit: u32) -> Option<ScriptedOutcome> {
    let path = std::env::var("RIO_BUILDER_SCRIPT").ok()?;
    let body = std::fs::read_to_string(&path)
        .inspect_err(|e| tracing::warn!(%path, %e, "RIO_BUILDER_SCRIPT unreadable"))
        .ok()?;
    let script: Script = toml::from_str(&body)
        .inspect_err(|e| tracing::warn!(%path, %e, "RIO_BUILDER_SCRIPT malformed"))
        .ok()?;
    script
        .entry
        .into_iter()
        .find(|e| e.pname == pname && e.cpu_limit == cpu_limit)
        .map(|e| ScriptedOutcome {
            wall_secs: e.wall_secs,
            peak_mem: e.peak_mem,
            peak_cpu: e.peak_cpu,
            cpu_seconds: e.cpu_seconds,
            peak_disk: e.peak_disk,
        })
}

/// Build a synthetic [`ExecutionResult`] from a scripted outcome.
/// `start_time/stop_time` are `now-wall_secs .. now` so the scheduler's
/// `result.duration()` recovers `wall_secs` exactly. `built_outputs`
/// stays empty: the SLA fit reads `build_samples` (telemetry only),
/// and an empty output map means downstream drvs in the same DAG would
/// fail — fixture scenarios use single-drv submits.
pub fn scripted_result(
    drv_path: &str,
    assignment_token: &str,
    cpu_limit: u32,
    o: ScriptedOutcome,
) -> ExecutionResult {
    let stop = SystemTime::now();
    let start = stop - Duration::from_secs_f64(o.wall_secs);
    ExecutionResult {
        drv_path: drv_path.to_string(),
        result: ProtoBuildResult {
            status: BuildResultStatus::Built as i32,
            error_msg: String::new(),
            built_outputs: Vec::new(),
            start_time: Some(start.into()),
            stop_time: Some(stop.into()),
        },
        assignment_token: assignment_token.to_string(),
        peak_memory_bytes: o.peak_mem,
        peak_cpu_cores: o.peak_cpu,
        fixture_resources: Some(ResourceUsage {
            cpu_limit_cores: Some(f64::from(cpu_limit)),
            cpu_seconds_total: Some(o.cpu_seconds),
            peak_disk_bytes: o.peak_disk,
            memory_used_bytes: o.peak_mem,
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script.toml");
        std::fs::write(
            &p,
            r#"
[[entry]]
pname = "synth-amdahl"
cpu_limit = 4
wall_secs = 530
peak_cpu = 3.8
cpu_seconds = 2012
peak_mem = 2147483648
"#,
        )
        .unwrap();
        // SAFETY: nextest runs each test in its own process; no other
        // thread mutates env in this binary's test harness.
        unsafe { std::env::set_var("RIO_BUILDER_SCRIPT", &p) };
        let o = lookup("synth-amdahl", 4).expect("matched");
        assert_eq!(o.wall_secs, 530.0);
        assert_eq!(o.peak_mem, 2147483648);
        assert!(lookup("synth-amdahl", 8).is_none(), "cpu_limit mismatch");
        assert!(lookup("other", 4).is_none());
        unsafe { std::env::remove_var("RIO_BUILDER_SCRIPT") };
    }

    #[test]
    fn scripted_result_duration_roundtrips() {
        let r = scripted_result(
            "/nix/store/x.drv",
            "tok",
            4,
            ScriptedOutcome {
                wall_secs: 530.0,
                peak_mem: 1 << 30,
                peak_cpu: 3.8,
                cpu_seconds: 2012.0,
                peak_disk: None,
            },
        );
        let start: SystemTime = r.result.start_time.unwrap().try_into().unwrap();
        let stop: SystemTime = r.result.stop_time.unwrap().try_into().unwrap();
        let d = stop.duration_since(start).unwrap().as_secs_f64();
        assert!((d - 530.0).abs() < 0.01, "duration={d}");
        assert_eq!(r.fixture_resources.unwrap().cpu_limit_cores, Some(4.0));
    }
}
