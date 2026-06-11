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
//! Generator evasion corpus (R22′ — plants DERIVE from the grammar's
//! production list, under `tests/timeout_census_corpus/`):
//! - the USE-GRAMMAR axes (bug_151): one plant per production of the
//!   import grammar — `use-plain-red/` (`use tokio::time::timeout;`),
//!   `alias-red/` (`… as <alias>`), `brace-group-red/`
//!   (`use tokio::time::{…, timeout as x, …}`, multi-line),
//!   `module-path-red/` (`use tokio::time;` + `time::timeout(…)`).
//!   `use_grammar_axis_total` iterates [`USE_GRAMMAR`]: a production
//!   row without a firing plant is itself a red, so the needle table
//!   cannot silently re-open one form at a time (the wave-9 scanner
//!   enabled needles for exactly 1 of 4 forms and the registry
//!   self-certified the axis closed).
//! - `string-brace-red/`: a `{` inside a string within a cfg(test)
//!   module must not extend the skip over production code — brace
//!   counting runs over comment/string-STRIPPED text (the shared
//!   lexer's semantics, ported below with a parity selftest; the old
//!   raw-line counting could swallow everything after a test module).
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
    ("brace-group-red", "braced.rs", include_str!("timeout_census_corpus/brace-group-red/braced.rs")),
    ("cfgtest-green", "with_tests.rs", include_str!("timeout_census_corpus/cfgtest-green/with_tests.rs")),
    ("label-red", "mislabeled.rs", include_str!("timeout_census_corpus/label-red/mislabeled.rs")),
    ("module-path-red", "modpath.rs", include_str!("timeout_census_corpus/module-path-red/modpath.rs")),
    ("scope-red", "other-crate/src/lib.rs", include_str!("timeout_census_corpus/scope-red/other-crate/src/lib.rs")),
    ("string-brace-red", "skew.rs", include_str!("timeout_census_corpus/string-brace-red/skew.rs")),
    ("use-plain-red", "plain.rs", include_str!("timeout_census_corpus/use-plain-red/plain.rs")),
];

