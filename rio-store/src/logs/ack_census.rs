//! [GEN-SET] The durable-ack carrier census (merged_bug_005, R31).
//!
//! `durable_through_line` is a sealed wire quantity: a
//! contiguous-prefix durability claim whose ONE producing formula is
//! [`rio_log_kernel::CoverageMap::contiguous_durable_frontier`]
//! (consumed in this crate through `IngestSession::durable_frontier` /
//! `clamped_durable_ack`). This module is the machine-derived census
//! of every in-crate site that carries the quantity — the generator
//! IS the committed walk below, never an author-typed member list.
//!
//! The quantity's in-crate carriers (the accessor grammar): the wire
//! field `durable_through_line` (`AppendLogAck`), the accept-outcome
//! carrier `durable_through` (`AcceptOutcome::CoveredReplay`), and
//! the cut carrier `durable_ack` (`CutCommit`). Every production
//! site that touches one of them must hold an adjacent
//! `durable-ack: <class>` tag from the CLOSED vocabulary:
//!
//! - `producer` — mints a value through the one producing chain
//!   (`clamped_durable_ack` / `durable_frontier` on the same site);
//! - `forward` — re-emits an already-clamped carrier value without
//!   arithmetic;
//! - `bind` — destructures a carrier (pattern bind) for forwarding,
//!   suppression, or assertion;
//! - `decl` — the carrier type/field declaration itself.
//!
//! Reader rows (the R31 per-reader measure-compatibility register —
//! each reader's ASSUMED measure, and what entails it):
//!
//! - builder `trim()` (rio-builder/src/log_upload.rs, cross-crate):
//!   assumes the CONTIGUOUS-PREFIX measure — every line at-or-below
//!   the ack is durable. Entailed by the producer clamp; witnessed
//!   crate-side by `frontier_satisfies_the_prefix_measure` (W12-C)
//!   and builder-side by the commit-3 trim-arm witness. The
//!   workspace-UNION completeness face ("every consult site across
//!   ALL crates") is NOT discharged here: it lands at the same-wave
//!   S8 registry union row, which re-runs this grammar over the
//!   staged workspace (the in-crate face enforces from this commit).
//! - the mbt mirror's `acked_below` (test half): assumes a prefix
//!   watermark; entailed by the same clamp (`Some(v)` implies every
//!   line `<= v` durable).
//! - the service forwarding arms: assume the carrier is
//!   already-clamped; entailed because every `producer` site routes
//!   through the one chain and no `forward` site performs
//!   arithmetic (the census law below).
//!
//! Census faces (the §2 riders block): the jurisdiction is DERIVED
//! from `mod.rs` declarations (never a hand crate-list) and pinned
//! bidirectionally against the embedded universe ((wwwww) — a new
//! module joins the embed or the pin goes red, the auto-join face);
//! population floors are per declared carrier root (`service.rs`,
//! `ingest.rs`) plus a global non-vacuity floor; the three plants are
//! the enrollment strawman (W12-C2), the hand-list jurisdiction
//! strawman, and the struct-update grammar-refusal.

use std::collections::BTreeSet;

/// The embedded census universe ((wwwww) form): every PRODUCTION
/// module of `logs/`, by literal compile-time embed. The jurisdiction
/// pin asserts this list equals the `mod.rs`-derived declaration set,
/// so the list cannot rot into a hand census.
const UNIVERSE: &[(&str, &str)] = &[
    ("chunks", include_str!("chunks.rs")),
    ("gate", include_str!("gate.rs")),
    ("ingest", include_str!("ingest.rs")),
    ("loss", include_str!("loss.rs")),
    ("service", include_str!("service.rs")),
    ("sessions", include_str!("sessions.rs")),
    ("sweep", include_str!("sweep.rs")),
    ("tail", include_str!("tail.rs")),
];

/// The carrier-field needles (the proto-accessor grammar: field
/// writes, field reads, and pattern binds all contain the carrier
/// name; comment lines are excluded by the scanner).
const CARRIERS: &[&str] = &["durable_through_line", "durable_through", "durable_ack"];

/// The closed tag vocabulary.
const CLASSES: &[&str] = &["producer", "forward", "bind", "decl"];

