//! W9-AL: the timeout-Err census — D2's impl population, machine-derived.
//!
//! Every PRODUCTION `tokio::time::timeout` Err arm in this crate is
//! classified by consequence class {delay, refusal, irreversible} via
//! a `timeout-census: <class>` comment within the three lines at/above
//! the call, and the classified set is committed as
//! `tests/timeout_census.txt` (the [GEN-SET] artifact the
//! `census[gen: ...]` tags at the call sites bind to).
//!
//! Laws enforced here:
//! - **Totality**: an untagged production timeout site is a red.
//! - **Closed vocabulary**: a class outside {delay, refusal,
//!   irreversible} is a red (the label-key evasion axis).
//! - **Zero irreversible** (D2, post-D-053-1-rider): a site classified
//!   `irreversible` FAILS THIS TEST with a stop-and-escalate message —
//!   it is a D2 violation, never a census entry. The `sys.guard.*` D2
//!   RULE mints at the round-9 doctrine pass over THIS population.
//! - **Snapshot**: the committed artifact equals the live scan
//!   (regenerate with
//!   `cargo test -p rio-controller --test timeout_census -- --ignored regenerate_timeout_census`).
//!
//! Generator evasion corpus (R22 — one planted red per axis, under
//! `tests/timeout_census_corpus/`):
//! - `alias-red/`: `use tokio::time::timeout as <alias>` + an untagged
//!   `<alias>(...)` call — alias-form evasion must still scan.
//! - `scope-red/`: an untagged site in a sibling-crate-shaped tree —
//!   the generator finds sites in ANY root handed to it (the
//!   production scope — this crate's `src/` — is a declared argument,
//!   not a hardcode; the round-9 tier-2 census plane owns widening the
//!   default scope).
//! - `label-red/`: a site tagged with an out-of-vocabulary class.
//! - `cfgtest-green/`: a site inside a `#[cfg(test)]` module — the
//!   population is PRODUCTION arms; test code is excluded (behavior
//!   pin, not an axis).

//!
//! Sandbox form (the S1/b870121ac embed precedent — hazard (vvvvv) in
//! its in-crate face): the nix gate runs test binaries WITHOUT the
//! source tree on disk and with the COMPILE-TIME `env!` manifest path
//! pointing at a build dir that no longer exists, so a runtime walk is
//! premise-unreachable exactly where it gates. The census universe
//! (every `.rs` under `src/`), the R22 corpus plants, and the
//! committed snapshot are therefore EMBEDDED at compile time
//! (machine-generated `include_str!` tables — generator: a python walk
//! emitting sorted (relpath, include_str!) pairs, command recorded in
//! the owning commit body); `census_universe_matches_live_tree` pins
//! embed == live tree in BOTH directions on every dev run (the
//! sandbox skip is eprintln-disclosed, never silent). The regenerator
//! stays a dev-only live-tree writer.

use std::path::{Path, PathBuf};

/// One detected timeout call site.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    /// Path relative to the scanned root, `/`-separated.
    rel_path: String,
    /// The classification, when a recognized tag was found.
    class: Option<String>,
}

/// The closed consequence-class vocabulary (D2).
const CLASSES: [&str; 3] = ["delay", "refusal", "irreversible"];

