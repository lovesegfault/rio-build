//! Parsing of the `nix build -L` child's stderr: the gateway's
//! `rio: build <uuid>` line and the relayed per-derivation failure
//! lines, plus the classification of relayed scheduler reasons
//! (infra vs. target vs. dependency) and the deterministic failure
//! signatures used to group identical failures.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// `rio: build <uuid>` — emitted once per accepted build by the gateway
/// (`rio-gateway/src/handler/build.rs`). The optional ` (trace <hex>)`
/// suffix and any nix progress-bar prefix are tolerated by searching
/// anywhere in the line.
static BUILD_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"rio: build ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})")
        .expect("static regex")
});

/// `derivation '<drv>' failed: <reason>` — the gateway's relay of one
/// scheduler terminal failure (`rio-gateway/src/handler/build.rs`). The
/// `↳ rio-cli logs '<drv>'` hint arrives on the FOLLOWING line and is
/// ignored.
static DRV_FAILED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"derivation '([^']+\.drv)' failed: (.*)$").expect("static regex"));

/// Everything the engine extracts from one batch's stderr.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedStderr {
    pub build_id: Option<String>,
    /// drv path → reason (first occurrence wins; the scheduler emits one
    /// terminal Failed per drv per build).
    pub reasons: BTreeMap<String, String>,
}

/// Feed one stderr line into `parsed` (streaming form, used by the live
/// child-stderr reader).
pub fn parse_line(parsed: &mut ParsedStderr, line: &str) {
    if parsed.build_id.is_none()
        && let Some(c) = BUILD_ID_RE.captures(line)
    {
        parsed.build_id = Some(c[1].to_string());
    }
    if let Some(c) = DRV_FAILED_RE.captures(line) {
        parsed
            .reasons
            .entry(c[1].to_string())
            .or_insert_with(|| c[2].trim().to_string());
    }
}

/// Parse a whole captured stderr text (resume / post-mortem form).
pub fn parse_stderr(text: &str) -> ParsedStderr {
    let mut parsed = ParsedStderr::default();
    for line in text.lines() {
        parse_line(&mut parsed, line);
    }
    parsed
}

/// Classification of one relayed failure reason: the scheduler-reason
/// signal that failure attribution combines with the scheduler's
/// poison/builder evidence (the second signal) before counting a failure
/// against rio. The reason strings are the scheduler's terminal-failure
/// messages (`rio-scheduler/src/actor/{completion,executor,dispatch,recovery}.rs`)
/// relayed verbatim by the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasonClass {
    /// `max_infra_retries=… exhausted after infrastructure failures` /
    /// `max_exempt_infra_retries=… exceeded` — positively infra.
    Infra,
    /// `max_timeout_retries=… exhausted` — counted against rio, own signature.
    Timeout,
    /// `max_infra_retries=… exhausted at resource ceiling` — counted against
    /// rio, own signature.
    ResourceCeiling,
    /// `poison threshold…` / `failed on every eligible worker` /
    /// `max_retries=… exhausted after transient failures` / any unprefixed
    /// worker message — genuine target failure (the safe default).
    Target,
    /// `dependency '<drv>' failed: …` — failed-dependency rooted at `<drv>`.
    Dependency { failing_drv: String },
}

static DEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^dependency '([^']+)' failed: ").expect("static regex"));

/// Classify one relayed reason line into its [`ReasonClass`]. Anything
/// unrecognized falls back to [`ReasonClass::Target`]: charging rio with a
/// failure is the safe default, never the other way around.
pub fn classify_reason(reason: &str) -> ReasonClass {
    let r = reason.trim();
    if let Some(c) = DEP_RE.captures(r) {
        return ReasonClass::Dependency {
            failing_drv: c[1].to_string(),
        };
    }
    if r.starts_with("max_infra_retries=") {
        if r.contains("exhausted at resource ceiling") {
            return ReasonClass::ResourceCeiling;
        }
        if r.contains("exhausted after infrastructure failures") {
            return ReasonClass::Infra;
        }
        // Unknown max_infra_retries variant: only the two known suffixes are
        // positively classified — default to the conservative target class.
        return ReasonClass::Target;
    }
    if r.starts_with("max_exempt_infra_retries=") && r.contains("exceeded") {
        return ReasonClass::Infra;
    }
    if r.starts_with("max_timeout_retries=") && r.contains("exhausted") {
        return ReasonClass::Timeout;
    }
    // poison threshold / every-eligible-worker / max_retries transient /
    // unprefixed worker messages → target.
    ReasonClass::Target
}

