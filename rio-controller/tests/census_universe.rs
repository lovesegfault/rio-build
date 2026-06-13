//! The shared in-crate census universe + lexer primitives — ONE home
//! for the machine-generated source-embed table and the
//! production-region walk every rio-controller census consumes
//! (timeout_census.rs, term_decoder_census.rs). bug_002's lesson is
//! the law here: a second include_str! copy of the module tree is a
//! census-universe drift waiting to happen, so the table lives once
//! and the bidirectional live-tree pin in timeout_census.rs guards
//! THIS copy. Consumers import via `#[path = "census_universe.rs"]`
//! (tests/ files are separate binaries; the module is shared by
//! path, never duplicated).
#![allow(dead_code)]

/// EVERY `.rs` under `rio-controller/src`, embedded at compile time
/// (the S1/b870121ac CENSUS_SOURCES form). Machine-generated — sorted
/// (relpath, include_str!) pairs; the completeness pin
/// (`census_universe_matches_live_tree`) forces this table to track
/// the live tree exactly in both directions.
#[rustfmt::skip]
pub const CENSUS_SOURCES: &[(&str, &str)] = &[
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

/// Line-preserving comment/string strip — the shared lexer's walk
/// (nix/rust_strip.py) ported for the in-crate face: line comments and
/// NESTED block comments blanked; string bodies (plain/byte/raw/
/// byte-raw) and char/byte-char bodies blanked with exact escape-pair
/// stepping; delimiters kept (brace/quote parity for the structural
/// pass); newlines survive, so line numbers are stable. The parity
/// selftest (`strip_parity_with_shared_lexer_families`, in the
/// timeout-census consumer) pins the same token families the python
/// selftest pins.
pub fn strip_rust(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let n = b.len();
    let mut out: Vec<char> = b.clone();
    let blank = |o: &mut Vec<char>, a: usize, z: usize| {
        for c in o.iter_mut().take(z.min(n)).skip(a) {
            if *c != '\n' {
                *c = ' ';
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

/// Per-line production mask over a [`strip_rust`]-stripped source:
/// `false` for lines inside `#[cfg(test)]` items (including the
/// closing-brace line), `true` elsewhere — the EXACT walk
/// timeout_census.rs's scanner used inline (extracted verbatim so
/// both censuses share one derivation of "production code"; the
/// cfgtest-green corpus plant pins the behavior).
pub fn production_line_mask(stripped: &str) -> Vec<bool> {
    let lines: Vec<&str> = stripped.lines().collect();
    let mut mask = vec![true; lines.len()];
    let mut depth_skip: Option<i64> = None; // brace depth inside a cfg(test) block
    let mut pending_cfg_test = false;
    let mut depth: i64 = 0;
    for (i, line) in lines.iter().enumerate() {
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
            mask[i] = false;
            continue;
        }
        if in_skip {
            mask[i] = false;
        }
    }
    mask
}