/// EVERY `.rs` under `rio-controller/src`, embedded at compile time
/// (the S1/b870121ac CENSUS_SOURCES form). Machine-generated — sorted
/// (relpath, include_str!) pairs; the completeness pin
/// (`census_universe_matches_live_tree`) forces this table to track
/// the live tree exactly in both directions.
#[rustfmt::skip]
const CENSUS_SOURCES: &[(&str, &str)] = &[
    ("config.rs", include_str!("../src/config.rs")),
    ("error.rs", include_str!("../src/error.rs")),
    ("fixtures.rs", include_str!("../src/fixtures.rs")),
    ("guard.rs", include_str!("../src/guard.rs")),
    ("lib.rs", include_str!("../src/lib.rs")),
    ("main.rs", include_str!("../src/main.rs")),
    ("observability.rs", include_str!("../src/observability.rs")),
    ("reconcilers/componentscaler/decide.rs", include_str!("../src/reconcilers/componentscaler/decide.rs")),
    ("reconcilers/componentscaler/mod.rs", include_str!("../src/reconcilers/componentscaler/mod.rs")),
    ("reconcilers/fence.rs", include_str!("../src/reconcilers/fence.rs")),
    ("reconcilers/gc_schedule.rs", include_str!("../src/reconcilers/gc_schedule.rs")),
    ("reconcilers/mod.rs", include_str!("../src/reconcilers/mod.rs")),
    ("reconcilers/node_informer.rs", include_str!("../src/reconcilers/node_informer.rs")),
    ("reconcilers/nodeclaim_pool/consolidate.rs", include_str!("../src/reconcilers/nodeclaim_pool/consolidate.rs")),
    ("reconcilers/nodeclaim_pool/cover.rs", include_str!("../src/reconcilers/nodeclaim_pool/cover.rs")),
    ("reconcilers/nodeclaim_pool/evidence.rs", include_str!("../src/reconcilers/nodeclaim_pool/evidence.rs")),
    ("reconcilers/nodeclaim_pool/ffd.rs", include_str!("../src/reconcilers/nodeclaim_pool/ffd.rs")),
    ("reconcilers/nodeclaim_pool/health.rs", include_str!("../src/reconcilers/nodeclaim_pool/health.rs")),
    ("reconcilers/nodeclaim_pool/lifecycle_tests.rs", include_str!("../src/reconcilers/nodeclaim_pool/lifecycle_tests.rs")),
    ("reconcilers/nodeclaim_pool/mod.rs", include_str!("../src/reconcilers/nodeclaim_pool/mod.rs")),
    ("reconcilers/nodeclaim_pool/pods.rs", include_str!("../src/reconcilers/nodeclaim_pool/pods.rs")),
    ("reconcilers/nodeclaim_pool/sketch.rs", include_str!("../src/reconcilers/nodeclaim_pool/sketch.rs")),
    ("reconcilers/nodeclaim_pool/wedge.rs", include_str!("../src/reconcilers/nodeclaim_pool/wedge.rs")),
    ("reconcilers/pool/candidate.rs", include_str!("../src/reconcilers/pool/candidate.rs")),
    ("reconcilers/pool/disruption.rs", include_str!("../src/reconcilers/pool/disruption.rs")),
    ("reconcilers/pool/job.rs", include_str!("../src/reconcilers/pool/job.rs")),
    ("reconcilers/pool/jobs.rs", include_str!("../src/reconcilers/pool/jobs.rs")),
    ("reconcilers/pool/mod.rs", include_str!("../src/reconcilers/pool/mod.rs")),
    ("reconcilers/pool/pod.rs", include_str!("../src/reconcilers/pool/pod.rs")),
    ("reconcilers/pool/tests/builders_tests.rs", include_str!("../src/reconcilers/pool/tests/builders_tests.rs")),
    ("reconcilers/pool/tests/disruption_tests.rs", include_str!("../src/reconcilers/pool/tests/disruption_tests.rs")),
    ("reconcilers/pool/tests/jobs_tests.rs", include_str!("../src/reconcilers/pool/tests/jobs_tests.rs")),
    ("reconcilers/pool/tests/mod.rs", include_str!("../src/reconcilers/pool/tests/mod.rs")),
];
/// The R22 corpus plants, embedded the same way: (axis,
/// relpath-within-axis, contents).
#[rustfmt::skip]
const CORPUS_SOURCES: &[(&str, &str, &str)] = &[
    ("alias-red", "aliased.rs", include_str!("timeout_census_corpus/alias-red/aliased.rs")),
    ("cfgtest-green", "with_tests.rs", include_str!("timeout_census_corpus/cfgtest-green/with_tests.rs")),
    ("label-red", "mislabeled.rs", include_str!("timeout_census_corpus/label-red/mislabeled.rs")),
    ("scope-red", "other-crate/src/lib.rs", include_str!("timeout_census_corpus/scope-red/other-crate/src/lib.rs")),
];

/// The committed census artifact, embedded (same commit, same bytes —
/// the gate-time compare is never premise-unreachable).
const SNAPSHOT: &str = include_str!("timeout_census.txt");

