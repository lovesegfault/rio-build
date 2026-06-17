//! Worker log banner: a `rio:`-prefixed header at the start of every
//! build log, and a footer after the build process exits.
//!
//! These lines are **display-only**. They are written by the worker
//! into the same untrusted byte stream as build stdout/stderr —
//! arbitrary build code can emit its own `rio: result   ok` lines.
//! Consumers MUST NOT parse the `rio:` markers for authoritative
//! state; the system's source of truth for `exec_id` / outcome /
//! sizing is `drv_executions` and `assignments`, not the log text. The
//! `grep '^rio:'` extraction is a convenience for humans, not a
//! protocol.
//!
//! Pod name, node name, and executor ID are deliberately excluded —
//! the "cluster is one machine" abstraction holds at the log level
//! too (same reasoning as the gateway's `actBuild.machineName`
//! decision). `hw_class` and the resource triple answer the actual
//! debugging question ("why was my build slow") without leaking
//! ephemeral pod identity to anyone reading the stored log.
//!
//! r[impl obs.log.worker-header]

use std::time::Duration;

/// Number of lines [`header_lines`] writes. Used to seed
/// `LogBatcher::new`'s `initial_line` so the build's real output
/// numbering follows the header instead of colliding at line 0, and
/// as the footer's line-number fallback when the build process never
/// produced output (early daemon-setup failure).
pub(crate) const HEADER_LINE_COUNT: u64 = 3;

/// Render the header lines written before the build process starts.
///
/// Format (key-value lines, fixed-width key column, lowercase keys, no
/// trailing colon — `rio: ` is the marker):
///
/// ```text
/// rio: exec     01976e8b-1234-7890-abcd-ef0123456789
/// rio: builder  x86_64-linux/large (16c, 32 GiB, 100 GiB)
/// rio: started  2026-05-19T10:00:00Z
/// ```
///
/// Absent-field rendering:
/// - `hw_class` absent (non-k8s, annotator timeout, read error, bench
///   still running at first assignment) → drop the `/{hw_class}`
///   suffix.
/// - Sizing fields absent (`WorkAssignment.assigned_*` not set —
///   pre-ADR-023 path) → `?` for the missing component. Do NOT fall
///   back to cgroup limits — the cgroup clamp `ceil()`s fractional
///   limits and would print a different number than the SLA model
///   fitted, which is the exact noise `assigned_cores` exists to
///   remove. The build *runtime* still falls back to the cgroup
///   clamp; the banner just doesn't claim a precision it doesn't
///   have.
pub(crate) fn header_lines(
    exec_id: &str,
    system: &str,
    hw_class: Option<&str>,
    cores: Option<u32>,
    mem_bytes: Option<u64>,
    disk_bytes: Option<u64>,
) -> Vec<Vec<u8>> {
    let builder = match hw_class {
        Some(hw) => format!("{system}/{hw}"),
        None => system.to_string(),
    };
    let cores_str = cores.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
    let now = format_rfc3339_secs(std::time::SystemTime::now());
    vec![
        format!("rio: exec     {exec_id}").into_bytes(),
        format!(
            "rio: builder  {builder} ({cores_str}c, {}, {})",
            fmt_size(mem_bytes),
            fmt_size(disk_bytes),
        )
        .into_bytes(),
        format!("rio: started  {now}").into_bytes(),
    ]
}

/// Size formatter shared by the header's `(Nc, mem, disk)` triple and
/// the footer's `rio: peaks` line.
///
/// live_058-d + merged_bug_004: the law is the VIOLATION CLASS, total
/// over the input domain — a present NON-ZERO size never renders as
/// zero at ANY unit — quantifier: census(present_nonzero_never_renders_zero_at_any_unit) — (the live incident's 45-69 MB
/// raw-stamps read as "no memory assigned" during diagnosis; the
/// header doc above forbids claiming a precision the banner doesn't
/// have, and rounding a present value to zero is the inverse violation
/// — at every rung, not just the GiB one the incident happened to
/// hit). The unit ladder descends to the first rung that preserves a
/// non-zero magnitude and bottoms out at bytes; absent stays "? GiB".
/// Pinned by the `present_nonzero_never_renders_zero_at_any_unit`
/// property.
fn fmt_size(b: Option<u64>) -> String {
    match b {
        None => "? GiB".to_string(),
        Some(n) if n >= 1 << 30 => format!("{} GiB", n >> 30),
        Some(n) if n >= 1 << 20 => format!("{} MiB", n >> 20),
        Some(n) if n >= 1 << 10 => format!("{} KiB", n >> 10),
        Some(n) => format!("{n} B"),
    }
}

