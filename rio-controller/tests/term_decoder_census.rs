//! W13-AJ3 (bug_063): the one-alphabet-one-decoder census — the
//! in-crate face of the cross-crate split form (the workspace union
//! row lands at the meta-plane registry; rio-scheduler's only consult
//! is the `decode_capacity_requirement` delegation, asserted there).
//!
//! LAW: rio-controller has ONE decode law for the
//! `(hw_class_names, node_affinity)` capacity grammar — the shared
//! `rio_common::k8s::capacity_term` decoder. Every PRODUCTION consult
//! of selector-term requirements that adjudicates the capacity key is
//! either (a) a BLESSED site feeding the shared decoder (the
//! `TermRequirement` view construction / `decode_capacity_term`
//! call), or (b) a content-keyed EXCEPTION row below — so a third
//! consumer of the grammar cannot mint its own parse. That is the
//! bug_063 channel verbatim: merged_bug_039 hardened the scheduler's
//! decoder, and merged_bug_006's close rebuilt the condemned peek
//! cross-crate ONE DAY later; single-site hardening does not survive
//! template reuse, and a hand-audit does not survive the next
//! template reuse either — this census does.
//!
//! Census shape (R31′): the population derives from the grammar's
//! consult SHAPES — a production `.match_expressions` READ whose
//! classification window adjudicates the capacity key (ident in
//! stripped text, literal in raw text) — never from names or a hand
//! list; the universe is the shared embedded table
//! (census_universe.rs — ONE copy, live-tree-pinned via
//! timeout_census.rs); exception rows are CONTENT-KEYED at the
//! granularity of the consult expression and SHRINK-ONLY (a row
//! matching zero or multiple live sites is a red); the self-test
//! carries a planted-DUPLICATE inside the population and a
//! K-mutation battery over the census's own control flow (each
//! seeded mutation must MISS the plant the intact census catches).
//!
//! PROVISIONAL (T0-disclosed): authored in the standing [GEN-SET]
//! census shape pending the wave's H1-pack census-grammar post;
//! re-verified at the integration rebase (grammar consumer, never
//! re-deriver).

#[path = "census_universe.rs"]
mod census_universe;
use census_universe::{CENSUS_SOURCES, production_line_mask, strip_rust};

/// The grammar's key, as the IDENT consults compare against (the
/// const survives `strip_rust`; every known comparison uses a const
/// path ending in this ident).
const CAPACITY_KEY_IDENT: &str = "CAPACITY_TYPE_LABEL";
/// The grammar's key, as a LITERAL (strings are blanked by
/// `strip_rust`, so the literal lane reads RAW lines — belt and
/// braces against a consult that inlines the string).
const CAPACITY_KEY_LITERAL: &str = "karpenter.sh/capacity-type";
/// The blessed shapes: feeding the ONE shared decoder.
const BLESSED_NEEDLES: [&str; 2] = ["decode_capacity_term", "TermRequirement"];
/// Classification window below the consult line. Sized to cover a
/// rustfmt-wrapped closure chain (the largest known consult spans 9
/// lines); a window mutation is part of the K-battery.
const WINDOW: usize = 14;

