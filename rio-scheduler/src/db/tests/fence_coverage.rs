//! Source-enumerating fence-coverage net (bug_269 / bug_273 class):
//! every write-verb SQL statement on a DECISION TABLE in the `db/`
//! production sources must live in a function that either (a) owns a
//! [`crate::db::FencedTx`] (constructs it via `begin_fenced`), (b) is
//! a `*_in_tx` body taking the caller's `&mut PgConnection` (the fence
//! lives at the transaction owner — every such owner is itself
//! enumerated by this test or constructs a `FencedTx`), or (c) sits on
//! the explicit allowlist below with a written rationale.
//!
//! This is a SOURCE test, not a runtime probe: it fails the moment a
//! new unfenced decision writer is introduced, naming the function.

/// Decision tables: rows that drive scheduling/lifecycle decisions.
const DECISION_TABLES: &[&str] = &[
    "assignments",
    "derivations",
    "materialization_jobs",
    "build_wanted_outputs",
    "drv_attempts",
];

/// (file, fn) pairs allowed to write decision tables without a fence,
/// each with its rationale.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "mod.rs",
        "close_assignment",
        "the FencedTx capability's own method: existence of &mut self \
         IS the fence proof (constructed only by begin_fenced)",
    ),
    (
        "assignments.rs",
        "insert_assignment",
        "cfg(test) fixture: seeds historical row shapes for db tests; \
         compiled out of production",
    ),
    (
        "assignments.rs",
        "update_assignment_status",
        "cfg(test) fixture: seeds historical row shapes for db tests; \
         compiled out of production",
    ),
    (
        "open_attempts.rs",
        "fill_open_execution_source_node",
        "NULL-only idempotent enrichment of drv_executions (not a \
         decision table) plus no decision-table verb; listed because \
         the fn name appears near assignment SQL in review — kept for \
         explicitness",
    ),
];

/// Files under `src/db/` that contain production SQL (tests excluded —
/// fixtures there exercise historical shapes deliberately).
const SOURCES: &[(&str, &str)] = &[
    ("mod.rs", include_str!("../mod.rs")),
    ("assignments.rs", include_str!("../assignments.rs")),
    ("attempts.rs", include_str!("../attempts.rs")),
    ("batch.rs", include_str!("../batch.rs")),
    ("builds.rs", include_str!("../builds.rs")),
    ("derivations.rs", include_str!("../derivations.rs")),
    ("executions.rs", include_str!("../executions.rs")),
    ("history.rs", include_str!("../history.rs")),
    ("live_pins.rs", include_str!("../live_pins.rs")),
    ("materialization.rs", include_str!("../materialization.rs")),
    ("open_attempts.rs", include_str!("../open_attempts.rs")),
    ("recovery.rs", include_str!("../recovery.rs")),
    ("tenants.rs", include_str!("../tenants.rs")),
    ("wanted.rs", include_str!("../wanted.rs")),
];

/// A write verb on one of the decision tables, inside a string literal.
fn writes_decision_table(line: &str) -> Option<&'static str> {
    let upper = line.to_uppercase();
    for table in DECISION_TABLES {
        let t_upper = table.to_uppercase();
        for verb in [
            format!("UPDATE {t_upper}"),
            format!("INSERT INTO {t_upper}"),
            format!("DELETE FROM {t_upper}"),
        ] {
            if upper.contains(&verb) {
                return Some(table);
            }
        }
    }
    None
}

/// The name of the enclosing `fn` for a given line index, plus the
/// body window from the fn declaration to the line.
fn enclosing_fn(lines: &[&str], idx: usize) -> Option<(String, usize)> {
    for back in (0..=idx).rev() {
        let l = lines[back].trim_start();
        if let Some(rest) = l
            .strip_prefix("pub async fn ")
            .or_else(|| l.strip_prefix("pub(crate) async fn "))
            .or_else(|| l.strip_prefix("pub(super) async fn "))
            .or_else(|| l.strip_prefix("async fn "))
            .or_else(|| l.strip_prefix("pub fn "))
            .or_else(|| l.strip_prefix("pub(crate) fn "))
            .or_else(|| l.strip_prefix("fn "))
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return Some((name, back));
        }
    }
    None
}