/// Scan a set of embedded (relpath, contents) pairs for production
/// `tokio::time::timeout` call sites and their classifications. Pure
/// function of the pairs: the committed snapshot, the corpus reds,
/// and the production run all go through here.
fn scan_pairs(pairs: &[(&str, &str)]) -> Vec<Site> {
    let mut out = Vec::new();
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort();
    for (rel, contents) in sorted {
        scan_file(contents, rel, &mut out);
    }
    out.sort();
    out
}

/// One corpus axis as (relpath, contents) pairs.
fn corpus_pairs(axis: &str) -> Vec<(&'static str, &'static str)> {
    CORPUS_SOURCES
        .iter()
        .filter(|(a, _, _)| *a == axis)
        .map(|(_, rel, contents)| (*rel, *contents))
        .collect()
}

/// Dev-only live-tree walk (the regenerator + completeness pin).
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Per-file scan. Detection needles: the fully-qualified
/// `tokio::time::timeout(`, the bare name when `use tokio::time::timeout`
/// is in scope, and any alias from `use tokio::time::timeout as X`.
/// `#[cfg(test)]`-attributed blocks are excluded by brace tracking
/// (the population is production Err arms).
fn scan_file(src: &str, rel: &str, out: &mut Vec<Site>) {
    // Alias closure (the merged_bug_001 lesson, applied at birth).
    let mut needles: Vec<String> = vec!["tokio::time::timeout(".into()];
    for line in src.lines() {
        let t = line.trim();
        if t == "use tokio::time::timeout;" {
            needles.push("timeout(".into());
        } else if let Some(rest) = t.strip_prefix("use tokio::time::timeout as ")
            && let Some(alias) = rest.strip_suffix(';')
        {
            needles.push(format!("{}(", alias.trim()));
        }
    }
    let lines: Vec<&str> = src.lines().collect();
    let mut depth_skip: Option<i64> = None; // brace depth inside a cfg(test) block
    let mut pending_cfg_test = false;
    let mut depth: i64 = 0;
    for (i, raw) in lines.iter().enumerate() {
        let line = raw;
        let trimmed = line.trim();
        if depth_skip.is_none() {
            if trimmed.starts_with("#[cfg(test)]") {
                pending_cfg_test = true;
            } else if pending_cfg_test {
                // The attribute applies to THIS item; if it opens a
                // brace, skip until it closes.
                if line.contains('{') {
                    depth_skip = Some(depth);
                }
                if !trimmed.starts_with("#[") {
                    pending_cfg_test = false;
                }
            }
        }
        let opens = line.matches('{').count() as i64;
        let closes = line.matches('}').count() as i64;
        let in_skip = depth_skip.is_some();
        depth += opens - closes;
        if let Some(d) = depth_skip
            && depth <= d
        {
            depth_skip = None;
            continue;
        }
        if in_skip {
            continue;
        }
        // Comment lines never fire (the needle would be prose).
        if trimmed.starts_with("//") {
            continue;
        }
        let hit = needles.iter().any(|n| {
            line.find(n.as_str()).is_some_and(|pos| {
                // Reject prose matches inside line comments.
                line.find("//").is_none_or(|c| pos < c)
            })
        });
        if !hit {
            continue;
        }
        // Classification: `timeout-census: <word>` on this line or the
        // three lines above (comment lane).
        let mut class = None;
        for j in (i.saturating_sub(3)..=i).rev() {
            if let Some(pos) = lines[j].find("timeout-census:") {
                let rest = &lines[j][pos + "timeout-census:".len()..];
                class = rest.split_whitespace().next().map(|w| {
                    w.trim_matches(|c: char| !c.is_ascii_alphabetic())
                        .to_owned()
                });
                break;
            }
        }
        out.push(Site {
            rel_path: format!("{rel}:{needle_context}", needle_context = i + 1),
            class,
        });
    }
}

/// Render the scan as the committed artifact body (scan section only).
fn render(sites: &[Site]) -> String {
    let mut s = String::new();
    for site in sites {
        let class = site.class.as_deref().unwrap_or("UNTAGGED");
        s.push_str(&format!("{}\t{}\n", site.rel_path, class));
    }
    s
}

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn snapshot_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/timeout_census.txt")
}

