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

// r[verify sched.db.exec-stamp-on-close]
/// bug_047's structural net: every production `UPDATE assignments`
/// statement renders through `close_assignments_sql` (db/mod.rs),
/// whose CTE pair stamps the closed rows' `drv_executions` status in
/// the same statement. An open-coded assignment UPDATE can close a row
/// WITHOUT the stamp — the immortal-exec-row class (`status` stays
/// NULL, the terminality conjunct never matches, `gc_exec_rows` never
/// collects). The cfg(test) fixtures in `assignments.rs` are exempt:
/// seeding historical row shapes is their documented purpose.
#[test]
fn closer_statements_render_through_close_assignments_sql() {
    const SANCTIONED: &[(&str, &str)] = &[
        ("mod.rs", "close_assignments_sql"),
        ("assignments.rs", "update_assignment_status"),
    ];
    let mut violations = Vec::new();
    for (file, src) in SOURCES {
        let lines: Vec<&str> = src.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            // Comments may NAME the statement; only code may render it.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if !line.to_uppercase().contains("UPDATE ASSIGNMENTS") {
                continue;
            }
            let fn_name = enclosing_fn(&lines, idx)
                .map(|(n, _)| n)
                .unwrap_or_default();
            if SANCTIONED.iter().any(|(f, n)| f == file && *n == fn_name) {
                continue;
            }
            violations.push(format!(
                "{file}:{}: `UPDATE assignments` in fn `{fn_name}` — production \
                 closers must render via close_assignments_sql (the close+stamp \
                 CTE family); an open-coded close skips the drv_executions stamp \
                 (immortal exec rows, bug_047)",
                idx + 1
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "open-coded assignment closers:\n{}",
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

/// The `DagAuthority` witness (merged_bug_210) has exactly ONE
/// production mint: `DagActor::dag_authority`, which reads
/// `dag_authoritative`. A second construction expression means a
/// destructive path acquired authority without the bit — review it
/// against `sched.attempt.establishment-window+6`.
#[test]
fn dag_authority_single_mint_site() {
    let count: usize = ACTOR_SOURCES
        .iter()
        .map(|(_, src)| {
            src.lines()
                .filter(|l| {
                    let t = l.trim_start();
                    // The tuple-struct DECLARATION also contains the
                    // `DagAuthority(())` token sequence — only value
                    // constructions count.
                    !t.starts_with("//") && !t.contains("struct ") && l.contains("DagAuthority(())")
                })
                .count()
        })
        .sum();
    assert_eq!(
        count, 1,
        "DagAuthority(()) must be constructed exactly once (DagActor::dag_authority), found {count}"
    );
}

/// The absolute batch status writer is FRESH-WRITE ONLY
/// (merged_bug_011): `update_derivation_status_batch`'s absolute
/// UPDATE + derivation-scoped close are sound exactly at the
/// in-memory transition. Census of allowed callers:
///
/// - `completion.rs`: 1 — `persist_status_batch` (the at-transition
///   writer; its FAILURE latches into the outbox).
/// - `merge.rs`: 2 — the merge tail's reset/lane persists (statuses
///   the merge just decided, same transaction epoch).
///
/// `housekeeping.rs` MUST be 0: the outbox flush re-drives through
/// `replay_status_batch_guarded` (flush-time re-derivation +
/// exec-scoped close). A new absolute caller anywhere else is the
/// stale-replay regression class reopening — route it here by name.
#[test]
fn absolute_status_batch_writer_callers_pinned() {
    const ALLOWED: &[(&str, usize)] = &[("completion.rs", 1), ("merge.rs", 2)];
    let mut offenders = Vec::new();
    for (file, src) in ACTOR_SOURCES {
        let hits = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && l.contains(".update_derivation_status_batch(")
            })
            .count();
        let allowed = ALLOWED
            .iter()
            .find(|(f, _)| f == file)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if hits != allowed {
            offenders.push(format!(
                "{file}: {hits} absolute batch-writer calls (allowed {allowed})"
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "absolute status-batch writer outside the fresh-write census \
         (merged_bug_011 class — re-drives use replay_status_batch_guarded):\n{}",
        offenders.join("\n")
    );
}

/// The status writers the source scan below derives — the runtime
/// advance/stasis pair in `db/tests/derivations.rs` (the
/// value-changing biconditional AND the value-preserving stasis test)
/// consumes EXACTLY this list (each per-writer match panics on an
/// entry it does not drive), so a new status writer fails the census
/// here until classified AND fails the runtime tests until driven.
/// The GENERATOR is the scan in [`derivations_status_stamp_census`];
/// this const is its pinned output, asserted equal every run.
pub(crate) const STATUS_WRITER_FNS: &[&str] = &[
    "clear_poison_batch_in_tx",
    "clear_poison_in_tx",
    "persist_poisoned_in_tx",
    "replay_status_batch_guarded",
    "update_derivation_status_batch_in_tx",
    "update_derivation_status_in_tx",
];

/// Production writers that touch `derivations` WITHOUT setting
/// `status` — the comparand-purity law's other half: their SET lists
/// must NEVER name `status_changed_at`. Same generator/pin contract
/// as [`STATUS_WRITER_FNS`].
pub(crate) const NON_STATUS_DERIVATIONS_WRITER_FNS: &[&str] =
    &["batch_upsert_derivations", "update_resource_floor"];

/// Every production source the stamp census scans: the COMPILE-TIME
/// embedded [`SOURCES`] (db/) + [`ACTOR_SOURCES`] (actor/) lists —
/// the same embedding every fence test in this file uses, because
/// the nix gate runs the test binary without the source tree on
/// disk (a runtime walk fails there with NotFound; observed live at
/// the bughunt-6 gate). Completeness of the embedded lists against
/// the live tree is pinned by
/// [`derivations_sql_confined_to_embedded_sources`], which walks the
/// real `src/` whenever it exists (every dev-tree `cargo nextest`
/// run of the same commit).
fn production_rs_sources() -> Vec<(String, &'static str)> {
    SOURCES
        .iter()
        .map(|(f, s)| (format!("db/{f}"), *s))
        .chain(
            ACTOR_SOURCES
                .iter()
                .map(|(f, s)| (format!("actor/{f}"), *s)),
        )
        .collect()
}

/// Dev-tree completeness pin for the census's file universe: any
/// production `.rs` under `src/` that names a `derivations` write
/// verb MUST be one of the embedded census sources (db/ in
/// [`SOURCES`], actor/ in [`ACTOR_SOURCES`]). Walks the real tree
/// when present; in the nix sandbox (no source dir) the embedded
/// scan is the same commit's content, so the dev-run enforcement
/// covers the identical tree — the skip is disclosed, not silent.
#[test]
fn derivations_sql_confined_to_embedded_sources() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !root.exists() {
        eprintln!(
            "src/ not on disk (nix sandbox): completeness pinned by the \
             dev-tree run of this same commit"
        );
        return;
    }
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable src dir") {
            let entry = entry.expect("readable dir entry");
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name == "tests" {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") && name != "tests.rs" {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(
        files.iter().any(|p| p.ends_with("db/derivations.rs")),
        "walk must reach the known writer file (src layout moved?)"
    );
    let mut violations = Vec::new();
    for path in files {
        let src = std::fs::read_to_string(&path).expect("readable source");
        let has_write = src.lines().any(|l| {
            let t = l.trim_start();
            if t.starts_with("//") || t.starts_with("--") {
                return false;
            }
            let u = l.to_uppercase();
            u.contains("UPDATE DERIVATIONS") || u.contains("INSERT INTO DERIVATIONS")
        });
        if !has_write {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let embedded = (path.parent().is_some_and(|d| d.ends_with("db"))
            && SOURCES.iter().any(|(f, _)| *f == name))
            || (path.parent().is_some_and(|d| d.ends_with("actor"))
                && ACTOR_SOURCES.iter().any(|(f, _)| *f == name));
        if !embedded {
            violations.push(format!(
                "{}: writes derivations but is not in the embedded census \
                 sources (add it to SOURCES/ACTOR_SOURCES so the stamp \
                 census sees it in the nix sandbox too)",
                path.display()
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "derivations SQL outside the embedded census universe:\n{}",
        violations.join("\n")
    );
}

/// One scanned `derivations` write statement: where it is, what kind,
/// and which columns its SET list (or INSERT column list) names.
struct DerivationsWrite {
    file: String,
    line: usize,
    fn_name: String,
    /// `UPDATE` SET list or `ON CONFLICT … DO UPDATE SET` list
    /// (`None` for a plain INSERT — its columns ride `insert_cols`).
    set_cols: Option<Vec<String>>,
    /// INSERT column list (`None` for UPDATEs).
    insert_cols: Option<Vec<String>>,
}

/// Column names on the LHS of `lhs = rhs` assignments in a SET list.
fn set_list_columns(set_list: &str) -> Vec<String> {
    set_list
        .split(',')
        .filter_map(|assign| assign.split_once('='))
        .map(|(lhs, _)| {
            lhs.trim()
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_string()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Scan a source file for `UPDATE derivations` / `INSERT INTO
/// derivations` statements and parse the column sets the law
/// quantifies over. Statement text is captured across lines (the
/// `query!` raw-string forms put the verb and the SET list on
/// different lines — a quote heuristic would silently skip them).
fn derivations_writes(file: &str, src: &str) -> Vec<DerivationsWrite> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("--") {
            continue;
        }
        let upper = line.to_uppercase();
        let is_update = upper.contains("UPDATE DERIVATIONS");
        let is_insert = upper.contains("INSERT INTO DERIVATIONS");
        if !is_update && !is_insert {
            continue;
        }
        // The statement window: this line plus the rest of the SQL
        // string (raw strings span many lines; 80 is comfortably past
        // the longest production statement).
        let window = lines[idx..(idx + 80).min(lines.len())].join("\n");
        let upper_window = window.to_uppercase();
        let fn_name = enclosing_fn(&lines, idx)
            .map(|(n, _)| n)
            .unwrap_or_else(|| format!("<no enclosing fn at {file}:{}>", idx + 1));
        if is_update {
            let set_at = upper_window.find("SET").unwrap_or(0);
            let end = upper_window[set_at..]
                .find("WHERE")
                .map(|w| set_at + w)
                .unwrap_or(upper_window.len());
            out.push(DerivationsWrite {
                file: file.to_string(),
                line: idx + 1,
                fn_name,
                set_cols: Some(set_list_columns(&window[set_at + 3..end])),
                insert_cols: None,
            });
        } else {
            // INSERT: column list between the first '(' and its ')'.
            let cols = window
                .find('(')
                .and_then(|open| window[open..].find(')').map(|close| (open, open + close)))
                .map(|(open, close)| {
                    window[open + 1..close]
                        .split(',')
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            // An upsert's DO UPDATE SET list gets the UPDATE law.
            let set_cols = upper_window.find("DO UPDATE SET").map(|s| {
                let after = s + "DO UPDATE SET".len();
                let end = ["WHERE", "RETURNING"]
                    .iter()
                    .filter_map(|t| upper_window[after..].find(t))
                    .min()
                    .map(|e| after + e)
                    .unwrap_or(upper_window.len());
                set_list_columns(&window[after..end])
            });
            out.push(DerivationsWrite {
                file: file.to_string(),
                line: idx + 1,
                fn_name,
                set_cols,
                insert_cols: Some(cols),
            });
        }
    }
    out
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_004/merged_bug_006: the comparand-purity census, in its
/// post-102 TOTAL-BAN form. PROPOSITION CERTIFIED: NO production
/// `derivations` statement names `status_changed_at` in any SET list,
/// `DO UPDATE SET` list, or INSERT column list — the migration 102
/// BEFORE UPDATE trigger is the column's single authority (stamping
/// IFF the status VALUE changes), so the single-authority law is
/// checkable as the ABSENCE of any client-side writer; fresh rows
/// ride the migration 101 DEFAULT. This is the precedence law's
/// quantification premise: the replay conjunct cuts on a column
/// writable solely by status VALUE-change events. The law is TOTAL —
/// no allowlist; the scan-derived writer sets stay pinned to the
/// consts the runtime stasis/advance tests drive.
#[test]
fn derivations_status_stamp_census() {
    let mut violations = Vec::new();
    let mut status_writers = std::collections::BTreeSet::new();
    let mut non_status_writers = std::collections::BTreeSet::new();
    for (file, src) in production_rs_sources() {
        for w in derivations_writes(&file, src) {
            let names = |cols: &Option<Vec<String>>, col: &str| {
                cols.as_ref().is_some_and(|cs| cs.iter().any(|c| c == col))
            };
            let set_status = names(&w.set_cols, "status");
            if names(&w.set_cols, "status_changed_at") {
                violations.push(format!(
                    "{}:{}: fn `{}` names `status_changed_at` in a SET/DO-UPDATE list \
                     (the migration 102 trigger is the stamp's single authority — \
                     no client statement may write the comparand)",
                    w.file, w.line, w.fn_name
                ));
            }
            if names(&w.insert_cols, "status_changed_at") {
                violations.push(format!(
                    "{}:{}: fn `{}` INSERTs `status_changed_at` (fresh rows ride \
                     the migration 101 DEFAULT; the comparand has no client-side \
                     writer)",
                    w.file, w.line, w.fn_name
                ));
            }
            if set_status {
                status_writers.insert(w.fn_name.clone());
            } else if w.set_cols.is_some() || w.insert_cols.is_some() {
                non_status_writers.insert(w.fn_name.clone());
            }
        }
    }
    assert!(
        violations.is_empty(),
        "status_changed_at single-authority violations (merged_bug_006):\n{}",
        violations.join("\n")
    );
    // Pin the derived writer sets to the consts the runtime
    // biconditional drives — drift in EITHER direction fails.
    assert_eq!(
        status_writers.iter().cloned().collect::<Vec<_>>(),
        STATUS_WRITER_FNS,
        "scan-derived status-writer set drifted from STATUS_WRITER_FNS \
         (update the const AND drive the new writer in \
         status_writers_stamp_status_changed_at_biconditional)"
    );
    assert_eq!(
        non_status_writers.iter().cloned().collect::<Vec<_>>(),
        NON_STATUS_DERIVATIONS_WRITER_FNS,
        "scan-derived non-status-writer set drifted from \
         NON_STATUS_DERIVATIONS_WRITER_FNS"
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
