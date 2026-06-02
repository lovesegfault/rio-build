//! Parsing of the gateway's relayed stderr lines — the `rio: build <uuid>`
//! announcement and the per-derivation failure lines — captured as evidence
//! by the client-ops build observer, plus the classification of relayed
//! scheduler reasons (infra vs. target vs. dependency) and the deterministic
//! failure signatures used to group identical failures.

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
/// scheduler terminal failure (`rio-gateway/src/handler/build.rs`). For a
/// derivation that actually executed, the gateway embeds the
/// `↳ rio-cli logs '<drv>'` hint after a newline INSIDE the same stderr
/// payload — and in this regex's default (non-multiline) mode `.` stops at
/// a newline while `$` matches ONLY at the end of the haystack (no `(?m)`
/// before-a-newline matching, and none of PCRE's before-final-newline
/// allowance), so against an unsplit multi-line payload the pattern
/// matches NOTHING and the relayed reason would be silently lost. Callers
/// must therefore split each payload into lines before matching — the
/// build observer does — and the hint line itself never matches and is
/// ignored. The engine semantics are pinned by
/// `drv_failed_dollar_matches_only_at_end_of_haystack` below.
static DRV_FAILED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"derivation '([^']+\.drv)' failed: (.*)$").expect("static regex"));

/// Everything the engine extracts from one batch's stderr.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedStderr {
    /// Build id from the gateway's `rio: build <uuid>` line. The gateway
    /// emits exactly one such line per accepted client submission, so
    /// the FIRST id seen wins; a later line carrying a different id is
    /// ignored (with a warning) rather than re-keying the submission
    /// mid-stream.
    pub build_id: Option<String>,
    /// drv path → reason (first occurrence wins; the scheduler emits one
    /// terminal Failed per drv per build).
    pub reasons: BTreeMap<String, String>,
}

/// Feed one stderr line into `parsed` (streaming form, used by the
/// client-ops build observer, which splits each relayed stderr payload
/// into lines before calling this).
pub fn parse_line(parsed: &mut ParsedStderr, line: &str) {
    if let Some(c) = BUILD_ID_RE.captures(line) {
        let id = &c[1];
        match &parsed.build_id {
            None => parsed.build_id = Some(id.to_string()),
            // One client submission carries exactly one accepted rio
            // build, so a second DISTINCT id means the stream is not what
            // the engine assumes (e.g. two submissions' stderr concatenated).
            // Keep the first id — earlier failure lines belong to it — and
            // surface the anomaly instead of silently switching handles.
            Some(existing) if existing != id => {
                tracing::warn!(
                    kept = %existing,
                    ignored = id,
                    "second distinct `rio: build` id in one stderr stream; keeping the first"
                );
            }
            Some(_) => {}
        }
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
/// same one. The 60-character slugs collapse byte-identical message
/// prefixes only: the same underlying failure mode worded differently (or
/// carrying different embedded paths/versions) yields different signatures,
/// so signature counts are NOT failure-mode counts — folding those together
/// is a curated-rule-table concern for a later milestone.
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
                    // Unprefixed worker permanent failure.
                    format!("worker:{}", slug60(r))
                }
            }
        };
        return Some(sig);
    }
    log_tail.and_then(|t| {
        let line = t
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        let slug = slug60(line);
        // A whitespace-only (or all-punctuation) tail slugs to nothing; a
        // bare "log:" would group unrelated failures together, so report no
        // signature at all instead.
        (!slug.is_empty()).then(|| format!("log:{slug}"))
    })
}

/// Normalize a free-form message into a short grouping slug: ASCII
/// alphanumerics are lowercased, everything else becomes `-`, the result is
/// cut at 60 characters and trimmed of leading/trailing dashes.
fn slug60(text: &str) -> String {
    let mut s: String = text
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
    s.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured-shape fixture: the relayed stderr an ssh-ng client
    /// submission against rio-gateway observes for a two-job batch where one
    /// drv fails on the worker and a dependent cascades. Format strings
    /// verified against rio-gateway/src/handler/build.rs and
    /// rio-scheduler/src/actor/*.rs.
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

    #[test]
    fn second_distinct_build_id_keeps_the_first() {
        let mut p = ParsedStderr::default();
        parse_line(&mut p, "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a");
        parse_line(&mut p, "rio: build ffffffff-ffff-ffff-ffff-ffffffffffff");
        assert_eq!(
            p.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
    }

    #[test]
    fn failure_line_with_progress_prefix_still_parses() {
        let mut p = ParsedStderr::default();
        parse_line(
            &mut p,
            "[31/5/97 built] derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: builder failed with exit code 2",
        );
        assert_eq!(
            p.reasons["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv"],
            "builder failed with exit code 2"
        );
    }

    /// Pins the regex-engine semantics [`DRV_FAILED_RE`]'s mandatory
    /// line-splitting rests on: in Rust's regex default (non-multiline)
    /// mode `$` matches ONLY at the end of the haystack — not before an
    /// interior newline (that is `(?m)` behavior) and not before a final
    /// trailing newline either (PCRE's allowance, which this engine does
    /// not have). An unsplit relay therefore matches NOTHING — the reason
    /// is dropped outright, not matched up to its first line — which is
    /// why every caller splits payloads into lines first.
    #[test]
    fn drv_failed_dollar_matches_only_at_end_of_haystack() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";
        let line = format!("derivation '{drv}' failed: builder failed with exit code 2");
        // Unsplit two-line relay (reason line + hint line in one payload):
        // zero matches, not a first-line match.
        let unsplit = format!("{line}\n  ↳ rio-cli logs '{drv}'");
        assert!(DRV_FAILED_RE.captures(&unsplit).is_none());
        // Newline-TERMINATED single line: still zero matches.
        assert!(DRV_FAILED_RE.captures(&format!("{line}\n")).is_none());
        // Positive control: the properly split line matches and captures.
        let c = DRV_FAILED_RE.captures(&line).expect("split line matches");
        assert_eq!(&c[1], drv);
        assert_eq!(&c[2], "builder failed with exit code 2");
    }

    #[test]
    fn duplicate_failure_lines_keep_the_first_reason() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";
        let mut p = ParsedStderr::default();
        parse_line(&mut p, &format!("derivation '{drv}' failed: first reason"));
        parse_line(&mut p, &format!("derivation '{drv}' failed: second reason"));
        assert_eq!(p.reasons.len(), 1);
        assert_eq!(p.reasons[drv], "first reason");
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
        // Tails whose slug is empty (whitespace-only, or nothing but
        // punctuation) must not yield a bare "log:" signature.
        assert_eq!(signature_for(None, Some("   \n\t  \n")), None);
        assert_eq!(signature_for(None, Some("*** !!! ***\n")), None);
    }
}
