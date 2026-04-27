//! ADR-023 SLA observability. `describe_all()` is wired into
//! `rio_scheduler::describe_metrics()`.
//!
//! Emit sites are **NOT** in this file. Each `counter!()` / `gauge!()`
//! / `histogram!()` lives inline next to the production code path that
//! produces the event, so an unwired metric is a `dead_code` lint
//! failure (a `pub fn` wrapper here would suppress that). The
//! `registered_and_emitted_are_consistent` test enforces the same
//! invariant from the other direction by grepping the emit-site source
//! for each `SLA_METRICS` name.

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};

use super::solve::SlaPrediction;

/// Register `# HELP` lines for every `rio_scheduler_sla_*` metric.
/// One call site (lib.rs `describe_metrics`) — keeping the SLA block
/// together means adding a metric is a one-file change.
pub fn describe_all() {
    describe_histogram!(
        "rio_scheduler_sla_prediction_ratio",
        Unit::Count,
        "actual/predicted, by dimension (labeled dim=wall|mem). \
         1.0=perfect; >1.0=under-predicted."
    );
    describe_counter!(
        "rio_scheduler_sla_envelope_result_total",
        "SLA envelope hit/miss per tier (labeled tier, result=hit|miss, \
         constraint=wall|mem|-). `constraint` names the dimension that \
         missed; `-` for hits."
    );
    describe_counter!(
        "rio_scheduler_sla_infeasible_total",
        "infeasible-at-any-tier count, labeled `reason` ∈ \
         {serial_floor, mem_ceiling, disk_ceiling, core_ceiling, \
         interrupt_runaway, capacity_exhausted}. The reason names which \
         constraint bound at the loosest tier (see InfeasibleReason)."
    );
    describe_counter!(
        "rio_scheduler_sla_suspicious_scaling_total",
        "exploration froze at maxCores still saturated (labeled tenant). \
         The build wants more cores than the cluster offers."
    );
    describe_counter!(
        "rio_scheduler_sla_outlier_rejected_total",
        "MAD-rejected samples (labeled tenant). Row stays in PG \
         (outlier_excluded=TRUE) for forensics; refit excludes it."
    );
    describe_counter!(
        "rio_scheduler_sla_mem_fit_weak_total",
        "M(c) Koenker-Machado pseudo-R1<0.7 fallback to independent \
         p90 (labeled tenant)"
    );
    describe_counter!(
        "rio_scheduler_sla_residual_multimodal_total",
        "Hartigan dip test rejected unimodality (p<0.05) on a key's \
         log-residuals (labeled tenant). The single-curve T(c) model \
         is wrong — likely two workloads sharing a pname."
    );
    describe_gauge!(
        "rio_scheduler_sla_prior_divergence",
        "fleet-median prior parameter ÷ operator-probe basis (labeled \
         `param`); outside [0.5, 2.0] ⇒ clamped to the band edge"
    );
    describe_gauge!(
        "rio_scheduler_sla_hw_cost_stale_seconds",
        Unit::Seconds,
        "age of the hw-band $/vCPU·hr snapshot. Climbs when the \
         lease-gated spot-price poller is failing or this replica is \
         standby (price is PG-backed; standby reads but doesn't write)"
    );
    describe_counter!(
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        "ICE-mask hardware ladder exhausted at the terminal tier with \
         no admissible (hw_class, cap) cell left. Labeled `tenant`, \
         `exit` (the cell the ladder gave up on). Replaces \
         `_ice_backoff_total`."
    );
    describe_counter!(
        "rio_scheduler_sla_hw_cost_unknown_total",
        "solve hit a (hw_class, cap) cell the cost table has no $/vCPU·hr \
         for; the cell is dropped from the admissible set (labeled \
         `tenant`). Sustained nonzero ⇒ hwClasses config drifted from \
         the cost-poller's instance-type menu."
    );
    describe_counter!(
        "rio_scheduler_sla_hw_cost_fallback_total",
        "cost-poller fell back from a live spot-price source. Labeled \
         `reason` ∈ {api_error, empty_history, parse, stale}. `stale`: \
         `_hw_cost_stale_seconds > 6 × pollInterval` → price() clamped \
         to the helm seed."
    );
    describe_counter!(
        "rio_scheduler_sla_als_round_cap_hit_total",
        "ALS alternation hit the 5-round cap without ‖Δα‖₁<10⁻² \
         convergence (labeled `tenant`). The α/T_ref(c) joint fit \
         failed to converge — alpha is the round-5 iterate, not a \
         fixed point. Sustained nonzero ⇒ ALS_MAX_ROUNDS too low or \
         the rank gate is admitting degenerate factor matrices."
    );
    describe_counter!(
        "rio_scheduler_sla_keys_evicted_total",
        "per-tenant LRU evicted a (pname, system) key at \
         `sla.maxKeysPerTenant` (labeled `tenant`). Nonzero ⇒ a \
         tenant is exhausting the fit-cache cap; check for \
         random-pname submissions (`r[sched.sla.threat.corpus-clamp]`)."
    );
}

