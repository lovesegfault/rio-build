//! W10-BN (merged_bug_063): the wanted-width resolver census.
//!
//! Name-to-path resolution over the (`output_names` ↔
//! `expected_output_paths`) parallel arrays is the anchored contract's
//! fourth leg: it MUST route through
//! `rio_common::wanted_outputs::verifiable_wanted_paths` (None on
//! skew / any wanted-matched placeholder), never an open-coded zip —
//! the open-coded form is exactly how the store leg silently forked
//! the documented same-width contract while the scheduler leg
//! refused.
//!
//! [GEN-SET]: the universe is this crate's materialize plane, embedded
//! at compile time (`include_str!` — self-fresh, no snapshot to
//! stale). Scope: rio-store's materialize sources (the leg that
//! drifted); the scheduler leg routes through the guard and is held
//! by its own agreement census; the gateway's direct iteration is
//! sanctioned by construction (its `unzip` keeps the arrays paired)
//! and lives outside this crate.
//!
//! R22′: coverage is the complement of the refusal predicate below;
//! the planted-red corpus enters at the outermost layer (raw source
//! text — the pre-fix zip verbatim).

const EXECUTOR_SRC: &str = include_str!("../src/materialize/executor.rs");
const CLIENT_SRC: &str = include_str!("../src/materialize/client.rs");

/// The refusal predicate: a `.zip(` whose 3-line window also touches
/// the expected-paths array — the open-coded resolution shape.
fn open_coded_zip_violations(src: &str) -> Vec<usize> {
    let lines: Vec<&str> = src.lines().collect();
    let mut hits = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !l.contains(".zip(") || l.trim_start().starts_with("//") {
            continue;
        }
        let window = lines[i.saturating_sub(2)..(i + 3).min(lines.len())].join("\n");
        if window.contains("expected_paths") || window.contains("expected_output_paths") {
            hits.push(i + 1);
        }
    }
    hits
}

/// The census: zero open-coded zips in the materialize plane, and the
/// sanctioned guard call still present (bidirectional — deleting the
/// resolution entirely would also fail).
#[test]
fn materialize_plane_has_zero_open_coded_wanted_zips() {
    for (rel, src) in [
        ("src/materialize/executor.rs", EXECUTOR_SRC),
        ("src/materialize/client.rs", CLIENT_SRC),
    ] {
        assert_eq!(
            open_coded_zip_violations(src),
            Vec::<usize>::new(),
            "{rel}: open-coded name-to-path zip — route through \
             rio_common::wanted_outputs::verifiable_wanted_paths \
             (the anchored same-width contract, merged_bug_063)"
        );
    }
    assert!(
        EXECUTOR_SRC.contains("wanted_outputs::verifiable_wanted_paths"),
        "the resolution must still route through the shared guard \
         (the census is bidirectional)"
    );
}

/// R22′ planted red: the pre-fix open-coded zip, verbatim, entered at
/// the outermost layer (raw source text). The refusal predicate must
/// fire on it.
#[test]
fn census_plant_open_coded_zip_is_refused() {
    const PLANT: &str = r#"
            output_names
                .iter()
                .zip(expected_paths.iter())
                .filter(|(name, path)| (all || names.contains(*name)) && !path.is_empty())
                .map(|(_, path)| path.clone())
                .collect()
"#;
    assert_eq!(
        open_coded_zip_violations(PLANT).len(),
        1,
        "the planted pre-fix zip must be refused by the census"
    );
}