/// One classified production consult of the term grammar.
#[derive(Debug, PartialEq, Eq)]
struct Consult {
    rel: String,
    /// 1-based line of the `.match_expressions` read.
    line: usize,
    kind: ConsultKind,
    /// The trimmed consult line — the content key's anchor.
    content: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ConsultKind {
    /// Feeds the shared decoder (the lawful shape).
    Blessed,
    /// Adjudicates the capacity key with its own logic.
    KeyComparison,
}

/// A frozen exception: a production consult that lawfully keeps its
/// own capacity-key read, keyed by CONTENT (R31′(i) — the row dies
/// when the site's content changes; rel alone would quotient distinct
/// sites, the bug_047 degenerate-key shape).
struct ExceptionRow {
    rel: &'static str,
    /// A verbatim (trimmed) line that must appear in the consult's
    /// classification window — the content key.
    content_key: &'static str,
    /// Why this consult cannot ride the typed decoder.
    rationale: &'static str,
}

/// The frozen exception set — SHRINK-ONLY. Adding a row is a review
/// reject unless the new consult provably cannot consume the shared
/// decoder; deleting happens when the site rebinds or dies.
const EXCEPTIONS: [ExceptionRow; 1] = [ExceptionRow {
    rel: "reconcilers/pool/jobs.rs",
    content_key: "r.key == crate::reconcilers::nodeclaim_pool::ffd::CAPACITY_TYPE_LABEL",
    rationale: "intent_cells_annotation_value renders the intent-cells \
                annotation and needs the VERBATIM capacity token for \
                byte-fidelity (the arm decode accepts wire AND karpenter \
                forms, so re-encoding through WireCapacity would rewrite \
                the operator's bytes); every off-shape face degrades to \
                None (fail-open no-stamp — refuse-aligned with the typed \
                law, never an inversion), and the annotation's re-parse \
                half validates each token through WireCapacity::parse, \
                the shared alphabet owner.",
}];

/// Scan controls — the K-mutation battery's seam. Production scans
/// use [`ScanFlags::default`]; each battery arm flips exactly one
/// flag and must MISS the plant.
struct ScanFlags {
    /// m1: drop the ident lane (literal-only classification).
    ident_lane: bool,
    /// m2: walk the population at all.
    walk_population: bool,
    /// m4: classification window size.
    window: usize,
    /// m5: require a struct-literal shape (`match_expressions:`)
    /// instead of the read shape (`.match_expressions`).
    literal_shape_needle: bool,
    /// m6: honor the production mask (false scans nothing — the
    /// inverted-jurisdiction miss).
    production_only: bool,
}

impl Default for ScanFlags {
    fn default() -> Self {
        Self {
            ident_lane: true,
            walk_population: true,
            window: WINDOW,
            literal_shape_needle: false,
            production_only: true,
        }
    }
}

/// The census scan: every production `.match_expressions` read whose
/// window adjudicates the capacity key, classified Blessed (feeds the
/// shared decoder) or KeyComparison (its own logic). Pure over the
/// embedded pairs — the live universe, the plants, and the battery
/// all route through here.
fn scan(pairs: &[(&str, &str)], flags: &ScanFlags) -> Vec<Consult> {
    let mut out = Vec::new();
    if !flags.walk_population {
        return out;
    }
    let read_needle = if flags.literal_shape_needle {
        "match_expressions:"
    } else {
        ".match_expressions"
    };
    for (rel, src) in pairs {
        // Module-graph test membership: files under a tests/ module
        // DIR are cfg(test)-gated at their parent's decl
        // (`#[cfg(test)] pub(super) mod tests;`), invisible to the
        // per-file mask — the decl pin below keeps this path rule
        // from rotting.
        if flags.production_only && rel.contains("/tests/") {
            continue;
        }
        let stripped = strip_rust(src);
        let raw_lines: Vec<&str> = src.lines().collect();
        let stripped_lines: Vec<&str> = stripped.lines().collect();
        let mask = production_line_mask(&stripped);
        for (i, line) in stripped_lines.iter().enumerate() {
            if flags.production_only && !mask[i] {
                continue;
            }
            if !line.contains(read_needle) {
                continue;
            }
            // Struct-literal construction (`match_expressions: vec![`)
            // is the PRODUCER face, not a consult — excluded under the
            // read-shape needle by the leading dot.
            if !flags.literal_shape_needle && line.contains("match_expressions:") {
                continue;
            }
            let end = (i + flags.window).min(stripped_lines.len());
            let win_stripped = &stripped_lines[i..end];
            let win_raw = &raw_lines[i..end];
            let blessed = win_stripped
                .iter()
                .any(|l| BLESSED_NEEDLES.iter().any(|n| l.contains(n)));
            let ident_hit =
                flags.ident_lane && win_stripped.iter().any(|l| l.contains(CAPACITY_KEY_IDENT));
            let literal_hit = win_raw.iter().any(|l| l.contains(CAPACITY_KEY_LITERAL));
            let kind = if blessed {
                Some(ConsultKind::Blessed)
            } else if ident_hit || literal_hit {
                Some(ConsultKind::KeyComparison)
            } else {
                // Generic term consumption (affinity matching, k8s
                // conversion, fingerprints) — outside the capacity
                // grammar's population.
                None
            };
            if let Some(kind) = kind {
                out.push(Consult {
                    rel: (*rel).to_string(),
                    line: i + 1,
                    kind,
                    content: raw_lines[i].trim().to_string(),
                });
            }
        }
    }
    out
}

/// True iff the exception row matches this consult: same file AND the
/// content key appears verbatim (trimmed) in the consult's window.
fn exception_matches(row: &ExceptionRow, c: &Consult, pairs: &[(&str, &str)]) -> bool {
    if c.rel != row.rel {
        return false;
    }
    let Some((_, src)) = pairs.iter().find(|(rel, _)| *rel == c.rel) else {
        return false;
    };
    let lines: Vec<&str> = src.lines().collect();
    let end = (c.line - 1 + WINDOW).min(lines.len());
    lines[c.line - 1..end]
        .iter()
        .any(|l| l.trim().contains(row.content_key))
}

// r[verify ctrl.nodeclaim.one-term-decoder]
/// The census law over the live (embedded) tree: every KeyComparison
/// consult is a content-matched exception; every exception row
/// matches EXACTLY one live consult (shrink-only + the
/// planted-duplicate discrimination at row granularity); the blessed
/// shared-decoder site exists where the rebind landed (ffd.rs).
#[test]
fn one_decode_law_total() {
    let consults = scan(CENSUS_SOURCES, &ScanFlags::default());
    let blessed: Vec<&Consult> = consults
        .iter()
        .filter(|c| c.kind == ConsultKind::Blessed)
        .collect();
    assert!(
        blessed
            .iter()
            .any(|c| c.rel == "reconcilers/nodeclaim_pool/ffd.rs"),
        "cells_of_checked must consult through the shared decoder \
         (the blessed view-mapping shape); found: {blessed:?}"
    );
    let key_cmps: Vec<&Consult> = consults
        .iter()
        .filter(|c| c.kind == ConsultKind::KeyComparison)
        .collect();
    for c in &key_cmps {
        let matches: Vec<&ExceptionRow> = EXCEPTIONS
            .iter()
            .filter(|row| exception_matches(row, c, CENSUS_SOURCES))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "a production capacity-key consult outside the shared \
             decoder must be EXACTLY ONE frozen exception (a third \
             consumer cannot mint its own parse — route it through \
             rio_common::k8s::capacity_term or file a content-keyed \
             row with its impossibility rationale): {c:?}"
        );
    }
    for row in &EXCEPTIONS {
        let matched: Vec<&&Consult> = key_cmps
            .iter()
            .filter(|c| exception_matches(row, c, CENSUS_SOURCES))
            .collect();
        assert_eq!(
            matched.len(),
            1,
            "exception rows are SHRINK-ONLY and site-exact: row for \
             {} keyed '{}' matched {} live consults (0 = the site \
             rebound or died — delete the row; >1 = the content key \
             is degenerate, the bug_047 shape)",
            row.rel,
            row.content_key,
            matched.len()
        );
        assert!(
            !row.rationale.is_empty(),
            "every exception carries its impossibility rationale"
        );
    }
}