/// Resource peaks for the footer's `rio: peaks` line. The same fields
/// the worker reports in `CompletionReport` (cgroup `memory.peak`,
/// 1Hz-polled `cpu.stat` peak-cores, prjquota `dqb_curspace` max),
/// rendered into the log so a human reading the tail sees the build's
/// own sizing answer without joining against `drv_executions`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FooterPeaks {
    /// cgroup `memory.peak` (kernel-tracked tree-wide lifetime max).
    pub mem_bytes: u64,
    /// Peak instantaneous CPU (cores-equivalent), 1Hz-polled.
    pub cpu_cores: f64,
    /// prjquota `dqb_curspace` running max. `None` = no prjquota.
    pub disk_bytes: Option<u64>,
    /// Cumulative `cpu.stat usage_usec` (seconds). `None` = pre-cgroup
    /// error or read failure.
    pub cpu_seconds_total: Option<f64>,
    /// `WorkAssignment.assigned_cores` — the SLA-fitted denominator
    /// for `cpu_util`. `None` (or `Some(0)`) = pre-ADR-023 path: the
    /// banner doesn't fall back to the cgroup clamp for the same
    /// reason the header doesn't (the clamp `ceil()`s and would
    /// disagree with the SLA model). `cpu_util` is omitted then.
    pub assigned_cores: Option<u32>,
}

impl FooterPeaks {
    /// `cpu_seconds_total / (wall × assigned_cores)` — the same
    /// formula the scheduler's compute-bound corroborator uses
    /// (`rio_scheduler::actor::floor::corroborated_compute_bound`).
    /// `None` when any input is absent or `wall`/`cores` are zero —
    /// the absent-field discipline applies (don't claim a precision
    /// we don't have).
    fn cpu_util(&self, wall: Duration) -> Option<f64> {
        let cpu = self.cpu_seconds_total?;
        let cores = self.assigned_cores.filter(|&c| c > 0)?;
        let wall = wall.as_secs_f64();
        (wall > 0.0).then(|| cpu / (wall * f64::from(cores)))
    }
}

/// Render the footer lines written after the build process exits.
///
/// Format:
///
/// ```text
/// rio: exec     01976e8b-1234-7890-abcd-ef0123456789
/// rio: peaks    cpu=3.8c mem=2 GiB disk=12 GiB wall=263s
/// rio: result   failed (PermanentFailure) after 4m23s
/// ```
///
/// `result` is one of `ok`, `failed (<reason>)`, or `cancelled
/// (sigterm)`. `footer_result_str` (in `crate::executor`) maps the
/// per-attempt daemon outcome to `ok`/`failed (<reason>)` — see its
/// doc for why exit codes aren't available — and
/// `runtime::result::final_footer_result` overrides to `cancelled
/// (sigterm)` from the assignment's cancel flag (best-effort: dropped
/// by the scheduler's cancel-path seal before it reaches the stored
/// log). The `exec` line repeats so a truncated tail (e.g. Nix's
/// "last 10 lines" failure summary) still includes the identifier
/// without scrolling back to the header. The `peaks` line precedes
/// `result` so the same truncated tail keeps the sizing answer too;
/// footer length flows through `sealed_final_line_count` which is
/// already `footer.len()`-driven.
pub(crate) fn footer_lines(
    exec_id: &str,
    result: &str,
    duration: Duration,
    peaks: FooterPeaks,
) -> Vec<Vec<u8>> {
    let util = peaks
        .cpu_util(duration)
        .map(|u| format!(" cpu_util={:.0}%", u * 100.0))
        .unwrap_or_default();
    vec![
        format!("rio: exec     {exec_id}").into_bytes(),
        format!(
            "rio: peaks    cpu={:.1}c mem={} disk={} wall={}s{util}",
            peaks.cpu_cores,
            fmt_size(Some(peaks.mem_bytes)),
            fmt_size(peaks.disk_bytes),
            duration.as_secs(),
        )
        .into_bytes(),
        format!("rio: result   {result} after {}", format_duration(duration)).into_bytes(),
    ]
}