/// Parse the production module names out of `mod.rs`: `mod x;` /
/// `pub mod x;` declarations NOT preceded by a `#[cfg(test)]`
/// attribute line. THE jurisdiction derivation — the universe embed
/// is checked against this, never trusted on its own.
fn declared_production_modules(mod_src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut prev_was_cfg_test = false;
    for line in mod_src.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let is_cfg_test = t.replace(' ', "").starts_with("#[cfg(test)]");
        if let Some(rest) = t
            .strip_prefix("pub mod ")
            .or_else(|| t.strip_prefix("mod "))
            && let Some(name) = rest.strip_suffix(';')
            && !prev_was_cfg_test
        {
            out.insert(name.trim().to_string());
        }
        prev_was_cfg_test = is_cfg_test;
    }
    out
}

/// The production lines of one module source: every line NOT inside
/// a `#[cfg(test)]`-gated item. Structural, not path-conventional
/// (the bug_152 lesson): on a cfg(test) attribute line the scanner
/// consumes the WHOLE following item — a one-line statement (ends in
/// `;` before any `{`) or a braced item tracked to depth zero — so
/// mid-file test helpers, recorder modules, and gated statements are
/// excluded wherever they sit. Glitches err toward OVERSCAN (a line
/// misread as production goes red until tagged), never silence.
fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let cfg_test = format!("#[cfg({})]", "test");
    let mut out = Vec::new();
    let mut lines = src.lines().enumerate().peekable();
    while let Some((i, line)) = lines.next() {
        let t = line.trim();
        if !t.starts_with(&cfg_test) {
            out.push((i + 1, line));
            continue;
        }
        // Skip further attributes/doc lines, then the item itself.
        let mut depth: i64 = 0;
        let mut entered = false;
        for (_, l) in lines.by_ref() {
            let lt = l.trim();
            if !entered && (lt.starts_with("#[") || lt.starts_with("//") || lt.is_empty()) {
                continue;
            }
            let opens = l.matches('{').count() as i64;
            let closes = l.matches('}').count() as i64;
            if !entered && opens == 0 {
                if lt.ends_with(';') {
                    break; // one-line gated statement or declaration
                }
                continue; // signature spilling toward its brace
            }
            entered = true;
            depth += opens - closes;
            if depth <= 0 {
                break;
            }
        }
    }
    out
}

/// One censused site.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    module: &'static str,
    line: usize,
    carrier: &'static str,
    class: Option<String>,
}