/// The path-rule's anti-rot pin: the tests/ module DIR this census
/// skips is cfg(test)-gated at its parent decl — if the gate moves,
/// this red points at the jurisdiction rule, not a silent widening.
#[test]
fn tests_dir_membership_pinned() {
    let (_, pool_mod) = CENSUS_SOURCES
        .iter()
        .find(|(rel, _)| *rel == "reconcilers/pool/mod.rs")
        .expect("pool/mod.rs in the embedded universe");
    let lines: Vec<&str> = pool_mod.lines().map(str::trim).collect();
    let decl = lines
        .iter()
        .position(|l| *l == "pub(super) mod tests;")
        .expect("the tests module decl exists");
    assert_eq!(
        lines[decl - 1],
        "#[cfg(test)]",
        "reconcilers/pool/tests/ must stay cfg(test)-gated at the \
         decl — the census's /tests/ path rule depends on it"
    );
}

/// The planted-DUPLICATE (R31′(iii), inside the population): a
/// production-shaped consult minting its own capacity parse — the
/// intact census MUST catch it as an unmatched KeyComparison.
const PLANT: (&str, &str) = (
    "reconcilers/planted/third_decoder.rs",
    r#"
pub fn rogue_cells(i: &SpawnIntent) -> Vec<Cell> {
    let mut cells = Vec::new();
    for (h, t) in i.hw_class_names.iter().zip(&i.node_affinity) {
        let cap = t
            .match_expressions
            .iter()
            .find(|r| r.key == CAPACITY_TYPE_LABEL)
            .and_then(|r| r.values.first());
        if let Some(v) = cap {
            cells.push(Cell(h.clone(), v.clone()));
        }
    }
    cells
}
"#,
);