/// Format a duration as `45s` / `4m23s` / `1h02m03s`. Hand-rolled —
/// no `humantime`/`chrono` dep in `rio-builder` and a 6-line `match`
/// is cheaper than a new dep for a display-only string.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m{s:02}s"),
        _ => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// Format a `SystemTime` as RFC 3339 with second precision, UTC.
///
/// Reuses `prost_types::Timestamp`'s `Display` impl (already a
/// direct dep — used for `BuildResult.start_time/stop_time`). Its
/// civil-time conversion is musl's `__secs_to_tm` translated to Rust
/// — a battle-tested reference, not a hand-rolled Gregorian calendar.
/// Nanos are zeroed so the output is `YYYY-MM-DDTHH:MM:SSZ` (the
/// `Display` impl appends sub-second precision only when nanos > 0).
fn format_rfc3339_secs(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        // Pre-epoch SystemTime is unrepresentable on the platforms
        // this runs on (Linux); 0 (1970) is a defensible never-taken
        // fallback that's still valid RFC 3339.
        .unwrap_or(0);
    prost_types::Timestamp {
        seconds: secs,
        nanos: 0,
    }
    .to_string()
}

// r[verify obs.log.worker-header]
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn header_renders_with_all_fields() {
        let lines = header_lines(
            "01976e8b-test",
            "x86_64-linux",
            Some("large"),
            Some(16),
            Some(32 << 30),
            Some(100 << 30),
        );
        assert_eq!(lines.len() as u64, HEADER_LINE_COUNT);
        assert!(
            str::from_utf8(&lines[0])
                .unwrap()
                .starts_with("rio: exec     01976e8b-test")
        );
        assert!(
            str::from_utf8(&lines[1])
                .unwrap()
                .contains("rio: builder  x86_64-linux/large (16c, 32 GiB, 100 GiB)")
        );
        assert!(
            str::from_utf8(&lines[2])
                .unwrap()
                .starts_with("rio: started  ")
        );
    }

    /// W10-CL (live_058-d): the three-cell matrix — sub-GiB renders
    /// value-preserving in MiB; >=1 GiB and absent are pinned
    /// unchanged. During live_058 the wedged pods' banners read
    /// "(2c, 0 GiB, ...)" for a 69 MiB assignment and the zero was
    /// read live as "no memory assigned", costing diagnosis time;
    /// the file's own header doc forbids claiming a precision it
    /// doesn't have — rendering 69 MiB as 0 GiB is the inverse
    /// violation.
    #[test]
    fn header_renders_sub_gib_sizes_in_mib() {
        // Cell 1 (the live banner, verbatim shape): 69 MiB mem, 2c.
        let lines = header_lines(
            "01976e8b-live058",
            "x86_64-linux",
            None,
            Some(2),
            Some(69 << 20),
            Some(25 << 30),
        );
        let builder_line = str::from_utf8(&lines[1]).unwrap();
        assert!(
            !builder_line.contains("0 GiB"),
            "left: the live banner rendered '(2c, 0 GiB, …)' for a 69 MiB \
             assignment (integer division) / right: sub-GiB renders in \
             MiB: {builder_line}"
        );
        assert!(
            builder_line.contains("(2c, 69 MiB, 25 GiB)"),
            "sub-GiB must render value-preserving in MiB: {builder_line}"
        );
        // Cell 2: >=1 GiB pinned unchanged (covered in depth by
        // header_renders_with_all_fields; re-pinned here so the
        // matrix is one test).
        let lines = header_lines(
            "x",
            "x86_64-linux",
            None,
            Some(16),
            Some(32 << 30),
            Some(100 << 30),
        );
        assert!(
            str::from_utf8(&lines[1])
                .unwrap()
                .contains("(16c, 32 GiB, 100 GiB)")
        );
        // Cell 3: absent pinned unchanged ('? GiB').
        let lines = header_lines("x", "x86_64-linux", None, None, None, None);
        assert!(
            str::from_utf8(&lines[1])
                .unwrap()
                .contains("(?c, ? GiB, ? GiB)")
        );
    }

    /// W11-Y (merged_bug_004): the formatter's law is the VIOLATION
    /// CLASS, total over the input domain — a present non-zero size
    /// NEVER renders as zero at ANY unit — quantifier: census(present_nonzero_never_renders_zero_at_any_unit). The live_058-d fix closed
    /// the truncation-to-zero at the GiB rung and the W10-CL matrix
    /// pinned three cells; the fallthrough still rendered `n >> 20`,
    /// so any present value in [1, 1 MiB) printed "0 MiB" — the
    /// identical shape one granularity down (and "0 KiB" would be
    /// one below that). The population that reaches this banner is
    /// by definition the mis-scaled anomalous one the fix exists to
    /// stay honest for.
    #[test]
    fn present_nonzero_never_renders_zero_at_any_unit() {
        use proptest::prelude::*;
        // The example red (pre-fix, verbatim in the owning commit):
        // 512 KiB rendered "0 MiB".
        let lines = header_lines(
            "01976e8b-w11y",
            "x86_64-linux",
            None,
            Some(2),
            Some(512 << 10),
            Some(25 << 30),
        );
        let builder_line = str::from_utf8(&lines[1]).unwrap();
        assert!(
            !builder_line.contains("0 MiB"),
            "left: a present 512 KiB stamp renders '0 MiB' — the \
             live_058-d truncation-to-zero relocated one rung down / \
             right: the smallest rung floors, never zero: {builder_line}"
        );
        assert!(
            builder_line.contains("(2c, 512 KiB, 25 GiB)"),
            "sub-MiB renders value-preserving in KiB: {builder_line}"
        );
        // Sub-KiB falls to bytes.
        let lines = header_lines("x", "x86_64-linux", None, Some(1), Some(37), Some(1));
        let builder_line = str::from_utf8(&lines[1]).unwrap();
        assert!(
            builder_line.contains("(1c, 37 B, 1 B)"),
            "sub-KiB renders in bytes: {builder_line}"
        );

        // The property at the formatter's own domain quantifier:
        // nonzero in ⇒ never `0 <unit>` out, domain-wide.
        proptest!(|(n in 1u64..=u64::MAX)| {
            let lines = header_lines("x", "s", None, None, Some(n), None);
            let line = String::from_utf8(lines[1].clone()).unwrap();
            // The mem token is the second comma-field inside the
            // parens — token-exact so "12260528520 GiB" (which merely
            // ENDS in a 0) never false-trips the zero check.
            let inner = line
                .split('(')
                .nth(1)
                .and_then(|t| t.split(')').next())
                .unwrap_or("");
            let mem_token = inner.split(", ").nth(1).unwrap_or("");
            prop_assert!(
                !mem_token.starts_with("0 "),
                "present non-zero {} rendered as zero: {}", n, line
            );
        });
    }

    #[test]
    fn header_renders_with_absent_fields() {
        let lines = header_lines("01976e8b-test", "x86_64-linux", None, None, None, None);
        let builder_line = str::from_utf8(&lines[1]).unwrap();
        // No hw_class → no slash.
        assert!(
            !builder_line.contains('/'),
            "hw_class absent should drop the / suffix: {builder_line}"
        );
        assert!(
            builder_line.contains("(?c, ? GiB, ? GiB)"),
            "absent sizing should render '?', not 0: {builder_line}"
        );
    }

    #[test]
    fn footer_renders_failed() {
        // Fixture mirrors the real domain: footer_result_str produces
        // `failed (<reason>)`, never `failed (exit N)` — BuildStatus has
        // no exit code.
        let lines = footer_lines(
            "01976e8b-test",
            "failed (PermanentFailure)",
            Duration::from_secs(263),
            FooterPeaks::default(),
        );
        assert!(
            str::from_utf8(&lines[2])
                .unwrap()
                .contains("rio: result   failed (PermanentFailure) after 4m23s")
        );
        // Footer repeats the exec line so a truncated tail still has it.
        assert!(
            str::from_utf8(&lines[0])
                .unwrap()
                .starts_with("rio: exec     01976e8b-test")
        );
    }

    #[test]
    fn footer_renders_ok() {
        let lines = footer_lines(
            "01976e8b-test",
            "ok",
            Duration::from_secs(45),
            FooterPeaks::default(),
        );
        assert!(str::from_utf8(&lines[2]).unwrap().contains("ok after 45s"));
    }

    /// sh-038 Tier 1: the footer carries the build's own sizing answer
    /// (cgroup `memory.peak`, peak-cores, prjquota peak, wall) so a
    /// human reading Nix's "last 10 lines" failure summary sees it
    /// without joining against `drv_executions`. Display-only — the
    /// module doc's "consumers MUST NOT parse" applies.
    #[test]
    fn footer_renders_peaks_line() {
        let lines = footer_lines(
            "01976e8b-test",
            "ok",
            Duration::from_secs(263),
            FooterPeaks {
                mem_bytes: 2 << 30,
                cpu_cores: 3.84,
                disk_bytes: Some(12 << 30),
                cpu_seconds_total: None,
                assigned_cores: None,
            },
        );
        assert_eq!(lines.len(), 3, "exec + peaks + result");
        let peaks = str::from_utf8(&lines[1]).unwrap();
        assert!(
            peaks.starts_with("rio: peaks    "),
            "footer must contain a `rio: peaks` line: {peaks}"
        );
        assert!(peaks.contains("cpu=3.8c"), "peaks: {peaks}");
        assert!(peaks.contains("mem=2 GiB"), "peaks: {peaks}");
        assert!(peaks.contains("disk=12 GiB"), "peaks: {peaks}");
        assert!(peaks.contains("wall=263s"), "peaks: {peaks}");
        // Absent disk renders the same `?` sentinel as the header.
        let lines = footer_lines(
            "x",
            "ok",
            Duration::from_secs(1),
            FooterPeaks {
                mem_bytes: 69 << 20,
                cpu_cores: 0.5,
                disk_bytes: None,
                cpu_seconds_total: None,
                assigned_cores: None,
            },
        );
        let peaks = str::from_utf8(&lines[1]).unwrap();
        assert!(
            peaks.contains("mem=69 MiB") && peaks.contains("disk=? GiB"),
            "fmt_size law applies to the footer too: {peaks}"
        );
        assert!(
            !peaks.contains("cpu_util"),
            "cpu_util omitted when inputs absent: {peaks}"
        );
    }

    /// sh-038 Tier 2: `cpu_util = cpu_seconds_total / (wall ×
    /// assigned_cores)` — the same formula the scheduler's
    /// compute-bound corroborator uses (`floor.rs
    /// corroborated_compute_bound`). Omitted when any input is absent
    /// or zero — the banner doesn't claim a precision it doesn't have.
    #[test]
    fn footer_peaks_line_carries_cpu_util() {
        // 4 cores × 263s wall = 1052 cpu-seconds available; 999.4
        // consumed → 95%.
        let lines = footer_lines(
            "x",
            "ok",
            Duration::from_secs(263),
            FooterPeaks {
                mem_bytes: 0,
                cpu_cores: 3.8,
                disk_bytes: None,
                cpu_seconds_total: Some(999.4),
                assigned_cores: Some(4),
            },
        );
        let peaks = str::from_utf8(&lines[1]).unwrap();
        assert!(
            peaks.contains("cpu_util=95%"),
            "peaks line must carry cpu_util when cpu_seconds_total and \
             assigned_cores are both present: {peaks}"
        );
        // Absent-field discipline: assigned_cores=0 (proto3 unset) →
        // omit, not divide-by-zero / `inf%`.
        let p = FooterPeaks {
            cpu_seconds_total: Some(10.0),
            assigned_cores: Some(0),
            ..FooterPeaks::default()
        };
        assert_eq!(p.cpu_util(Duration::from_secs(10)), None);
        // wall=0 → omit.
        let p = FooterPeaks {
            cpu_seconds_total: Some(10.0),
            assigned_cores: Some(4),
            ..FooterPeaks::default()
        };
        assert_eq!(p.cpu_util(Duration::ZERO), None);
    }

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_duration(Duration::from_secs(263)), "4m23s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h00m00s");
        assert_eq!(format_duration(Duration::from_secs(3723)), "1h02m03s");
    }

    #[test]
    fn format_rfc3339_secs_format() {
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s = format_rfc3339_secs(t);
        // prost_types::Timestamp Display gives "YYYY-MM-DDTHH:MM:SSZ"
        // when nanos == 0 (no fractional seconds).
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }
}