/// Actual-vs-predicted score for one completion. Pure so the
/// `record_build_sample` call site stays a one-liner and the
/// hit/miss/ratio rules are unit-testable without a metrics recorder.
#[derive(Debug, PartialEq)]
pub struct CompletionScore {
    pub ratio_wall: Option<f64>,
    pub ratio_mem: Option<f64>,
    /// `(tier, "hit"|"miss", constraint)`. `None` ⇔ no tier was
    /// predicted (BestEffort / cold-start) — nothing to score against.
    pub envelope: Option<(String, &'static str, &'static str)>,
}

/// Score one completion against its dispatch-time prediction.
///
/// `actual_mem=0` ("poller didn't fire") is treated as no-signal — a
/// 0/predicted ratio would drag the histogram floor. Wall ratio is
/// gated on `pred.wall_secs.is_some()` so probe-path dispatches (no
/// fitted curve → no `T(c)`) don't emit.
///
/// `hw_factor` is the completion's `HwTable.factor(hw_class)`: the
/// prediction (`pred.wall_secs`) is in **reference-seconds** — `t_at()`
/// evaluates the ref-second-denominated fit — so `ratio_wall` first maps
/// `actual_wall` to the same timeline. The envelope check stays
/// wall-vs-wall (`tier_target` is the operator-facing wall-second SLA).
/// Pass `1.0` for unknown/absent hw_class.
///
/// Envelope: `miss` if wall blew the tier's binding bound OR memory
/// blew the reservation (`constraint` names which). Memory-miss is the
/// OOM-adjacent case — the build fit because the controller's headroom
/// pad absorbed it, but the model under-predicted.
pub fn score_completion(
    actual_wall: f64,
    hw_factor: f64,
    actual_mem: u64,
    pred: &SlaPrediction,
) -> CompletionScore {
    let ratio_wall = pred
        .wall_secs
        .filter(|p| *p > 0.0)
        .map(|p| (actual_wall * hw_factor) / p);
    let ratio_mem =
        (actual_mem > 0 && pred.mem_bytes > 0).then(|| actual_mem as f64 / pred.mem_bytes as f64);
    let envelope = pred.tier.as_ref().map(|tier| {
        let wall_miss = pred.tier_target.is_some_and(|t| actual_wall > t);
        let mem_miss = actual_mem > 0 && actual_mem > pred.mem_bytes;
        let (result, constraint) = if wall_miss {
            ("miss", "wall")
        } else if mem_miss {
            ("miss", "mem")
        } else {
            ("hit", "-")
        };
        (tier.clone(), result, constraint)
    });
    CompletionScore {
        ratio_wall,
        ratio_mem,
        envelope,
    }
}

/// `(counter-name → Σ label-variants)` from ONE drained snapshot.
///
/// `Snapshotter::snapshot` **drains** (counters swap to 0 — the handle
/// is an `Arc<AtomicU64>` cloned from the registry, so the swap zeros
/// the shared atomic). Never call it twice expecting cumulative values;
/// always capture once and assert against the capture. See
/// [`infeasible_counts`] for the per-`reason`-label variant.
#[cfg(test)]
pub fn counter_map(
    snap: &metrics_util::debugging::Snapshotter,
) -> std::collections::BTreeMap<String, u64> {
    use metrics_util::debugging::DebugValue;
    let mut m = std::collections::BTreeMap::new();
    for (ck, _, _, v) in snap.snapshot().into_vec() {
        if let DebugValue::Counter(c) = v {
            *m.entry(ck.key().name().to_owned()).or_default() += c;
        }
    }
    m
}

