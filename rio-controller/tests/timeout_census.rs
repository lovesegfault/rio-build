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

/// Scan one root for production `tokio::time::timeout` call sites and
/// their classifications. Pure function of the tree: the committed
/// snapshot, the corpus reds, and the production run all go through
/// here.
fn scan(root: &Path) -> Vec<Site> {
    let mut files = Vec::new();
    collect_rs(root, &mut files);
    files.sort();
    let mut out = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read source file");
        let rel = f
            .strip_prefix(root)
            .expect("under root")
            .to_str()
            .expect("utf8 path")
            .replace('\\', "/");
        scan_file(&src, &rel, &mut out);
    }
    out.sort();
    out
}

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
fn timeout_census_is_total_classified_and_frozen() {
    let sites = scan(&crate_src());
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
    let committed = std::fs::read_to_string(snapshot_path())
        .expect("tests/timeout_census.txt missing — run the regenerator");
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
    let sites = scan(&crate_src());
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

fn corpus(axis: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/timeout_census_corpus")
        .join(axis)
}

#[test]
fn corpus_alias_red_fires() {
    let sites = scan(&corpus("alias-red"));
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
    let sites = scan(&corpus("scope-red"));
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
    let sites = scan(&corpus("label-red"));
    assert_eq!(sites.len(), 1, "{sites:#?}");
    let class = sites[0].class.as_deref().expect("tagged");
    assert!(
        !CLASSES.contains(&class),
        "the label plant must carry an OUT-of-vocabulary class: {class}"
    );
}

#[test]
fn corpus_cfgtest_green_excluded() {
    let sites = scan(&corpus("cfgtest-green"));
    assert!(
        sites.is_empty(),
        "cfg(test) sites are NOT production population: {sites:#?}"
    );
}
