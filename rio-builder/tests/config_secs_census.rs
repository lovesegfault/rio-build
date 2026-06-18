//! W10-BH (bug_117): the builder Config seconds-seam census.
//!
//! Every `Config` seconds field's path to a `Duration` is enumerated
//! here, and each path must run through a clamped type — either the
//! `rio_common::config::secs_bounded` adapter (ceiling at parse) or
//! the `rio_common::config::BoundedSecs` field type (ceiling carried
//! by the type). The RAW `rio_common::config::secs` adapter is
//! REFUSED in this crate: it deserializes verbatim, and a verbatim
//! config lane is exactly how bug_117's `u64::MAX` "disable the
//! timeout" sentinel reached the stderr-loop `Instant + Duration`
//! deadline add and panicked fleet-wide (the wire lane had been
//! clamped in wave-6; the config lane was the second missed lane —
//! the Nth-strike rule moved the clamp into the type).
//!
//! [GEN-SET]: the universe is derived from the raw crate source at
//! compile time (`include_str!` — self-fresh, re-read on every build,
//! no snapshot to stale). Scope: `rio-builder/src/config.rs` (the
//! builder `Config` surface; `CommonConfig`'s flattened fields live in
//! rio-common and are owned by its own tests).
//!
//! R22′: the census's coverage is the complement of its own refusal
//! predicate, and the planted-red corpus enters at the OUTERMOST
//! layer (raw source text, not post-extraction fixtures).

const CONFIG_SRC: &str = include_str!("../src/config.rs");

/// The refusal predicate: lines that bind a serde field to the RAW
/// (unbounded) seconds adapter. The closing quote excludes the
/// `secs_bounded` variant.
fn raw_secs_violations(src: &str) -> Vec<usize> {
    src.lines()
        .enumerate()
        .filter(|(_, l)| l.contains(r#"with = "rio_common::config::secs""#))
        .map(|(i, _)| i + 1)
        .collect()
}

/// Enumerate `( serde key, clamp mechanism )` for every seconds-keyed
/// field: the mechanism is the bounded with-adapter named on the
/// attribute line, the raw adapter (a violation), or the field's own
/// type on the following declaration line.
fn secs_field_rows(src: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut rows = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(key) = l
            .split(r#"rename = ""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
        else {
            continue;
        };
        if !key.ends_with("_secs") {
            continue;
        }
        // The serde attribute may be rustfmt-wrapped across lines:
        // join from the rename line through the attribute's closing
        // `)]` before looking for the with-adapter.
        let mut attr = String::new();
        for m in &lines[i..] {
            attr.push_str(m);
            attr.push('\n');
            if m.trim_end().ends_with(")]") {
                break;
            }
        }
        let mechanism = if attr.contains(r#"with = "rio_common::config::secs_bounded""#) {
            "secs_bounded".to_string()
        } else if attr.contains(r#"with = "rio_common::config::secs""#) {
            "RAW-secs".to_string()
        } else {
            // Field declaration follows the attribute stack: skip
            // remaining attributes until the `pub name: Type,` line.
            lines[i + 1..]
                .iter()
                .take(8)
                .find_map(|m| {
                    let m = m.trim();
                    m.strip_prefix("pub ")
                        .and_then(|rest| rest.split_once(':'))
                        .map(|(_, ty)| ty.trim().trim_end_matches(',').to_string())
                })
                .unwrap_or_else(|| "UNRESOLVED".to_string())
        };
        rows.push((key.to_string(), mechanism));
    }
    rows
}

/// The committed enumeration: four seconds fields, each through a
/// clamped type. Set-equality is the bidirectional pin — a new
/// `_secs` field cannot land without joining this table (and the
/// refusal predicate forces it onto a bounded path).
#[test]
fn every_config_seconds_field_paths_through_a_clamped_type() {
    assert_eq!(
        raw_secs_violations(CONFIG_SRC),
        Vec::<usize>::new(),
        "raw `rio_common::config::secs` adapter found in builder \
         config — every seconds lane must parse bounded (bug_117)"
    );

    let rows = secs_field_rows(CONFIG_SRC);
    let expect = [
        ("dag_prefetch_timeout_secs", "secs_bounded"),
        ("fuse_fetch_timeout_secs", "secs_bounded"),
        ("daemon_timeout_secs", "rio_common::config::BoundedSecs"),
        ("max_silent_time_secs", "secs_bounded"),
        ("idle_secs", "secs_bounded"),
    ];
    assert_eq!(
        rows,
        expect
            .iter()
            .map(|(k, m)| (k.to_string(), m.to_string()))
            .collect::<Vec<_>>(),
        "the Config seconds-field census drifted — enumerate the new \
         field's path here and route it through a clamped type"
    );
}

/// R22′ planted red: a strawman field on the RAW adapter, entered at
/// the outermost derivation layer (raw source text). The refusal
/// predicate must fire on it — this is the census's own
/// evasion-axis plant, and it is the pre-fix shape verbatim (all four
/// live fields parsed through the raw adapter before this close).
#[test]
fn census_plant_raw_secs_field_is_refused() {
    const PLANT_RAW_SECS: &str = r##"
    /// strawman: a config seconds field on the RAW (unbounded) adapter
    #[serde(rename = "strawman_timeout_secs", with = "rio_common::config::secs")]
    #[schemars(with = "u64")]
    pub strawman_timeout: std::time::Duration,
"##;
    assert_eq!(
        raw_secs_violations(PLANT_RAW_SECS).len(),
        1,
        "the planted raw-secs field must be refused by the census"
    );
    assert_eq!(
        secs_field_rows(PLANT_RAW_SECS),
        vec![("strawman_timeout_secs".to_string(), "RAW-secs".to_string())],
        "the plant's mechanism column must name the violation"
    );
}