#[test]
fn every_decision_table_writer_is_fenced_or_allowlisted() {
    let mut violations = Vec::new();
    for (file, src) in SOURCES {
        let lines: Vec<&str> = src.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            // Only string-literal SQL: require a quote on the line.
            if !line.contains('"') && !line.contains("r#\"") {
                continue;
            }
            let Some(table) = writes_decision_table(line) else {
                continue;
            };
            let Some((fn_name, fn_idx)) = enclosing_fn(&lines, idx) else {
                violations.push(format!(
                    "{file}:{}: decision-table write ({table}) outside any fn",
                    idx + 1
                ));
                continue;
            };
            if ALLOWLIST
                .iter()
                .any(|(f, name, _)| f == file && *name == fn_name)
            {
                continue;
            }
            // (b) `*_in_tx` body or a fn taking the caller's connection:
            // the fence lives at the transaction owner.
            let header_window = lines[fn_idx..(fn_idx + 12).min(lines.len())].join("\n");
            if fn_name.ends_with("_in_tx")
                || header_window.contains("&mut PgConnection")
                || header_window.contains("tx: &mut sqlx::PgConnection")
            {
                continue;
            }
            // (a) constructs the capability in its body window.
            let body_window = lines[fn_idx..(idx + 1)].join("\n");
            if body_window.contains("begin_fenced(") || body_window.contains("FencedTx") {
                continue;
            }
            violations.push(format!(
                "{file}:{}: fn `{fn_name}` writes decision table `{table}` without the \
                 FencedTx capability (use begin_fenced / a *_in_tx body / the allowlist \
                 with rationale)",
                idx + 1
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "unfenced decision-table writers found:\n{}",
        violations.join("\n")
    );
}

/// The canonical claims-floor SQL exists in exactly the two sanctioned
/// homes: `db/mod.rs` (the capability) and `db/recovery.rs`
/// (`max_known_generation`, the pool-seeding read — a READ, not a
/// fence). bug_269's textual net.
#[test]
fn fence_sql_canonical_homes_only() {
    let mut offenders = Vec::new();
    for (file, src) in SOURCES {
        let hits = src
            .lines()
            .filter(|l| l.contains("GREATEST(") && l.to_uppercase().contains("MAX(GENERATION)"))
            .count();
        // The literal spans two lines in the canonical form; count any
        // GREATEST whose file also mentions leader_generation_claims.
        let has_floor_shape = src.contains("leader_generation_claims") && hits > 0
            || (src.contains("GREATEST(")
                && src.contains("(SELECT MAX(generation) FROM assignments)"));
        if has_floor_shape && !matches!(*file, "mod.rs" | "recovery.rs") {
            offenders.push(*file);
        }
    }
    assert!(
        offenders.is_empty(),
        "claims-floor GREATEST SQL outside db/mod.rs + db/recovery.rs: {offenders:?} \
         (open-coded floors are bug_269's class — use begin_fenced)"
    );
}

/// Actor sources for the tenure-authority net (merged_bug_338 class).
const ACTOR_SOURCES: &[(&str, &str)] = &[
    ("mod.rs", include_str!("../../actor/mod.rs")),
    (
        "housekeeping.rs",
        include_str!("../../actor/housekeeping.rs"),
    ),
    ("pull.rs", include_str!("../../actor/pull.rs")),
    ("recovery.rs", include_str!("../../actor/recovery.rs")),
    ("completion.rs", include_str!("../../actor/completion.rs")),
    ("materialize.rs", include_str!("../../actor/materialize.rs")),
    ("merge.rs", include_str!("../../actor/merge.rs")),
    ("build.rs", include_str!("../../actor/build.rs")),
    ("dispatch.rs", include_str!("../../actor/dispatch.rs")),
    ("snapshot.rs", include_str!("../../actor/snapshot.rs")),
    ("executor.rs", include_str!("../../actor/executor.rs")),
    ("event.rs", include_str!("../../actor/event.rs")),
];

/// Tenure authority is single-sourced (merged_bug_338): write paths
/// must use the claim-stamped `self.serving_generation`
/// ([`crate::db::ServingGeneration`]) — NEVER a fresh
/// `leader.generation()` atomic read, which can advance mid-mailbox on
/// a lease re-acquire and stamp evidence with a tenure the actor never
/// recovered under. Allowed reads of the live atomic, by exact census:
///
/// - `mod.rs`: 1 — the constructor's boot stamp (feeds
///   `ServingGeneration::stamp_from_claim`; there is no claim yet).
/// - `recovery.rs`: 3 — TOCTOU gate snapshots (read-only generation
///   COMPARISONS guarding recovery, not write stamps).
///
/// Any other occurrence — even one that plumbs the constructor — is a
/// fresh-atomic-read regression and fails here by name.
#[test]
fn tenure_authority_no_fresh_atomic_reads_in_write_paths() {
    const ALLOWED: &[(&str, usize)] = &[("mod.rs", 1), ("recovery.rs", 3)];
    let mut offenders = Vec::new();
    for (file, src) in ACTOR_SOURCES {
        let code_hits: Vec<(usize, &str)> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                !t.starts_with("//") && l.contains("leader.generation()")
            })
            .map(|(i, l)| (i + 1, l.trim()))
            .collect();
        let allowed = ALLOWED
            .iter()
            .find(|(f, _)| f == file)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if code_hits.len() != allowed {
            offenders.push(format!(
                "{file}: {} fresh leader.generation() reads (allowed {allowed}): {:?}",
                code_hits.len(),
                code_hits
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "fresh lease-atomic reads outside the tenure-stamp census \
         (merged_bug_338 class — use self.serving_generation):\n{}",
        offenders.join("\n")
    );
}

/// The `ServingGeneration` stamp has exactly TWO production
/// constructors: the boot stamp (`DagActor::new`) and the
/// claim stamp (`handle_leader_acquired`). A third
/// `stamp_from_claim` call site means a new tenure-authority seam —
/// review it against `sched.lease.tenure-stamp-type`.
#[test]
fn tenure_stamp_exactly_two_production_sites() {
    let count: usize = ACTOR_SOURCES
        .iter()
        .map(|(_, src)| {
            src.lines()
                .filter(|l| {
                    let t = l.trim_start();
                    !t.starts_with("//") && l.contains("stamp_from_claim(")
                })
                .count()
        })
        .sum();
    assert_eq!(
        count, 2,
        "ServingGeneration::stamp_from_claim must have exactly two \
         production call sites (boot stamp + claim stamp), found {count}"
    );
}