/// Failure signature: a deterministic short string for grouping identical
/// failures across jobs, derived from the relayed reason (preferred) or the
/// captured log tail (fallback). Signatures are raw-evidence-derived — they
/// make no attempt to explain a failure, only to collapse repeats of the
/// same one.
pub fn signature_for(reason: Option<&str>, log_tail: Option<&str>) -> Option<String> {
    if let Some(r) = reason {
        let class = classify_reason(r);
        let sig = match class {
            ReasonClass::Infra => "infra-retries-exhausted".to_string(),
            ReasonClass::Timeout => "timeout-retries-exhausted".to_string(),
            ReasonClass::ResourceCeiling => "resource-ceiling".to_string(),
            ReasonClass::Dependency { .. } => "dependency-failed".to_string(),
            ReasonClass::Target => {
                if r.starts_with("poison threshold") {
                    "poison-threshold".to_string()
                } else if r.starts_with("failed on every eligible worker") {
                    "failed-every-worker".to_string()
                } else if r.starts_with("max_retries=") {
                    "transient-retries-exhausted".to_string()
                } else {
                    // Unprefixed worker permanent failure: first 60 chars, normalized.
                    let mut s: String = r
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() {
                                c.to_ascii_lowercase()
                            } else {
                                '-'
                            }
                        })
                        .collect();
                    s.truncate(60);
                    format!("worker:{}", s.trim_matches('-'))
                }
            }
        };
        return Some(sig);
    }
    log_tail.map(|t| {
        let line = t
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        let mut s: String = line
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        s.truncate(60);
        format!("log:{}", s.trim_matches('-'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured-shape fixture: what a `nix build -L --store ssh-ng://…` run
    /// against rio-gateway prints for a two-job batch where one drv fails on
    /// the worker and a dependent cascades. Format strings verified against
    /// rio-gateway/src/handler/build.rs and rio-scheduler/src/actor/*.rs.
    const STDERR_FIXTURE: &str = concat!(
        "this derivation will be built:\n",
        "  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv\n",
        "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a (trace 4bf92f3577b34da6a3ce929d0e0e4736)\n",
        "libfoo> building\n",
        "derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: poison threshold reached after 3 distinct-worker failures\n",
        "  \u{21b3} rio-cli logs '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv'\n",
        "derivation '/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv' failed: dependency '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: poison threshold reached after 3 distinct-worker failures\n",
        "error: build of '/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv^*' failed\n",
    );

    #[test]
    fn parses_build_id_and_failure_reasons() {
        let parsed = parse_stderr(STDERR_FIXTURE);
        assert_eq!(
            parsed.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(parsed.reasons.len(), 2);
        assert_eq!(
            parsed.reasons["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv"],
            "poison threshold reached after 3 distinct-worker failures"
        );
        assert!(
            parsed.reasons["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv"].starts_with(
                "dependency '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed:"
            )
        );
    }

    #[test]
    fn build_id_without_trace_suffix_and_with_progress_prefix() {
        let mut p = ParsedStderr::default();
        parse_line(&mut p, "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a");
        assert!(p.build_id.is_some());
        let mut p2 = ParsedStderr::default();
        parse_line(
            &mut p2,
            "[1/0/2 built] rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a (trace ab)",
        );
        assert!(p2.build_id.is_some());
        // Lines that merely mention builds don't match.
        let mut p3 = ParsedStderr::default();
        parse_line(&mut p3, "rio: build of something else");
        assert!(p3.build_id.is_none());
    }

    /// Every scheduler terminal-failure reason string maps to the class the
    /// comparison model assigns it (strings from completion.rs/executor.rs/
    /// dispatch.rs/recovery.rs).
    #[test]
    fn reason_classification_covers_all_scheduler_strings() {
        use ReasonClass::*;
        let cases: Vec<(&str, ReasonClass)> = vec![
            (
                "max_infra_retries=3 exhausted after infrastructure failures: store unavailable",
                Infra,
            ),
            (
                "max_exempt_infra_retries=10 exceeded: concurrent PutPath in progress",
                Infra,
            ),
            (
                "max_infra_retries=3 exhausted at resource ceiling (OomKilled)",
                ResourceCeiling,
            ),
            (
                "max_timeout_retries=2 exhausted (DeadlineExceeded backstop)",
                Timeout,
            ),
            (
                "poison threshold reached after 3 distinct-worker failures",
                Target,
            ),
            (
                "poison threshold reached on worker disconnect after prior failures",
                Target,
            ),
            (
                "poison threshold reached on recovery (orphan worker did not reconnect)",
                Target,
            ),
            ("failed on every eligible worker", Target),
            ("max_retries=5 exhausted after transient failures", Target),
            (
                "builder failed with exit code 2: make: *** [all] Error 2",
                Target,
            ),
            (
                "dependency '/nix/store/cccccccccccccccccccccccccccccccc-dep.drv' failed: poison threshold reached after 3 distinct-worker failures",
                Dependency {
                    failing_drv: "/nix/store/cccccccccccccccccccccccccccccccc-dep.drv".into(),
                },
            ),
        ];
        for (reason, expected) in cases {
            assert_eq!(classify_reason(reason), expected, "reason: {reason}");
        }
    }

    #[test]
    fn signatures_are_deterministic_and_grouped() {
        assert_eq!(
            signature_for(
                Some("max_infra_retries=3 exhausted after infrastructure failures: x"),
                None
            ),
            Some("infra-retries-exhausted".into())
        );
        assert_eq!(
            signature_for(
                Some("poison threshold reached after 3 distinct-worker failures"),
                None
            ),
            Some("poison-threshold".into())
        );
        let a = signature_for(Some("builder failed with exit code 2: make error"), None);
        let b = signature_for(Some("builder failed with exit code 2: make error"), None);
        assert_eq!(a, b);
        assert!(a.unwrap().starts_with("worker:"));
        // Log-tail-only fallback.
        let s = signature_for(None, Some("phase x\nerror: linker `cc` not found\n"));
        assert!(s.unwrap().starts_with("log:error--linker"));
        assert_eq!(signature_for(None, None), None);
    }
}
