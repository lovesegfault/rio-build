//! Parsing of the gateway's relayed stderr lines — the `rio: build <uuid>`
//! announcement and the per-derivation failure lines — captured as evidence
//! by the client-ops build observer, plus the classification of relayed
//! scheduler reasons (infra vs. target vs. dependency) and the deterministic
//! failure signatures used to group identical failures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use rio_nix::protocol::build::BuildResult;
use rio_nix::protocol::client::STDERR_BUDGET_NODE_MULTIPLIER_CAP;

/// Cardinality cap on the per-batch `reasons` map: one entry per distinct
/// drv, at most the largest closure the stderr drain budget itself
/// recognizes for one op ([`STDERR_BUDGET_NODE_MULTIPLIER_CAP`] — the
/// node-multiplier clamp, >4× a chromium/texlive-class closure).
///
/// Trust provenance: the keys come from the relayed stderr channel, which
/// also carries WORKER-CONTROLLED build-log text (a build printing
/// `derivation '<x>.drv' failed: …` lines mints entries — the documented
/// spoof surface of this parser). The genuine producer emits one terminal
/// failure line per drv of the submitted DAG, so legitimate cardinality is
/// bounded by the op's merged-closure node count — which the drain budget
/// clamps at exactly this constant. Beyond it, only a flood of fabricated
/// keys is possible: under the raised drain budget (10M messages per root,
/// scaling with the closure estimate) an unbounded map grows multi-GiB
/// resident AND is persisted wholesale to batches.jsonl, then reloaded and
/// re-synced on every collect pass. Entries past the cap are dropped (the
/// capped raw tail still carries the lines as stream evidence), counted in
/// [`ParsedStderr::reasons_dropped`], and warned about once per stream.
///
/// Worst-case resident cost, named: 65,536 entries × (≤4 KiB value +
/// ~130 B key) ≈ 270 MiB — reachable only by a hostile worker printing
/// tens of thousands of distinct fabricated failure lines, survivable by
/// the engine pod, and loud (the drop warning names the cap).
pub(crate) const MAX_CAPTURED_REASONS: usize = STDERR_BUDGET_NODE_MULTIPLIER_CAP;

/// Per-value byte cap on a captured failure reason: the SHARED
/// retained-evidence cap, [`rio_common::limits::MAX_RETAINED_ERROR_BYTES`]
/// — the same constant the gateway applies to the per-root `BuildResult`
/// messages it RETAINS (`RETAINED_ERROR_MSG_CAP` in its build handler),
/// while relaying the full text only as stream traffic. Referencing the
/// one constant (instead of mirroring its literal, the pre-hoist shape)
/// makes the two retention postures structurally unable to drift. This
/// map is likewise retained state — persisted to batches.jsonl and
/// reloaded every pass — so it takes the retained cap, and the full line
/// remains available as stream evidence in the capped stderr tail
/// whenever it was among the last 200 lines.
///
/// Cost on genuine evidence, named: a genuine relayed reason can reach
/// ~16 KiB (the scheduler truncates worker `error_message`s at its
/// 16 KiB ingress cap, `MAX_ERROR_MSG_LEN`, and the gateway relays that
/// body verbatim; the cascade composition prefixes it once, never
/// recursively). Truncation at 4 KiB loses the tail of such a reason in
/// the RETAINED copy only. Every classification consumer reads prefixes
/// (`classify_reason`'s `starts_with` vocabulary, the dependency
/// trigger's quoted drv, `signature_for`'s 60-char slug), so
/// classification is unaffected; a needle scan over the retained value
/// could miss a needle past 4 KiB, exactly as it already does for the
/// gateway-retained in-band result messages capped at the same constant.
pub(crate) const MAX_CAPTURED_REASON_BYTES: usize = rio_common::limits::MAX_RETAINED_ERROR_BYTES;