const SCAN_HEADER: &str = "# timeout-Err census — GENERATED by tests/timeout_census.rs (scan section).\n\
                           # Regenerate: cargo test -p rio-controller --test timeout_census -- --ignored regenerate_timeout_census\n\
                           # Columns: <src-relative path>:<line>\\t<class>\n";
const DECLARED_MARKER: &str =
    "# --- declared (cross-crate) rows below this line are NOT regenerated ---\n";

/// W9-AL main face: totality + closed vocabulary + zero-irreversible +
/// snapshot equality over the production tree.
#[test]
// r[verify sys.guard.brownout-only]
fn timeout_census_is_total_classified_and_frozen() {
    let sites = scan_pairs(CENSUS_SOURCES);
    assert!(
        !sites.is_empty(),
        "scanner found ZERO timeout sites in src/ — the generator is broken \
         (admin_call alone guarantees at least one)"
    );
    let untagged: Vec<_> = sites.iter().filter(|s| s.class.is_none()).collect();
    assert!(
        untagged.is_empty(),
        "untagged production timeout Err arms (add a `timeout-census: \
         delay|refusal` comment within 3 lines above the call, plus the \
         census[gen: rio-controller/tests/timeout_census.txt] tag):\n{untagged:#?}"
    );
    let bad_class: Vec<_> = sites
        .iter()
        .filter(|s| s.class.as_deref().is_some_and(|c| !CLASSES.contains(&c)))
        .collect();
    assert!(
        bad_class.is_empty(),
        "out-of-vocabulary consequence class (closed set: {CLASSES:?}):\n{bad_class:#?}"
    );
    let irreversible: Vec<_> = sites
        .iter()
        .filter(|s| s.class.as_deref() == Some("irreversible"))
        .collect();
    assert!(
        irreversible.is_empty(),
        "STOP-AND-ESCALATE: a timeout Err arm classified `irreversible` is a \
         D2 violation (guard expiry must produce only delay or refusal; \
         irreversible effects require positive evidence), NOT a census \
         entry. Do not reclassify — escalate via the wave log:\n{irreversible:#?}"
    );
    // Snapshot equality (scan section only; declared rows preserved).
    // The artifact is EMBEDDED (same commit, same bytes) so the
    // compare runs in the gate sandbox too.
    let committed = SNAPSHOT;
    let scan_section: String = committed
        .lines()
        .take_while(|l| !l.starts_with(DECLARED_MARKER.trim_end()))
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(
        scan_section,
        render(&sites),
        "tests/timeout_census.txt is stale — regenerate with\n\
         cargo test -p rio-controller --test timeout_census -- --ignored regenerate_timeout_census"
    );
    // Declared rows: every cross-crate row names a class from the
    // closed vocabulary and never claims irreversible.
    for l in committed
        .lines()
        .skip_while(|l| !l.starts_with(DECLARED_MARKER.trim_end()))
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let class = l.split('\t').nth(1).unwrap_or("");
        assert!(
            CLASSES.contains(&class) && class != "irreversible",
            "declared row with bad class: {l}"
        );
    }
}

/// Regenerator (the proto-fields precedent): rewrites the scan section,
/// preserves the declared (cross-crate) section verbatim.
#[test]
#[ignore = "regenerator — run explicitly to rewrite tests/timeout_census.txt"]
fn regenerate_timeout_census() {
    // Dev-only: scan the LIVE tree (cargo rebuilds this binary when
    // any embedded file changes, but a freshly ADDED src file is not
    // in CENSUS_SOURCES until the table is regenerated — the live
    // walk keeps the regenerator authoritative; the completeness pin
    // keeps the table honest).
    let root = crate_src();
    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    files.sort();
    let mut sites = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).expect("read source file");
        let rel = f
            .strip_prefix(&root)
            .expect("under root")
            .to_str()
            .expect("utf8 path")
            .replace('\\', "/");
        scan_file(&text, &rel, &mut sites);
    }
    sites.sort();
    let path = snapshot_path();
    let declared: String = std::fs::read_to_string(&path)
        .ok()
        .map(|old| {
            old.lines()
                .skip_while(|l| !l.starts_with(DECLARED_MARKER.trim_end()))
                .map(|l| format!("{l}\n"))
                .collect()
        })
        .unwrap_or_default();
    let mut out = String::from(SCAN_HEADER);
    out.push_str(&render(&sites));
    if declared.is_empty() {
        out.push_str(DECLARED_MARKER);
    } else {
        out.push_str(&declared);
    }
    std::fs::write(&path, out).expect("write snapshot");
}