/// `(reason → count)` for `rio_scheduler_sla_infeasible_total` from ONE
/// drained snapshot. Same drain caveat as [`counter_map`] — calling
/// this in a `for r in ALL` loop and reading one reason per call is
/// 5/6-vacuous (iteration 0 drains; 1..N read zeros). bug 022.
#[cfg(test)]
pub fn infeasible_counts(
    snap: &metrics_util::debugging::Snapshotter,
) -> std::collections::HashMap<String, u64> {
    use metrics_util::debugging::DebugValue;
    snap.snapshot()
        .into_vec()
        .into_iter()
        .filter_map(|(ck, _, _, v)| {
            let k = ck.key();
            (k.name() == "rio_scheduler_sla_infeasible_total").then(|| {
                let reason = k
                    .labels()
                    .find(|l| l.key() == "reason")
                    .map(|l| l.value().to_owned())
                    .unwrap_or_default();
                let c = match v {
                    DebugValue::Counter(c) => c,
                    _ => 0,
                };
                (reason, c)
            })
        })
        .collect()
}

/// Every `rio_scheduler_sla_*` metric name. Single source of truth for
/// the [`tests::registered_and_emitted_are_consistent`] guard — adding
/// a metric means adding it here AND to [`describe_all`] AND wiring an
/// inline emit at a production call site, or that test fails. Catches
/// the "registered but never emitted" drift that left
/// `_resize_retry_total` and `_hw_cost_unknown_total` documented with
/// zero production callers for 11 commits.
#[cfg(test)]
pub const SLA_METRICS: &[&str] = &[
    "rio_scheduler_sla_prediction_ratio",
    "rio_scheduler_sla_envelope_result_total",
    "rio_scheduler_sla_infeasible_total",
    "rio_scheduler_sla_suspicious_scaling_total",
    "rio_scheduler_sla_outlier_rejected_total",
    "rio_scheduler_sla_mem_fit_weak_total",
    "rio_scheduler_sla_residual_multimodal_total",
    "rio_scheduler_sla_prior_divergence",
    "rio_scheduler_sla_hw_cost_stale_seconds",
    "rio_scheduler_sla_hw_ladder_exhausted_total",
    "rio_scheduler_sla_hw_cost_unknown_total",
    "rio_scheduler_sla_hw_cost_fallback_total",
    "rio_scheduler_sla_als_round_cap_hit_total",
    "rio_scheduler_sla_keys_evicted_total",
];

/// Metrics with a closed-domain label whose VALUES are each a separate
/// observability contract — the operator alerts on
/// `reason="api_error"`, not the bare counter. Each `(name, label,
/// value)` triple is checked by
/// [`tests::labeled_metric_values_have_emit_sites`] for a production
/// emit. Catches the bug_039 / merged_bug_006 class: name has ≥1 emit,
/// but a documented value has zero (`infeasible_total` had emits;
/// `reason="interrupt_runaway"` had none).
///
/// Two emit shapes are matched (see the test for the matcher):
///   1. Inline literal — `counter!("name", …, "label" => "value")`
///   2. Enum-dispatch — `counter!("name", …, "label" => x.as_str())`
///      where `=> "value"` is a match arm AND `Reason::{Variant}`
///      appears as a constructor in production source (i.e., NOT just
///      `Self::Variant` in `ALL` / `as_str()`).
///
/// `_envelope_result_total{result}` is deliberately NOT listed: its
/// `hit`/`miss` literals originate in [`score_completion`] above
/// (excluded from `SLA_FILES` by design — emit sites must live next to
/// the production code path) and are pinned directly by the
/// `envelope_*` tests below.
#[cfg(test)]
pub const SLA_LABELED_METRICS: &[(&str, &str, &[&str])] = &[
    (
        "rio_scheduler_sla_infeasible_total",
        "reason",
        &[
            "serial_floor",
            "mem_ceiling",
            "disk_ceiling",
            "core_ceiling",
            "interrupt_runaway",
            "capacity_exhausted",
        ],
    ),
    (
        "rio_scheduler_sla_hw_cost_fallback_total",
        "reason",
        &["api_error", "empty_history", "parse", "stale"],
    ),
    (
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        "exit",
        &["all_masked"],
    ),
];

#[cfg(test)]
mod tests {
    use super::super::solve::InfeasibleReason;
    use super::*;