/// The import-grammar PRODUCTION TABLE (bug_151, R22′): every form by
/// which `tokio::time::timeout` comes into scope, mapped to the corpus
/// axis that plants its red. Needle derivation ([`needles_for`]) and
/// plant totality ([`use_grammar_axis_total`]) both consume THIS table
/// — adding a production without a plant is a test red, so coverage
/// is computed from the grammar, never self-declared.
const USE_GRAMMAR: &[(&str, &str)] = &[
    ("use-plain", "use-plain-red"),
    ("use-as-rename", "alias-red"),
    ("use-brace-group", "brace-group-red"),
    ("use-module-path", "module-path-red"),
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

/// Line-preserving comment/string strip — the shared lexer's walk
/// (nix/rust_strip.py) ported for the in-crate face: line comments and
/// NESTED block comments blanked; string bodies (plain/byte/raw/
/// byte-raw) and char/byte-char bodies blanked with exact escape-pair
/// stepping; delimiters kept (brace/quote parity for the structural
/// pass); newlines survive, so line numbers are stable. The parity
/// selftest (`strip_parity_with_shared_lexer_families`) pins the same
/// token families the python selftest pins.
fn strip_rust(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let n = b.len();
    let mut out: Vec<char> = b.clone();
    let mut blank = |o: &mut Vec<char>, a: usize, z: usize| {
        for k in a..z.min(n) {
            if o[k] != '\n' {
                o[k] = ' ';
            }
        }
    };
    let raw_prefix_len = |i: usize| -> usize {
        let mut j = i;
        if j < n && b[j] == 'b' {
            j += 1;
        }
        if j >= n || b[j] != 'r' {
            return 0;
        }
        j += 1;
        while j < n && b[j] == '#' {
            j += 1;
        }
        if j < n && b[j] == '"' { j - i + 1 } else { 0 }
    };
    let mut i = 0;
    while i < n {
        let c = b[i];
        let nxt = if i + 1 < n { b[i + 1] } else { '\0' };
        if c == '/' && nxt == '/' {
            let mut j = i;
            while j < n && b[j] != '\n' {
                j += 1;
            }
            blank(&mut out, i, j);
            i = j;
        } else if c == '/' && nxt == '*' {
            let mut depth = 1i64;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if b[j] == '/' && j + 1 < n && b[j + 1] == '*' {
                    depth += 1;
                    j += 2;
                } else if b[j] == '*' && j + 1 < n && b[j + 1] == '/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if raw_prefix_len(i) > 0 {
            let plen = raw_prefix_len(i);
            let hashes = plen - (if b[i] == 'b' { 2 } else { 1 }) - 1;
            let mut j = i + plen;
            // find `"` + hashes `#`s
            let close_found = loop {
                if j >= n {
                    break n;
                }
                if b[j] == '"' && (j + 1..=j + hashes).all(|k| k < n && b[k] == '#') {
                    break j;
                }
                j += 1;
            };
            blank(&mut out, i + plen, close_found);
            i = if close_found >= n {
                n
            } else {
                close_found + 1 + hashes
            };
        } else if c == '"' || (c == 'b' && nxt == '"') {
            let start = i + if c == 'b' { 2 } else { 1 };
            let mut j = start;
            while j < n {
                if b[j] == '\\' {
                    j += 2;
                    continue;
                }
                if b[j] == '"' {
                    break;
                }
                j += 1;
            }
            blank(&mut out, start, j);
            i = (j + 1).min(n);
        } else if c == '\'' || (c == 'b' && nxt == '\'') {
            let q = if c == '\'' { i } else { i + 1 };
            let mut j = q + 1;
            if j < n && b[j] == '\\' {
                j += 2;
                while j < n && b[j] != '\'' {
                    if b[j] == '\\' {
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
            } else if j + 1 < n && b[j + 1] == '\'' {
                j += 1;
            } else {
                // Lifetime: untouched.
                i += 1;
                continue;
            }
            blank(&mut out, q + 1, j);
            i = (j + 1).min(n);
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// Expand a use-tree body (STRIPPED text, `;`-terminated item interior)
/// to `(leaf_path, alias)` rows — `tokio::{time::{timeout as t},
/// sync}` yields `("tokio::time::timeout", Some("t"))` and
/// `("tokio::sync", None)`.
fn expand_use_tree(prefix: &str, tree: &str, out: &mut Vec<(String, Option<String>)>) {
    // Split top-level commas.
    let mut depth = 0i64;
    let mut start = 0usize;
    let bytes = tree.as_bytes();
    let mut pieces: Vec<&str> = Vec::new();
    for (idx, &ch) in bytes.iter().enumerate() {
        match ch {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => {
                pieces.push(&tree[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    pieces.push(&tree[start..]);
    for piece in pieces {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(brace) = p.find('{') {
            // `head::{interior}` — recurse with the extended prefix.
            let head = p[..brace].trim().trim_end_matches("::").trim();
            let interior = p[brace + 1..].rsplit_once('}').map_or("", |(a, _)| a);
            let new_prefix = if prefix.is_empty() {
                head.to_string()
            } else if head.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}::{head}")
            };
            expand_use_tree(&new_prefix, interior, out);
        } else {
            let (path_part, alias) = match p.split_once(" as ") {
                Some((a, b)) => (a.trim(), Some(b.trim().to_string())),
                None => (p, None),
            };
            let full = if prefix.is_empty() {
                path_part.to_string()
            } else {
                format!("{prefix}::{path_part}")
            };
            out.push((full, alias));
        }
    }
}

/// Detection-needle derivation over the import grammar ([`USE_GRAMMAR`]
/// — bug_151): the fully-qualified call is always a needle; every
/// `use` item is expanded via [`expand_use_tree`], and a leaf reaching
/// `tokio::time::timeout`, `tokio::time`, or `tokio` enables the
/// bare/aliased, module-path, or crate-path needle respectively. All
/// four grammar productions (plain, as-rename, brace-group,
/// module-path) flow through the ONE resolver — there is no per-form
/// needle code to forget.
fn needles_for(stripped: &str) -> Vec<String> {
    let mut needles: Vec<String> = vec!["tokio::time::timeout(".into()];
    let mut leaves: Vec<(String, Option<String>)> = Vec::new();
    let chars: Vec<char> = stripped.chars().collect();
    let mut i = 0usize;
    let n = chars.len();
    while i < n {
        // Word-boundary `use` keyword.
        if stripped[i..].starts_with("use ")
            && (i == 0 || !chars[i - 1].is_alphanumeric() && chars[i - 1] != '_')
        {
            let mut j = i + 4;
            while j < n && chars[j] != ';' {
                j += 1;
            }
            let item: String = chars[i + 4..j.min(n)].iter().collect();
            let normalized: String = item.split_whitespace().collect::<Vec<_>>().join(" ");
            expand_use_tree("", &normalized, &mut leaves);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    for (path, alias) in leaves {
        match path.as_str() {
            "tokio::time::timeout" => {
                needles.push(format!("{}(", alias.as_deref().unwrap_or("timeout")));
            }
            "tokio::time" => {
                needles.push(format!("{}::timeout(", alias.as_deref().unwrap_or("time")));
            }
            "tokio" => {
                needles.push(format!(
                    "{}::time::timeout(",
                    alias.as_deref().unwrap_or("tokio")
                ));
            }
            _ => {}
        }
    }
    needles
}

/// Per-file scan. Needles derive from the import grammar
/// ([`needles_for`]); `#[cfg(test)]`-attributed blocks are excluded by
/// brace tracking over the comment/string-STRIPPED text (bug_151: raw
/// lines counted braces inside strings and comments, so a `{` in a
/// test-module string could extend the skip over production code).
/// Classification tags are read from the RAW lines (the comment lane).
fn scan_file(src: &str, rel: &str, out: &mut Vec<Site>) {
    let stripped = strip_rust(src);
    let needles = needles_for(&stripped);
    let raw_lines: Vec<&str> = src.lines().collect();
    let stripped_lines: Vec<&str> = stripped.lines().collect();
    let mut depth_skip: Option<i64> = None; // brace depth inside a cfg(test) block
    let mut pending_cfg_test = false;
    let mut depth: i64 = 0;
    for (i, line) in stripped_lines.iter().enumerate() {
        let trimmed = line.trim();
        if depth_skip.is_none() {
            if trimmed.starts_with("#[cfg(test)]") {
                // Attribute and opener on ONE line (`#[cfg(test)] mod t {`)
                // starts the skip immediately; otherwise it pends.
                if line.contains('{') {
                    depth_skip = Some(depth);
                } else {
                    pending_cfg_test = true;
                }
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
        // Needle hit on the STRIPPED line (comments/strings blanked),
        // word-boundary on the left so `my_timeout(` never fires.
        let hit = needles.iter().any(|nd| {
            let mut from = 0usize;
            while let Some(pos) = line[from..].find(nd.as_str()) {
                let abs = from + pos;
                let ok = abs == 0
                    || line[..abs]
                        .chars()
                        .next_back()
                        .is_none_or(|p| !p.is_alphanumeric() && p != '_' && p != ':');
                if ok {
                    return true;
                }
                from = abs + 1;
            }
            false
        });
        if !hit {
            continue;
        }
        // Classification: `timeout-census: <word>` on this line or the
        // three lines above (the COMMENT lane — read from raw lines).
        let mut class = None;
        for j in (i.saturating_sub(3)..=i).rev() {
            if let Some(pos) = raw_lines[j].find("timeout-census:") {
                let rest = &raw_lines[j][pos + "timeout-census:".len()..];
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

/// R22′ (bug_151): plant totality DERIVES from the grammar table —
/// every [`USE_GRAMMAR`] production has a corpus plant and the plant
/// FIRES (exactly one untagged site). Adding a production row without
/// a plant, or a plant the scanner cannot see, is a red here — the
/// registry cannot self-certify the axis closed (the covered/gap set
/// is this loop's output, not a declaration).
#[test]
fn use_grammar_axis_total() {
    for (production, axis) in USE_GRAMMAR {
        let pairs = corpus_pairs(axis);
        assert!(
            !pairs.is_empty(),
            "grammar production `{production}` has NO corpus plant under \
             axis `{axis}` — every import form gets a plant mechanically"
        );
        let sites = scan_pairs(&pairs);
        assert_eq!(
            sites.len(),
            1,
            "grammar production `{production}` plant must yield exactly one \
             site: {sites:#?}"
        );
        assert!(
            sites[0].class.is_none(),
            "the `{production}` plant is untagged BY DESIGN (the red): {sites:#?}"
        );
    }
}

/// bug_151 (brace integrity): a `{` inside a test-module STRING must
/// not extend the cfg(test) skip over the production site below —
/// red against the raw-line counter, which swallowed it.
#[test]
fn corpus_string_brace_red_fires() {
    let sites = scan_pairs(&corpus_pairs("string-brace-red"));
    assert_eq!(
        sites.len(),
        1,
        "the production site after the string-brace test module must be \
         detected: {sites:#?}"
    );
    assert!(sites[0].class.is_none(), "{sites:#?}");
}

/// The strip walk's parity selftest — the same token families
/// nix/rust_strip.py's selftest pins (escaped quotes, raw strings,
/// nested comments, newline preservation), so the in-crate port and
/// the shared lexer cannot silently diverge on the classes that
/// decide brace integrity.
#[test]
fn strip_parity_with_shared_lexer_families() {
    // Escaped-quote char soup: parity kept, bodies blanked.
    assert_eq!(strip_rust("let p = ('\\'','{');"), "let p = ('  ',' ');");
    // String bodies blanked, delimiters kept; braces in strings die.
    assert_eq!(
        strip_rust("let s = \"{ }\"; fn f() {}"),
        "let s = \"   \"; fn f() {}"
    );
    // Nested block comments fully blanked.
    let t = "a /* x /* y */ z */ b";
    let s = strip_rust(t);
    assert!(
        s.starts_with('a') && s.ends_with('b') && !s.contains("y"),
        "{s:?}"
    );
    // Raw strings with hashes: closer found, tail survives.
    let s = strip_rust("let r = r#\"a \" inside\"#; let after = 1;");
    assert!(s.contains("let after = 1;"), "{s:?}");
    assert!(!s.contains("inside"), "{s:?}");
    // Newlines survive blanking (line numbers stable).
    assert_eq!(strip_rust("\"one\ntwo\""), "\"   \n   \"");
    // Line comments blanked (to spaces — length preserved); code
    // before them kept.
    assert_eq!(strip_rust("x(); // tail { brace").trim_end(), "x();");
    // Lifetimes untouched.
    let s = strip_rust("fn f<'a>(x: &'a str) {}");
    assert_eq!(s.matches("'a").count(), 2, "{s:?}");
}

/// The use-tree resolver's own rows (the needle table's unit face):
/// nested groups, group-interior aliases, module aliases.
#[test]
fn needles_derive_from_every_grammar_production() {
    let plain = needles_for("use tokio::time::timeout;\n");
    assert!(plain.contains(&"timeout(".to_string()), "{plain:?}");
    let renamed = needles_for("use tokio::time::timeout as deadline;\n");
    assert!(renamed.contains(&"deadline(".to_string()), "{renamed:?}");
    let braced = needles_for("use tokio::time::{\n    sleep,\n    timeout as bounded,\n};\n");
    assert!(braced.contains(&"bounded(".to_string()), "{braced:?}");
    let modpath = needles_for("use tokio::time;\n");
    assert!(
        modpath.contains(&"time::timeout(".to_string()),
        "{modpath:?}"
    );
    let nested = needles_for("use tokio::{time::{timeout}, sync};\n");
    assert!(nested.contains(&"timeout(".to_string()), "{nested:?}");
    let mod_alias = needles_for("use tokio::time as t;\n");
    assert!(
        mod_alias.contains(&"t::timeout(".to_string()),
        "{mod_alias:?}"
    );
    let crate_alias = needles_for("use tokio as tk;\n");
    assert!(
        crate_alias.contains(&"tk::time::timeout(".to_string()),
        "{crate_alias:?}"
    );
    // Unrelated imports derive nothing beyond the qualified needle.
    let other = needles_for("use std::time::Duration;\n");
    assert_eq!(
        other,
        vec!["tokio::time::timeout(".to_string()],
        "{other:?}"
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