// ---------------------------------------------------------------------------
// R22 corpus: each plant goes RED under the generator (asserted here),
// and the tree itself is clean (asserted above).
// ---------------------------------------------------------------------------

#[test]
fn corpus_alias_red_fires() {
    let sites = scan_pairs(&corpus_pairs("alias-red"));
    assert_eq!(
        sites.len(),
        1,
        "alias-form call must be detected: {sites:#?}"
    );
    assert!(
        sites[0].class.is_none(),
        "the alias plant is untagged BY DESIGN (the red): {sites:#?}"
    );
}

#[test]
fn corpus_scope_red_fires() {
    let sites = scan_pairs(&corpus_pairs("scope-red"));
    assert_eq!(
        sites.len(),
        1,
        "a site in a sibling-crate-shaped tree must be detected when the \
         root is handed to the generator: {sites:#?}"
    );
    assert!(
        sites[0].rel_path.starts_with("other-crate/src/"),
        "{sites:#?}"
    );
}

#[test]
fn corpus_label_red_fires() {
    let sites = scan_pairs(&corpus_pairs("label-red"));
    assert_eq!(sites.len(), 1, "{sites:#?}");
    let class = sites[0].class.as_deref().expect("tagged");
    assert!(
        !CLASSES.contains(&class),
        "the label plant must carry an OUT-of-vocabulary class: {class}"
    );
}

#[test]
fn corpus_cfgtest_green_excluded() {
    let sites = scan_pairs(&corpus_pairs("cfgtest-green"));
    assert!(
        sites.is_empty(),
        "cfg(test) sites are NOT production population: {sites:#?}"
    );
}

/// The S1/b870121ac completeness pin: the embedded universe equals the
/// live tree in BOTH directions — a new/removed/renamed src file or
/// corpus plant fails this on every dev run until the tables are
/// regenerated, so the census quantifier domain stays
/// generator-bounded. In the gate sandbox the live tree is absent by
/// design; the skip is DISCLOSED, never silent (the dev-tree run of
/// this same commit carries the pin).
#[test]
fn census_universe_matches_live_tree() {
    let src_root = crate_src();
    if !src_root.exists() {
        eprintln!(
            "src/ not on disk (nix sandbox): universe pinned by the \
             dev-tree run of this same commit"
        );
        return;
    }
    let mut live: Vec<String> = Vec::new();
    let mut files = Vec::new();
    collect_rs(&src_root, &mut files);
    for f in files {
        live.push(
            f.strip_prefix(&src_root)
                .expect("under root")
                .to_str()
                .expect("utf8 path")
                .replace('\\', "/"),
        );
    }
    live.sort();
    let mut embedded: Vec<String> = CENSUS_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
    embedded.sort();
    assert_eq!(
        embedded, live,
        "census universe drifted from the live tree: regenerate \
         CENSUS_SOURCES (sorted python walk of src/**.rs) so the \
         timeout census sees the whole crate in the nix sandbox too"
    );

    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/timeout_census_corpus");
    let mut live_corpus: Vec<String> = Vec::new();
    let mut cfiles = Vec::new();
    collect_rs(&corpus_root, &mut cfiles);
    for f in cfiles {
        live_corpus.push(
            f.strip_prefix(&corpus_root)
                .expect("under root")
                .to_str()
                .expect("utf8 path")
                .replace('\\', "/"),
        );
    }
    live_corpus.sort();
    let mut embedded_corpus: Vec<String> = CORPUS_SOURCES
        .iter()
        .map(|(a, r, _)| format!("{a}/{r}"))
        .collect();
    embedded_corpus.sort();
    assert_eq!(
        embedded_corpus, live_corpus,
        "corpus universe drifted from the live tree: regenerate \
         CORPUS_SOURCES (sorted python walk of tests/timeout_census_corpus)"
    );
}