/// The plant fires under the intact census (the red the battery
/// proves load-bearing). The plant is the condemned peek itself —
/// the exact shape bug_063 re-introduced cross-crate.
#[test]
fn planted_third_decoder_reds() {
    let mut pairs: Vec<(&str, &str)> = CENSUS_SOURCES.to_vec();
    pairs.push(PLANT);
    let consults = scan(&pairs, &ScanFlags::default());
    let plant_hits: Vec<&Consult> = consults
        .iter()
        .filter(|c| c.rel == PLANT.0 && c.kind == ConsultKind::KeyComparison)
        .collect();
    assert_eq!(
        plant_hits.len(),
        1,
        "the planted peek must be detected as a KeyComparison consult"
    );
    let c = plant_hits[0];
    assert!(
        !EXCEPTIONS
            .iter()
            .any(|row| exception_matches(row, c, &pairs)),
        "the plant must not quotient under any exception row \
         (content keys discriminate at site granularity)"
    );
}

/// The K-mutation battery (R31′(iii)): K=5 seeded mutations of the
/// census's own control flow, each of which must MISS the plant the
/// intact census catches — a self-test that cannot detect its own
/// artifact's degeneration is the bug_047 born-broken shape.
#[test]
fn k_mutation_battery_kills_the_plant_detection() {
    let mut pairs: Vec<(&str, &str)> = CENSUS_SOURCES.to_vec();
    pairs.push(PLANT);
    let detected = |flags: &ScanFlags| {
        scan(&pairs, flags)
            .iter()
            .any(|c| c.rel == PLANT.0 && c.kind == ConsultKind::KeyComparison)
    };
    assert!(
        detected(&ScanFlags::default()),
        "precondition: the intact census detects the plant"
    );
    // m1: ident lane dropped — the plant compares via the const
    // ident, so a literal-only classifier goes blind.
    let m1 = ScanFlags {
        ident_lane: false,
        ..Default::default()
    };
    // m2: population walk emptied — the absence-of-hits ==
    // absence-of-evidence failure mode.
    let m2 = ScanFlags {
        walk_population: false,
        ..Default::default()
    };
    // m3: classification window collapsed — the consult line alone
    // carries no key comparison (rustfmt wraps the chain).
    let m3 = ScanFlags {
        window: 1,
        ..Default::default()
    };
    // m4: the read-shape needle swapped for the struct-literal
    // (producer) shape — population selected by the wrong grammar
    // face.
    let m4 = ScanFlags {
        literal_shape_needle: true,
        ..Default::default()
    };
    // m5: jurisdiction inverted — scanning nothing as "production".
    let m5 = ScanFlags {
        production_only: true,
        walk_population: false,
        ..Default::default()
    };
    for (name, flags) in [
        ("m1-ident-lane-dropped", m1),
        ("m2-population-emptied", m2),
        ("m3-window-collapsed", m3),
        ("m4-wrong-shape-needle", m4),
        ("m5-jurisdiction-emptied", m5),
    ] {
        assert!(
            !detected(&flags),
            "{name}: the seeded mutation must MISS the plant — if it \
             still detects, the mutated control flow is not \
             load-bearing and the battery is vacuous"
        );
    }
}

/// The discrimination plant (R31′(i)): a SECOND consult in the SAME
/// file as the frozen exception, different content — a rel-keyed
/// (degenerate) exception would quotient both under one row; the
/// content key must keep them distinct.
#[test]
fn same_file_second_consult_is_not_quotiented() {
    let dup = (
        "reconcilers/pool/jobs.rs",
        r#"
pub fn second_parse(i: &SpawnIntent) -> usize {
    i.node_affinity
        .iter()
        .flat_map(|t| t.match_expressions.iter())
        .filter(|r| r.key == CAPACITY_TYPE_LABEL && r.operator == "NotIn")
        .count()
}
"#,
    );
    // The plant rides a SEPARATE pair entry with the exception's rel:
    // exception_matches resolves content through the FIRST pair with
    // that rel (the real file), so the plant's window cannot satisfy
    // the row — exactly the discrimination the content key buys.
    let mut pairs: Vec<(&str, &str)> = CENSUS_SOURCES.to_vec();
    pairs.push(dup);
    let consults = scan(&pairs, &ScanFlags::default());
    let dup_consult = consults
        .iter()
        .filter(|c| c.rel == "reconcilers/pool/jobs.rs" && c.content.contains("flat_map"))
        .collect::<Vec<_>>();
    assert_eq!(
        dup_consult.len(),
        1,
        "the second consult is seen distinctly"
    );
    assert!(
        !EXCEPTIONS
            .iter()
            .any(|row| exception_matches(row, dup_consult[0], &[dup])),
        "the second same-file consult must NOT match the frozen row \
         by content (rel-only matching is the degenerate key)"
    );
}