    /// ADR-023 §Observability pins exactly these six `reason` label
    /// values, in this order. The enum's `ALL` is the contract.
    #[test]
    fn infeasible_reasons_complete() {
        let want = [
            "serial_floor",
            "mem_ceiling",
            "disk_ceiling",
            "core_ceiling",
            "interrupt_runaway",
            "capacity_exhausted",
        ];
        let got: Vec<_> = InfeasibleReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(got, want);
    }

    /// Source of every file with an SLA emit site, as separate strings
    /// so [`prod`] can strip each file's trailing `mod tests` block
    /// independently. Excludes THIS file: emit sites must live next to
    /// the production code path, not in a wrapper here. Adding an
    /// emit-file means adding it here too — but forgetting to do so
    /// fails the test (the metric appears nowhere), which is the safe
    /// direction.
    const SLA_FILES: &[&str] = &[
        include_str!("solve.rs"),
        include_str!("ingest.rs"),
        include_str!("cost.rs"),
        include_str!("mod.rs"),
        include_str!("explore.rs"),
        include_str!("prior.rs"),
        include_str!("../actor/snapshot.rs"),
        include_str!("../actor/completion.rs"),
    ];

    const EMIT_MACROS: [&str; 3] = ["counter!(", "gauge!(", "histogram!("];
    const DESCRIBE_MACROS: [&str; 3] = [
        "describe_counter!(",
        "describe_gauge!(",
        "describe_histogram!(",
    ];

    /// `src` truncated at its trailing `#[cfg(test)] mod tests` block.
    /// Inline `#[cfg(test)] fn` helpers earlier in the file are kept;
    /// only the test MODULE is dropped, so the label-value coverage
    /// check ignores test fixtures' `InfeasibleReason::Foo` references
    /// and test-only `counter!` calls.
    fn prod(src: &str) -> &str {
        src.split_once("#[cfg(test)]\nmod tests")
            .map_or(src, |(p, _)| p)
    }