/// Cardinality cap on the per-batch `lost_terminals` set — same
/// provenance and envelope as [`MAX_CAPTURED_REASONS`]: the genuine
/// producer (the gateway's lost-terminal relay marker) emits at most one
/// marker per root of the submitted DAG, the marker parser is byte-0
/// anchored but worker text can still reach byte 0 via the plain
/// `STDERR_NEXT` fallback (see the trust-bound doc on
/// `BuildResult::lost_terminal_relay_drv`), and each distinct spoofed
/// line otherwise grows the uncapped set. Keys are drv paths (~130 B), so
/// the capped worst case is ~8 MiB.
pub(crate) const MAX_CAPTURED_LOST_TERMINALS: usize = STDERR_BUDGET_NODE_MULTIPLIER_CAP;

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
    /// Roots for which the gateway relayed the lost-terminal marker line
    /// ([`BuildResult::lost_terminal_relay_line`]): the root's own
    /// terminal event never reached the gateway's relay while the
    /// DAG-level word implied it was resolved (completed DAG, failed
    /// keep-going DAG, or a gateway-synthesized reconnect-exhausted
    /// word), store presence was positively confirmed, and the wire
    /// status is therefore `Substituted` — indistinguishable in band
    /// from a genuine substitution terminal. Collect routes such a root's `Substituted`
    /// row to evidence-loss classification instead of recording a
    /// substitution event. Parsed by the shared producer-exact pair in
    /// rio-nix, the same discipline as the in-band evidence-loss prefix.
    pub lost_terminals: BTreeSet<String>,
    /// Distinct NEW `reasons` keys dropped because the map was at
    /// `MAX_CAPTURED_REASONS` (crate-private, see its doc) —
    /// observability for the cardinality clamp, never persisted (the
    /// capped tail still carries the dropped lines as stream evidence).
    pub reasons_dropped: usize,
    /// Distinct NEW marker drvs dropped at
    /// `MAX_CAPTURED_LOST_TERMINALS` (crate-private) — same role as
    /// [`reasons_dropped`](Self::reasons_dropped).
    pub lost_terminals_dropped: usize,
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
        // First occurrence wins for keys already present (unchanged);
        // NEW keys are admitted only under the cardinality cap, and a
        // kept value is truncated to the retained-evidence byte cap —
        // see the two constants' docs for provenance and the named
        // costs. `contains_key` first so a re-occurrence of an in-map
        // key is never miscounted as a drop at the boundary.
        if parsed.reasons.contains_key(&c[1]) {
            // First-wins: later occurrences never displace the value.
        } else if parsed.reasons.len() < MAX_CAPTURED_REASONS {
            let mut reason = c[2].trim().to_string();
            rio_common::grpc::truncate_utf8(&mut reason, MAX_CAPTURED_REASON_BYTES);
            parsed.reasons.insert(c[1].to_string(), reason);
        } else {
            if parsed.reasons_dropped == 0 {
                tracing::warn!(
                    cap = MAX_CAPTURED_REASONS,
                    dropped_drv = &c[1],
                    "stderr failure-reason capture is at its cardinality cap; dropping new \
                     entries (the capped stderr tail still carries the lines)"
                );
            }
            parsed.reasons_dropped += 1;
        }
    }
    // The gateway's lost-terminal relay marker, matched by the shared
    // producer-exact parser (byte-0 anchored, whole-line — see its doc
    // for why this vocabulary does not inherit the progress-prefix
    // tolerance of the regexes above). Same cardinality clamp as the
    // reasons map: re-occurrences of an in-set drv are no-ops at any
    // size, new drvs past the cap are dropped and counted.
    if let Some(drv) = BuildResult::lost_terminal_relay_drv(line) {
        if !parsed.lost_terminals.contains(drv)
            && parsed.lost_terminals.len() >= MAX_CAPTURED_LOST_TERMINALS
        {
            if parsed.lost_terminals_dropped == 0 {
                tracing::warn!(
                    cap = MAX_CAPTURED_LOST_TERMINALS,
                    dropped_drv = drv,
                    "lost-terminal marker capture is at its cardinality cap; dropping new \
                     entries (the capped stderr tail still carries the lines)"
                );
            }
            parsed.lost_terminals_dropped += 1;
        } else {
            parsed.lost_terminals.insert(drv.to_string());
        }
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

/// The scheduler's terminal-failure reason vocabulary, paired with the
/// class the comparison model assigns each string — the shared corpus for
/// every test that quantifies over "what reasons can the scheduler relay".
/// Strings are verbatim producer shapes from completion.rs / executor.rs /
/// dispatch.rs / recovery.rs; the embedded-cause rows compose the relay
/// prefix with rio's own worker/transport error text exactly as production
/// does (`completion.rs` embeds the worker's `error_msg`, which for a
/// store-upload failure is rio-builder's "output upload failed: …" wrapping
/// rio-common's "'<op>' timed out after <t>") — those rows are what give
/// the needle-collision cross-product its non-vacuous pairs.
#[cfg(test)]
pub(crate) fn scheduler_reason_corpus() -> Vec<(&'static str, ReasonClass)> {
    use ReasonClass::*;
    vec![
        (
            "max_infra_retries=3 exhausted after infrastructure failures: store unavailable",
            Infra,
        ),
        // Embedded worker cause: rio-builder wraps a store-upload failure
        // ("output upload failed: {e}", executor/outputs.rs) around
        // rio-common's timeout text ("'{name}' timed out after {timeout:?}",
        // grpc.rs) before completion.rs relays it — infra vocabulary that
        // CONTAINS the "timed out" needle.
        (
            "max_infra_retries=3 exhausted after infrastructure failures: output upload \
             failed: 'PutPathChunked' timed out after 30s",
            Infra,
        ),
        // Embedded transport cause: rio-common's with_retry timeout text
        // ("gRPC call '{name}' timed out after {timeout:?}", grpc.rs).
        (
            "max_infra_retries=3 exhausted after infrastructure failures: gRPC call \
             'PutPath' timed out after 30s",
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
        // Unprefixed worker message carrying a real fetch failure — the
        // must-admit control for the source-rot scan: Target-classified,
        // so the needle scan IS consulted for a fixed-output drv.
        (
            "builder failed: unable to download 'https://example.com/src.tar.gz'",
            Target,
        ),
        (
            "dependency '/nix/store/cccccccccccccccccccccccccccccccc-dep.drv' failed: poison \
             threshold reached after 3 distinct-worker failures",
            Dependency {
                failing_drv: "/nix/store/cccccccccccccccccccccccccccccccc-dep.drv".into(),
            },
        ),
    ]
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

    /// The lost-terminal relay marker is captured into `lost_terminals`
    /// — fixture constructed VIA the shared producer formatter
    /// ([`BuildResult::lost_terminal_relay_line`], the exact fn the
    /// gateway emission calls), never a hand-written consumer string —
    /// and the capture admits ONLY the producer's whole-line shape: the
    /// embedded forms a relayed failure payload can carry (worker-quoted
    /// log lines, the failure relay's own first line) must not mint a
    /// marker, because their text is worker-controlled while the genuine
    /// marker is gateway-authored.
    #[test]
    fn lost_terminal_marker_is_captured_producer_exact() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";
        let other = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv";
        let marker = BuildResult::lost_terminal_relay_line(drv);

        // Whole-capture form (resume / post-mortem): the marker line as
        // the gateway frames it — its own newline-terminated payload —
        // among ordinary relay traffic.
        let capture = format!(
            "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a\n\
             {marker}\n\
             derivation '{other}' failed: builder failed with exit code 2\n"
        );
        let parsed = parse_stderr(&capture);
        assert_eq!(
            parsed.lost_terminals,
            BTreeSet::from([drv.to_string()]),
            "the marker must be captured for exactly its drv"
        );
        assert!(parsed.build_id.is_some());
        assert_eq!(parsed.reasons.len(), 1, "the failure relay still parses");

        // Streaming form: same line through parse_line.
        let mut p = ParsedStderr::default();
        parse_line(&mut p, &marker);
        assert!(p.lost_terminals.contains(drv));

        // Must-NOT-capture: the marker text on worker-controllable
        // channels — embedded in a relayed failure message (its first
        // line, and a daemon-quoted log line inside the same payload) —
        // and the engine's own evidence-tail rendering of such payloads.
        let mut p = ParsedStderr::default();
        for line in [
            format!("derivation '{other}' failed: {marker}"),
            format!("> {marker}"),
            format!("{marker} (trace ab)"),
        ] {
            parse_line(&mut p, &line);
        }
        assert!(
            p.lost_terminals.is_empty(),
            "embedded/suffixed marker text must not mint a capture: {p:?}"
        );
    }

    /// Pins the ACCEPTED spoof surface of the marker capture — the
    /// documented residual, kept current deliberately so widening or
    /// closing it is a conscious act, never drift. Worker-controlled
    /// text CAN reach byte 0 of an observed line on two routes the
    /// producer-side trust-provenance doc names
    /// ([`BuildResult::lost_terminal_relay_drv`]): the gateway's
    /// no-live-activity fallback relay emits raw worker lines as
    /// single-line payloads, and the observer-boundary split puts the
    /// non-first lines of a multi-line payload at byte 0. A
    /// marker-shaped line arriving on either route IS captured — the
    /// parser cannot distinguish it from the gateway-authored emission,
    /// and the defense is consumer-enforced instead (the conservative
    /// evidence-loss flip, priced in that doc at its gate-accounting
    /// worst case: shared auto-retry budget burn, then an
    /// infra-indeterminate terminal that trips the regression gate).
    /// If this test starts failing because the capture grew an
    /// authenticity conjunct, re-derive the trust-bound doc and this
    /// pin together.
    #[test]
    fn worker_reachable_marker_text_is_captured_by_design() {
        let victim = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";
        // The spoof string is the producer formatter's own output: a
        // worker spoofs by reproducing it byte-for-byte, so the fixture
        // must be constructed via the same fn the gateway emission calls.
        let marker = BuildResult::lost_terminal_relay_line(victim);

        // Route 1 — observer-boundary split: the marker as a NON-FIRST
        // line of a multi-line payload (worker-authored body relayed in
        // one frame), split exactly as the observer splits payloads
        // (`str::lines`, the documented observer-boundary discipline).
        let payload = format!("build log tail before the forgery\n{marker}\n");
        let mut split_route = ParsedStderr::default();
        for line in payload.lines() {
            parse_line(&mut split_route, line);
        }
        assert!(
            split_route.lost_terminals.contains(victim),
            "the split route's byte-0 marker line is captured today: {split_route:?}"
        );

        // Route 2 — fallback relay: the marker as its own single-line
        // payload (raw worker line for a drv with no live activity),
        // indistinguishable from the genuine gateway frame.
        let mut fallback_route = ParsedStderr::default();
        parse_line(&mut fallback_route, &marker);
        assert!(
            fallback_route.lost_terminals.contains(victim),
            "the fallback route's whole-line marker is captured today: {fallback_route:?}"
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

    /// Synthesizes the i-th distinct drv path (fixed-width hash part, so
    /// every path is regex-shaped and unique).
    fn drv_path(i: usize) -> String {
        format!("/nix/store/{:032x}-pkg-{i}.drv", i)
    }

    /// THE accumulation-sink universe of the stderr observer, as one
    /// standing test: every field of [`ParsedStderr`] is destructured
    /// WITHOUT `..`, so adding a new sink fails compilation here until
    /// its bound is declared and asserted alongside the others. The
    /// channel feeding these sinks carries worker-controlled text under
    /// a drain budget of millions of messages per op — bounded COUNT
    /// does not bound ACCUMULATION, so each sink must hold its own
    /// bound: `build_id` first-wins (1), `reasons` capped in cardinality
    /// and value bytes, `lost_terminals` capped in cardinality, and the
    /// two drop counters are plain integers.
    #[test]
    fn observed_sink_universe_is_bounded() {
        let mut p = ParsedStderr::default();
        // Two distinct build ids; over-cap distinct reason keys with
        // oversized values; over-cap distinct marker drvs.
        parse_line(&mut p, "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a");
        parse_line(&mut p, "rio: build ffffffff-ffff-ffff-ffff-ffffffffffff");
        let big_reason = "x".repeat(MAX_CAPTURED_REASON_BYTES + 1);
        for i in 0..MAX_CAPTURED_REASONS + 2 {
            parse_line(
                &mut p,
                &format!("derivation '{}' failed: {big_reason}", drv_path(i)),
            );
        }
        for i in 0..MAX_CAPTURED_LOST_TERMINALS + 2 {
            parse_line(&mut p, &BuildResult::lost_terminal_relay_line(&drv_path(i)));
        }

        let ParsedStderr {
            build_id,
            reasons,
            lost_terminals,
            reasons_dropped,
            lost_terminals_dropped,
        } = p;
        // build_id: first id wins, exactly one retained.
        assert_eq!(
            build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        // reasons: cardinality clamped AT the cap (never one past it),
        // overflow counted, every retained value within the byte cap.
        assert_eq!(reasons.len(), MAX_CAPTURED_REASONS);
        assert_eq!(reasons_dropped, 2);
        assert!(
            reasons
                .values()
                .all(|v| v.len() <= MAX_CAPTURED_REASON_BYTES),
            "a retained reason exceeds the value cap"
        );
        // lost_terminals: same clamp shape.
        assert_eq!(lost_terminals.len(), MAX_CAPTURED_LOST_TERMINALS);
        assert_eq!(lost_terminals_dropped, 2);
    }

    /// Both sides of every conjunct the cardinality clamp adds, at the
    /// exact boundary: one under the cap admits, AT the cap a NEW key is
    /// dropped (and counted) while a RE-OCCURRENCE of an in-map key is
    /// not a drop — first-wins semantics are byte-identical to the
    /// uncapped behavior for everything the cap retains. Same rows for
    /// the marker set, where an in-set re-occurrence at the cap stays a
    /// no-op success.
    #[test]
    fn cardinality_cap_boundary_rows_in_both_directions() {
        // reasons: fill to cap-1, admit one more (reaches cap), then
        // cross.
        let mut p = ParsedStderr::default();
        for i in 0..MAX_CAPTURED_REASONS - 1 {
            parse_line(
                &mut p,
                &format!("derivation '{}' failed: r{i}", drv_path(i)),
            );
        }
        assert_eq!(
            (p.reasons.len(), p.reasons_dropped),
            (MAX_CAPTURED_REASONS - 1, 0)
        );
        let last_admitted = drv_path(MAX_CAPTURED_REASONS - 1);
        parse_line(
            &mut p,
            &format!("derivation '{last_admitted}' failed: at-cap"),
        );
        assert_eq!(
            (p.reasons.len(), p.reasons_dropped),
            (MAX_CAPTURED_REASONS, 0)
        );
        // New key at the cap: dropped and counted.
        parse_line(
            &mut p,
            &format!(
                "derivation '{}' failed: over",
                drv_path(MAX_CAPTURED_REASONS)
            ),
        );
        assert_eq!(
            (p.reasons.len(), p.reasons_dropped),
            (MAX_CAPTURED_REASONS, 1)
        );
        // Re-occurrence of an in-map key at the cap: first-wins no-op,
        // NOT a drop.
        parse_line(
            &mut p,
            &format!("derivation '{last_admitted}' failed: displaced?"),
        );
        assert_eq!(
            (p.reasons.len(), p.reasons_dropped),
            (MAX_CAPTURED_REASONS, 1)
        );
        assert_eq!(p.reasons[&last_admitted], "at-cap");

        // lost_terminals: same boundary walk through the marker parser.
        let mut p = ParsedStderr::default();
        for i in 0..MAX_CAPTURED_LOST_TERMINALS {
            parse_line(&mut p, &BuildResult::lost_terminal_relay_line(&drv_path(i)));
        }
        assert_eq!(
            (p.lost_terminals.len(), p.lost_terminals_dropped),
            (MAX_CAPTURED_LOST_TERMINALS, 0)
        );
        parse_line(
            &mut p,
            &BuildResult::lost_terminal_relay_line(&drv_path(MAX_CAPTURED_LOST_TERMINALS)),
        );
        assert_eq!(
            (p.lost_terminals.len(), p.lost_terminals_dropped),
            (MAX_CAPTURED_LOST_TERMINALS, 1)
        );
        // In-set re-occurrence at the cap: still a no-op success.
        parse_line(&mut p, &BuildResult::lost_terminal_relay_line(&drv_path(0)));
        assert_eq!(
            (p.lost_terminals.len(), p.lost_terminals_dropped),
            (MAX_CAPTURED_LOST_TERMINALS, 1)
        );
        assert!(p.lost_terminals.contains(&drv_path(0)));
    }

    /// Hostile-magnitude rows for the value cap, asserting CLAMP vs
    /// SCALE explicitly: a worker-shaped 1 MiB single-line reason is
    /// clamped to the retained-evidence byte cap (never scales the
    /// retained map with the input), the clamp cuts at a char boundary
    /// (a multi-byte char straddling the cap backs off, keeping the
    /// String valid), and — the other side — a reason of exactly the cap
    /// is retained whole, as is every genuine scheduler-corpus reason
    /// (all far under the cap; the corpus is the producer vocabulary).
    ///
    /// The hostile-magnitude clamp is asserted against the SHARED
    /// constant ([`rio_common::limits::MAX_RETAINED_ERROR_BYTES`]) — the
    /// one the gateway's own retention uses — so re-pointing this
    /// crate's alias at a local literal (the pre-hoist mirror) fails
    /// here, not in a code review.
    #[test]
    fn reason_values_clamp_at_the_retained_byte_cap() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";

        // Hostile magnitude: 1 MiB line → clamped, not scaled.
        let mut p = ParsedStderr::default();
        let huge = "y".repeat(1024 * 1024);
        parse_line(&mut p, &format!("derivation '{drv}' failed: {huge}"));
        assert_eq!(
            p.reasons[drv].len(),
            rio_common::limits::MAX_RETAINED_ERROR_BYTES
        );

        // Char-boundary clamp: a 3-byte char straddling the cap backs
        // off to the boundary before it.
        let mut p = ParsedStderr::default();
        let straddling = format!("{}€tail", "z".repeat(MAX_CAPTURED_REASON_BYTES - 1));
        parse_line(&mut p, &format!("derivation '{drv}' failed: {straddling}"));
        assert_eq!(p.reasons[drv].len(), MAX_CAPTURED_REASON_BYTES - 1);
        assert!(p.reasons[drv].chars().all(|c| c == 'z'));

        // At the cap exactly: retained whole.
        let mut p = ParsedStderr::default();
        let at_cap = "w".repeat(MAX_CAPTURED_REASON_BYTES);
        parse_line(&mut p, &format!("derivation '{drv}' failed: {at_cap}"));
        assert_eq!(p.reasons[drv], at_cap);

        // Every genuine producer reason (the shared scheduler corpus) is
        // far under the cap and retained verbatim — the clamp never
        // touches legitimate single-line evidence.
        for (reason, _) in scheduler_reason_corpus() {
            let mut p = ParsedStderr::default();
            parse_line(&mut p, &format!("derivation '{drv}' failed: {reason}"));
            assert_eq!(p.reasons[drv], reason.trim(), "corpus reason mangled");
        }
    }

    /// Every scheduler terminal-failure reason string in the shared corpus
    /// ([`scheduler_reason_corpus`]) maps to the class the comparison model
    /// assigns it.
    #[test]
    fn reason_classification_covers_all_scheduler_strings() {
        for (reason, expected) in scheduler_reason_corpus() {
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