/// The generating walk: every non-comment production line touching a
/// carrier, with its adjacent tag (same line or up to two lines
/// above). Refuses the struct-update idiom outright (grammar face):
/// `AppendLogAck { ..x }` would smuggle a carrier value past the
/// per-field scan, so its presence in any production half is an
/// error, not a site.
fn walk(universe: &[(&'static str, &str)]) -> Result<Vec<Site>, String> {
    let update_idiom = format!("AppendLogAck {{ {}", "..");
    let tag_needle = "durable-ack:";
    let mut sites = Vec::new();
    for &(module, src) in universe {
        let lines = production_lines(src);
        for (idx, &(line_no, line)) in lines.iter().enumerate() {
            let t = line.trim();
            if t.starts_with("//") {
                continue;
            }
            if t.contains(&update_idiom) {
                return Err(format!(
                    "{module}.rs:{line_no}: struct-update construction of AppendLogAck \
                     evades the per-field census; spell every field"
                ));
            }
            let Some(carrier) = CARRIERS.iter().find(|c| t.contains(**c)).copied() else {
                continue;
            };
            // `durable_through_line` contains `durable_through`;
            // attribute the longest match.
            let carrier = if t.contains("durable_through_line") {
                "durable_through_line"
            } else {
                carrier
            };
            let class = [idx.saturating_sub(2), idx.saturating_sub(1), idx]
                .iter()
                .filter_map(|&i| {
                    let l = lines[i].1;
                    l.find(tag_needle).map(|p| {
                        l[p + tag_needle.len()..]
                            .trim()
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_string()
                    })
                })
                .next_back();
            sites.push(Site {
                module,
                line: line_no,
                carrier,
                class,
            });
        }
    }
    Ok(sites)
}

/// The census law over a walked site list: total tagging, closed
/// vocabulary, per-root population floors, expected members.
fn violations(sites: &[Site]) -> Vec<String> {
    let mut v = Vec::new();
    for s in sites {
        match &s.class {
            None => v.push(format!(
                "{}.rs:{} untagged carrier site ({}); add `durable-ack: <class>` \
                 within two lines (closed set: {CLASSES:?})",
                s.module, s.line, s.carrier
            )),
            Some(c) if !CLASSES.contains(&c.as_str()) => v.push(format!(
                "{}.rs:{} out-of-vocabulary tag {c:?} (closed set: {CLASSES:?})",
                s.module, s.line
            )),
            Some(_) => {}
        }
    }
    // Expected members (verification targets for the generator — the
    // census riders' expected-member face; the generator remains the
    // population source).
    let count = |m: &str, class: &str| {
        sites
            .iter()
            .filter(|s| s.module == m && s.class.as_deref() == Some(class))
            .count()
    };
    if count("ingest", "producer") < 3 {
        v.push(
            "ingest.rs must hold the three producer mints (below-floor arm, covered \
             consult, cut commit) — the one-producer law lost a member"
                .to_string(),
        );
    }
    if count("service", "forward") < 3 {
        v.push(
            "service.rs must hold the forwarding arms (covered-replay ack, do_cut \
             ack, drain ack) — a wire construction left the census"
                .to_string(),
        );
    }
    if count("service", "producer") < 1 {
        v.push(
            "service.rs must hold the open-time frontier ack (the producer-consult \
             site)"
                .to_string(),
        );
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The jurisdiction pin ((wwwww), bidirectional): the embedded
    /// universe equals the `mod.rs`-derived production module set. A
    /// module added to `mod.rs` without joining the embed — or
    /// removed without leaving it — is a red, so the population is
    /// DERIVED, never a hand list that can rot.
    #[test]
    fn jurisdiction_is_derived_from_module_declarations() {
        let declared = declared_production_modules(include_str!("mod.rs"));
        let embedded: BTreeSet<String> = UNIVERSE.iter().map(|&(m, _)| m.to_string()).collect();
        assert_eq!(
            declared, embedded,
            "the census universe must equal the mod.rs production declarations \
             (left: declared, right: embedded); embed the new module or drop the \
             stale one"
        );
        // The hand-list strawman (the jurisdiction plant's refusal
        // half): a population missing one declared member must be
        // refused by this same comparison.
        let mut strawman = embedded.clone();
        strawman.remove("tail");
        assert_ne!(
            declared, strawman,
            "a hand-list population missing a declared module must go red"
        );
    }

    // r[verify store.log.frontier-denominated]
    /// The census main face: the walk is non-vacuous (population
    /// floor, timeout_census house form), every site is tagged from
    /// the closed vocabulary, and the expected members are present.
    #[test]
    fn durable_ack_census_is_total_and_classified() {
        let sites = walk(UNIVERSE).expect("the production universe holds no refused idiom");
        assert!(
            !sites.is_empty(),
            "the walk found ZERO carrier sites — the generator is broken (the \
             ack constructions in service.rs alone guarantee members)"
        );
        // Per declared carrier root: at least one member each.
        for root in ["service", "ingest"] {
            assert!(
                sites.iter().any(|s| s.module == root),
                "population floor: {root}.rs declared as a carrier root but the \
                 walk found no member in it"
            );
        }
        let v = violations(&sites);
        assert!(
            v.is_empty(),
            "durable-ack census violations:\n{}",
            v.join("\n")
        );
    }

    /// W12-C2 (the enrollment/completeness plant): an in-grammar but
    /// UNTAGGED carrier construction appended to a copy of the
    /// universe must red the census — driven through the SAME walk
    /// path as production (the empty-walk face rides the non-vacuity
    /// floor above).
    #[test]
    fn untagged_carrier_site_is_refused() {
        let strawman_src = format!(
            "fn smuggle(v: u64) -> AppendLogAck {{\n    AppendLogAck {{\n        {}: v,\n        open_coverage_next_line: None,\n    }}\n}}\n",
            "durable_through_line"
        );
        let planted: Vec<(&'static str, &str)> = vec![("service", strawman_src.as_str())];
        let sites = walk(&planted).expect("the strawman is in-grammar, not a refused idiom");
        assert!(
            !sites.is_empty(),
            "the plant must be FOUND by the walk (else the grammar lost the field)"
        );
        assert!(
            violations(&sites)
                .iter()
                .any(|v| v.contains("untagged carrier site")),
            "an untagged construction must be refused (the plant went green)"
        );
    }

    /// The grammar-refusal plant: the struct-update idiom smuggles a
    /// carrier value past any per-field scan, so the walk ERRORS on
    /// it (overscan-red, never silently green).
    #[test]
    fn struct_update_idiom_is_refused() {
        let strawman_src = format!(
            "fn smuggle(base: AppendLogAck) -> AppendLogAck {{\n    AppendLogAck {{ {}base }}\n}}\n",
            ".."
        );
        let planted: Vec<(&'static str, &str)> = vec![("service", strawman_src.as_str())];
        assert!(
            walk(&planted).is_err(),
            "struct-update construction of the ack must be a census ERROR"
        );
    }

    /// The jurisdiction plant (in-slot auto-join half): a carrier
    /// site in a module the census previously had no member for
    /// (sweep.rs is member-free today) is picked up by the SAME live
    /// walk — presence in the fresh walk is the oracle; the
    /// registry-diff half binds at the S8 union row.
    #[test]
    fn carrier_in_a_previously_silent_module_auto_joins() {
        let strawman_src = format!(
            "fn relay(ack: AppendLogAck) -> u64 {{\n    // durable-ack: bind\n    ack.{}\n}}\n",
            "durable_through_line"
        );
        let mut planted: Vec<(&'static str, &str)> = UNIVERSE.to_vec();
        planted.push(("sweep", strawman_src.as_str()));
        let sites = walk(&planted).expect("the plant is lawful and tagged");
        assert!(
            sites.iter().any(|s| s.module == "sweep"),
            "a carrier site planted inside the jurisdiction but outside the \
             previously-populated modules must appear in the fresh walk"
        );
    }

    // r[verify store.log.frontier-denominated]
    /// W12-C (the R31 measure-compatibility witness): the trim arm's
    /// ASSUMED measure is the contiguous prefix — every line at-or-
    /// below the ack is durable. The producing fn satisfies it on
    /// every map; the STRAWMAN set-containment producer (the pre-fix
    /// formula: the covered batch's containment end) violates it on
    /// the holey population — the merged_bug_005 shape is unwritable
    /// without this test going red.
    #[test]
    fn frontier_satisfies_the_prefix_measure() {
        let maps: Vec<Vec<(u64, u64)>> = vec![
            vec![],
            vec![(0, 5)],
            vec![(0, 5), (10, 10)],
            vec![(5, 5)],
            vec![(0, 1), (2, 1), (4, 1)],
            vec![(0, 100), (150, 50), (300, 1)],
        ];
        for intervals in maps {
            let map = rio_log_kernel::CoverageMap::from_intervals(intervals.iter().copied());
            // The producing fn: prefix measure holds at its output.
            if let Some(f) = map.contiguous_durable_frontier() {
                for line in 0..=f {
                    assert!(
                        map.covers_range(line, line + 1),
                        "prefix measure violated at line {line} for frontier {f} \
                         over {intervals:?}"
                    );
                }
            }
            // The strawman set-measure producer: a covered batch's
            // containment end. On any holey map with a covered span
            // past the hole, it asserts durability across the hole.
            for &(first, count) in &intervals {
                let end = first + count;
                if map.covers_range(first, end) && first > map.contiguous_prefix_end() {
                    let strawman_ack = end - 1;
                    let holds = (0..=strawman_ack).all(|line| map.covers_range(line, line + 1));
                    assert!(
                        !holds,
                        "the strawman containment-end producer must violate the \
                         prefix measure on the holey population ({intervals:?})"
                    );
                }
            }
        }
    }
}