    /// Argument-tail (next ≤256 bytes) of every `macros`-prefixed
    /// `"{name}"` literal in `src` — i.e., the slice immediately after
    /// the metric-name argument, holding the `"label" => value` pairs.
    /// Rejects word-char-prefixed matches so `describe_counter!(` does
    /// not satisfy `counter!(`. 256B covers every emit call in the
    /// codebase (≤8 lines) without bleeding past the next `counter!`
    /// for a *different* metric name.
    fn macro_call_tails<'a>(src: &'a str, name: &str, macros: &[&str]) -> Vec<&'a str> {
        let needle = format!("\"{name}\"");
        let mut out = vec![];
        let mut at = 0;
        while let Some(i) = src[at..].find(&needle) {
            let pos = at + i;
            let mut hs = pos.saturating_sub(64);
            while !src.is_char_boundary(hs) {
                hs += 1;
            }
            let head = src[hs..pos].trim_end();
            if macros.iter().any(|m| {
                head.strip_suffix(m).is_some_and(|pre| {
                    !pre.chars()
                        .last()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
                })
            }) {
                let start = pos + needle.len();
                let mut end = (start + 256).min(src.len());
                while !src.is_char_boundary(end) {
                    end -= 1;
                }
                out.push(&src[start..end]);
            }
            at = pos + needle.len();
        }
        out
    }

    /// `src` contains `"{name}"` as the first argument of one of
    /// `macros` (possibly across a line break).
    fn has_macro_call(src: &str, name: &str, macros: &[&str]) -> bool {
        !macro_call_tails(src, name, macros).is_empty()
    }

    /// Every `SLA_METRICS` name has a `describe_*!` registration AND at
    /// least one production `counter!`/`gauge!`/`histogram!` emit site
    /// in [`SLA_FILES`]. The previous version of this test
    /// self-invoked the wrapper functions, which made it pass for
    /// metrics with zero production callers — exactly the drift it was
    /// meant to catch. The version before THAT checked registration
    /// via `describe_src.contains("\"name\"")`, which matched the
    /// `SLA_METRICS` const itself — vacuously true.
    #[test]
    fn registered_and_emitted_are_consistent() {
        // Registration side: every SLA_METRICS name must appear inside
        // a describe_*! macro in describe_all()'s source. THIS file is
        // the only place describe_* lives, so include_str! itself.
        let describe_src = include_str!("metrics.rs");
        for name in SLA_METRICS {
            assert!(
                has_macro_call(describe_src, name, &DESCRIBE_MACROS),
                "{name} in SLA_METRICS but no describe_*! registration"
            );
        }
        // Emit side: every SLA_METRICS name must appear as the first
        // arg of a counter!/gauge!/histogram! macro in a production
        // source file (NOT this one).
        for name in SLA_METRICS {
            assert!(
                SLA_FILES
                    .iter()
                    .any(|f| has_macro_call(f, name, &EMIT_MACROS)),
                "{name} registered but never emitted in production code \
                 (no counter!/gauge!/histogram! call found in SLA_FILES)"
            );
        }
        // Retired metrics must NOT appear at any emit site.
        for retired in [
            "rio_scheduler_sla_ice_backoff_total",
            "rio_scheduler_sla_resize_retry_total",
            "rio_scheduler_sla_als_cap_hit_total",
        ] {
            assert!(
                !SLA_FILES
                    .iter()
                    .any(|f| has_macro_call(f, retired, &EMIT_MACROS)),
                "{retired} is retired but still emitted"
            );
        }
    }

    /// `snake_case` → `PascalCase` for the enum-dispatch variant check
    /// (`interrupt_runaway` → `InterruptRunaway`).
    fn pascal(s: &str) -> String {
        s.split('_')
            .filter_map(|w| {
                let mut it = w.chars();
                it.next()
                    .map(|c| c.to_ascii_uppercase().to_string() + it.as_str())
            })
            .collect()
    }

    /// Every `(name, label, value)` in [`SLA_LABELED_METRICS`] has a
    /// production emit. Two shapes are accepted:
    ///
    /// 1. **Inline literal** — an emit-macro call for `name` whose
    ///    argument tail contains `"label" => "value"` verbatim.
    ///    Covers `_hw_cost_fallback_total`, `_hw_ladder_exhausted_total`.
    /// 2. **Enum-dispatch** — an emit-macro call for `name` whose tail
    ///    contains `"label" => …as_str()`, AND production source has a
    ///    `=> "value"` match arm, AND production source constructs the
    ///    variant as `Reason::{Pascal}` (the `Self::` references in
    ///    `ALL` / `as_str()` don't count — they're definition-site, not
    ///    a reachable constructor). Covers `_infeasible_total`.
    ///
    /// Also asserts the converse for inline literals: every
    /// `"label" => "literal"` value at an emit site for `name` is in
    /// the declared set (the `emitted ⊆ documented` direction the
    /// `admissible-set` VM subtest used to own). Enum-dispatch
    /// soundness is [`infeasible_reasons_complete`] — `ALL` is the
    /// closed domain.
    #[test]
    fn labeled_metric_values_have_emit_sites() {
        let prod_src: String = SLA_FILES.iter().copied().map(prod).collect();
        for &(name, label, values) in SLA_LABELED_METRICS {
            let tails: Vec<&str> = SLA_FILES
                .iter()
                .flat_map(|f| macro_call_tails(prod(f), name, &EMIT_MACROS))
                .collect();
            assert!(!tails.is_empty(), "{name}: no production emit site");
            // Completeness: every documented value is emitted.
            for &v in values {
                let inline = format!(r#""{label}" => "{v}""#);
                if tails.iter().any(|t| t.contains(&inline)) {
                    continue;
                }
                let via_as_str = tails
                    .iter()
                    .any(|t| t.contains(&format!(r#""{label}" => "#)) && t.contains(".as_str()"));
                let arm = format!(r#"=> "{v}""#);
                let ctor = format!("Reason::{}", pascal(v));
                assert!(
                    via_as_str && prod_src.contains(&arm) && prod_src.contains(&ctor),
                    "{name}{{{label}=\"{v}\"}}: no production emit. \
                     inline `{inline}` absent; enum-dispatch \
                     via_as_str={via_as_str}, arm `{arm}` present={}, \
                     ctor `{ctor}` present={}",
                    prod_src.contains(&arm),
                    prod_src.contains(&ctor),
                );
            }
            // Soundness: every inline literal value is documented.
            let lit = format!(r#""{label}" => ""#);
            for t in &tails {
                for (i, _) in t.match_indices(&lit) {
                    let vs = i + lit.len();
                    let Some(ve) = t[vs..].find('"') else {
                        continue;
                    };
                    let found = &t[vs..vs + ve];
                    assert!(
                        values.contains(&found),
                        "{name}{{{label}=\"{found}\"}} emitted but NOT in \
                         SLA_LABELED_METRICS — undocumented label value"
                    );
                }
            }
        }
    }

    #[test]
    fn has_macro_call_distinguishes_prefix_and_bare_literal() {
        // Guard the guard. Test data is built at runtime so the
        // crate-wide `grep_emitted_names` source-grep (which scans
        // THIS file for `metrics::counter!("…"` literals) doesn't
        // pick up synthetic names.
        let m = "metrics";
        // `describe_counter!("X"` must NOT match EMIT_MACROS — its
        // tail is `counter!(` but the preceding `_` disqualifies.
        assert!(!has_macro_call(
            r#"describe_counter!("X")"#,
            "X",
            &EMIT_MACROS
        ));
        assert!(has_macro_call(
            &format!(r#"::{m}::counter!("X")"#),
            "X",
            &EMIT_MACROS
        ));
        assert!(has_macro_call(
            &format!("{m}::gauge!(\n            \"X\""),
            "X",
            &EMIT_MACROS
        ));
        assert!(!has_macro_call(r#"// see "X" metric"#, "X", &EMIT_MACROS));
        // DESCRIBE_MACROS: matches `describe_counter!("X"` but NOT a
        // bare `"X",` literal (the bug_040 vacuous-match case — the
        // SLA_METRICS const definition itself).
        assert!(has_macro_call(
            r#"describe_counter!("X")"#,
            "X",
            &DESCRIBE_MACROS
        ));
        assert!(!has_macro_call(r#""foo","#, "foo", &DESCRIBE_MACROS));
        assert!(!has_macro_call(
            &format!(r#"::{m}::counter!("X")"#),
            "X",
            &DESCRIBE_MACROS
        ));
    }

    fn pred(wall: f64, mem: u64, tier: &str, target: f64) -> SlaPrediction {
        SlaPrediction {
            wall_secs: Some(wall),
            mem_bytes: mem,
            tier: Some(tier.into()),
            tier_target: Some(target),
        }
    }

    #[test]
    fn ratio_wall_and_mem() {
        // wall=100, predicted=90 → ~1.11
        let s = score_completion(100.0, 1.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert!((s.ratio_wall.unwrap() - 1.111).abs() < 0.01);
        assert!((s.ratio_mem.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn envelope_hit_within_p90() {
        let s = score_completion(100.0, 1.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
    }

    #[test]
    fn envelope_miss_wall() {
        // wall > tier.p90 → result=miss, constraint=wall
        let s = score_completion(1500.0, 1.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "miss", "wall")));
    }

    #[test]
    fn envelope_miss_mem() {
        let s = score_completion(100.0, 1.0, 4 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "miss", "mem")));
    }

    #[test]
    fn no_tier_no_envelope() {
        let p = SlaPrediction {
            wall_secs: Some(90.0),
            mem_bytes: 2 << 30,
            tier: None,
            tier_target: None,
        };
        assert_eq!(score_completion(100.0, 1.0, 1 << 30, &p).envelope, None);
    }

    #[test]
    fn zero_mem_is_no_signal() {
        let s = score_completion(100.0, 1.0, 0, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert!(s.ratio_mem.is_none());
        // 0 mem shouldn't trip a mem-miss either.
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
    }

    // r[verify sched.sla.hw-ref-seconds]
    #[test]
    fn ratio_wall_normalizes_by_hw_factor_but_envelope_stays_wall() {
        // Fast hw (factor=2.0): wall=50s, ref=100s; predicted ref=100s.
        // ratio_wall = (50 × 2.0) / 100 = 1.0 (perfect prediction).
        // Without normalization the ratio would read 0.5 — the bug
        // that made sla_prediction_ratio{dim=wall} multimodal at
        // 1/factor[band] under heterogeneous hardware.
        let p = pred(100.0, 2 << 30, "normal", 60.0);
        let s = score_completion(50.0, 2.0, 1 << 30, &p);
        assert!((s.ratio_wall.unwrap() - 1.0).abs() < 1e-9);
        // Envelope is wall-vs-wall: 50s wall < 60s p90 → hit. The
        // hw_factor must NOT leak into the SLA envelope check.
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
        // Same wall on slow hw (factor=1.0) at p90=40s → miss.
        let s2 = score_completion(50.0, 1.0, 1 << 30, &pred(100.0, 2 << 30, "normal", 40.0));
        assert_eq!(s2.envelope, Some(("normal".into(), "miss", "wall")));
    }
}
