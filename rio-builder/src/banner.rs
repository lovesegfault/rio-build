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
    let fmt_gib = |b: Option<u64>| {
        b.map(|n| (n / (1 << 30)).to_string())
            .unwrap_or_else(|| "?".into())
    };
    let cores_str = cores.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
    let now = format_rfc3339_secs(std::time::SystemTime::now());
    vec![
        format!("rio: exec     {exec_id}").into_bytes(),
        format!(
            "rio: builder  {builder} ({cores_str}c, {} GiB, {} GiB)",
            fmt_gib(mem_bytes),
            fmt_gib(disk_bytes),
        )
        .into_bytes(),
        format!("rio: started  {now}").into_bytes(),
    ]
}

/// Render the footer lines written after the build process exits.
///
/// Format:
///
/// ```text
/// rio: exec     01976e8b-1234-7890-abcd-ef0123456789
/// rio: result   failed (PermanentFailure) after 4m23s
/// ```
///
/// `result` is one of `ok`, `failed (<reason>)`, or `cancelled`.
/// `footer_result_str` (in `crate::executor`) maps the per-attempt
/// daemon outcome to `ok`/`failed (<reason>)` — see its doc for why
/// exit codes aren't available — and `runtime::result::final_footer_result`
/// overrides to `cancelled` from the assignment's cancel flag
/// (best-effort: dropped by the scheduler's cancel-path seal before it
/// reaches the stored log). The `exec` line repeats so a truncated
/// tail (e.g. Nix's "last 10 lines" failure summary) still includes
/// the identifier without scrolling back to the header.
pub(crate) fn footer_lines(exec_id: &str, result: &str, duration: Duration) -> Vec<Vec<u8>> {
    vec![
        format!("rio: exec     {exec_id}").into_bytes(),
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
        );
        assert!(
            str::from_utf8(&lines[1])
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
        let lines = footer_lines("01976e8b-test", "ok", Duration::from_secs(45));
        assert!(str::from_utf8(&lines[1]).unwrap().contains("ok after 45s"));
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
