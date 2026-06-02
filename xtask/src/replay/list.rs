//! `cargo xtask replay list` — list the replay archive prefixes in S3.
//!
//! One row per archive prefix under `replay/archives/` in the chunk
//! bucket, newest first. "Published" means the prefix carries a usable
//! `complete.json` (the recorder uploads it last): candidacy is the
//! marker, the same predicate `replay launch`, `replay delete`, and the
//! engine apply, so the surfaces cannot disagree on what "a recording"
//! is. Prefixes that hold objects WITHOUT a usable marker — the
//! leftovers of an interrupted publish or delete; the recorder never
//! retries an archive id, so nothing ever completes them — are rendered
//! as flagged INCOMPLETE rows (object count + total size) instead of
//! being invisible: that makes `cargo xtask replay delete <short id>`
//! reachable for exactly the residue its sweep exists to remove.
//! `replay launch` still refuses them — flagged is not launchable.

use anyhow::Result;
use clap::Args;
use rio_replay::archive::s3::ARCHIVE_IMAGE_OBJECT;

use super::{launch, s3};
use crate::k8s::eks::TF_DIR;
use crate::{tofu, ui};

#[derive(Args)]
pub struct ListArgs {}

/// One listing row, with every cell already rendered. Split from the
/// fetch so the table layout is unit-testable.
struct Row {
    short_id: String,
    hydra_eval: String,
    scope: String,
    created: String,
    size: String,
    fidelity: String,
}

impl Row {
    fn cells(&self) -> [&str; 6] {
        [
            &self.short_id,
            &self.hydra_eval,
            &self.scope,
            &self.created,
            &self.size,
            &self.fidelity,
        ]
    }
}

/// Build one row from a candidate (whose completion marker carries the
/// image size).
fn row(candidate: &launch::ArchiveCandidate) -> Row {
    Row {
        short_id: candidate.archive_id_short().to_string(),
        // 0 = the archive's provenance names no Hydra eval (published via
        // `launch --archive`, not the recorder).
        hydra_eval: match candidate.hydra_eval_id {
            0 => "-".to_string(),
            eval => eval.to_string(),
        },
        scope: candidate.scope_summary(),
        created: candidate.created_at.clone(),
        size: candidate
            .marker()
            .objects
            .get(ARCHIVE_IMAGE_OBJECT)
            .map(|digest| s3::human_bytes(digest.size))
            .unwrap_or_else(|| "?".to_string()),
        fidelity: candidate.fidelity_summary(),
    }
}

/// Build one flagged row for a marker-less prefix: the short id (the
/// handle `replay delete` accepts), the INCOMPLETE flag with the object
/// count, and the total size. Everything else is unknowable — there is
/// no marker or manifest to read — and renders `-`. The empty `created`
/// sorts these rows after every dated recording.
fn incomplete_row(prefix: &launch::IncompletePrefix) -> Row {
    Row {
        short_id: prefix.archive_id_short.clone(),
        hydra_eval: "-".to_string(),
        scope: format!(
            "INCOMPLETE ({} object{})",
            prefix.objects,
            if prefix.objects == 1 { "" } else { "s" }
        ),
        created: String::new(),
        size: s3::human_bytes(prefix.bytes),
        fidelity: "-".to_string(),
    }
}

/// The footer under the table: the published count, plus — only when
/// interrupted-publish/delete residue exists — the incomplete count and
/// the command that removes it. Pure for testability.
fn footer(published: usize, incomplete: usize, bucket: &str, archives_prefix: &str) -> String {
    let mut out = format!("\n{published} recording(s) under s3://{bucket}/{archives_prefix}");
    if incomplete > 0 {
        out.push_str(&format!(
            "\n{incomplete} INCOMPLETE prefix(es) — leftovers of an interrupted publish or \
             delete; remove with `cargo xtask replay delete <short id>`"
        ));
    }
    out
}

/// Render rows as a column-aligned table with a header, newest row first
/// (the caller sorts). Pure for testability.
fn render_table(rows: &[Row]) -> String {
    const HEADER: [&str; 6] = [
        "SHORT ID",
        "HYDRA EVAL",
        "SCOPE",
        "CREATED",
        "SIZE",
        "FIDELITY",
    ];
    let mut widths = HEADER.map(str::len);
    for r in rows {
        for (width, cell) in widths.iter_mut().zip(r.cells()) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: [&str; 6]| -> String {
        let mut out = String::new();
        for (i, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&format!("{cell:<width$}"));
        }
        out.trim_end().to_string()
    };
    std::iter::once(line(HEADER))
        .chain(rows.iter().map(|r| line(r.cells())))
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(clippy::print_stdout)]
pub async fn run(_a: ListArgs) -> Result<()> {
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let bucket = tf.get("chunk_bucket_name")?;

    // Every prefix becomes an entry: published recordings (candidacy is
    // the completion marker, whose document carries the image size the
    // table shows) and flagged INCOMPLETE residue alike.
    let entries = ui::step("list recorded archives", || {
        launch::listed_archives(&region, &bucket)
    })
    .await?;
    let mut published = 0usize;
    let mut incomplete = 0usize;
    let mut rows: Vec<(String, Row)> = entries
        .iter()
        .map(|entry| match entry {
            launch::ListedArchive::Published(candidate) => {
                published += 1;
                (candidate.created_at.clone(), row(candidate))
            }
            launch::ListedArchive::Incomplete(prefix) => {
                incomplete += 1;
                (String::new(), incomplete_row(prefix))
            }
        })
        .collect();

    if rows.is_empty() {
        println!(
            "no recordings found under s3://{bucket}/{}",
            s3::archives_prefix()
        );
        return Ok(());
    }
    // Newest first: created_at is RFC3339, so the lexicographic order is
    // the chronological order; INCOMPLETE rows (no created_at) sort last.
    rows.sort_by(|(a, _), (b, _)| b.cmp(a));
    let rows: Vec<Row> = rows.into_iter().map(|(_, r)| r).collect();
    println!("{}", render_table(&rows));
    println!(
        "{}",
        footer(published, incomplete, &bucket, &s3::archives_prefix())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_row(short: &str, eval: &str, scope: &str, size: &str) -> Row {
        Row {
            short_id: short.to_string(),
            hydra_eval: eval.to_string(),
            scope: scope.to_string(),
            created: "2026-06-01T10:00:00Z".to_string(),
            size: size.to_string(),
            fidelity: "12/12".to_string(),
        }
    }

    #[test]
    fn table_aligns_columns_and_keeps_row_order() {
        let rows = vec![
            fixture_row(
                "8b919129046e0f60",
                "1824219",
                "constituents:tested",
                "3.0 GiB",
            ),
            fixture_row("aaaaaaaaaaaaaaaa", "-", "jobs:2", "512.0 KiB"),
        ];
        let table = render_table(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        // Header first, then the rows in the order given.
        assert!(lines[0].starts_with("SHORT ID"), "{table}");
        assert!(lines[1].starts_with("8b919129046e0f60"), "{table}");
        assert!(lines[2].starts_with("aaaaaaaaaaaaaaaa"), "{table}");
        // Every column header is present.
        for header in ["HYDRA EVAL", "SCOPE", "CREATED", "SIZE", "FIDELITY"] {
            assert!(lines[0].contains(header), "{table}");
        }
        // Cells line up: every row starts each column at the same offset
        // as the header (alignment is what makes the table scannable).
        let scope_col = lines[0].find("SCOPE").unwrap();
        assert_eq!(&lines[1][scope_col..scope_col + 12], "constituents");
        assert_eq!(&lines[2][scope_col..scope_col + 6], "jobs:2");
        // No trailing whitespace on any line (keeps copy-paste clean).
        for line in &lines {
            assert_eq!(*line, line.trim_end());
        }
    }

    #[test]
    fn rows_render_recorder_and_foreign_archives() {
        use serde_json::json;

        // A recorder candidate renders its eval id, scope, and fidelity; the
        // image size comes from its completion marker.
        let archive_id = "8b919129046e0f60".to_string() + &"a".repeat(48);
        let candidate = launch::ArchiveCandidate::fixture(
            &archive_id,
            Some(3 * 1024 * 1024 * 1024),
            &"fe".repeat(32),
            1824219,
            "2026-06-01T10:00:00Z",
            json!({
                "provenance": {
                    "fidelity": {"checked": 12, "matched": 12, "divergent": false},
                    "scope": {"kind": "constituents", "aggregate_job": "tested"},
                    "systems": ["x86_64-linux"],
                },
            }),
        );
        let r = row(&candidate);
        assert_eq!(r.short_id, "8b919129046e0f60");
        assert_eq!(r.hydra_eval, "1824219");
        assert_eq!(r.scope, "constituents:tested");
        assert_eq!(r.size, "3.0 GiB");
        assert_eq!(r.fidelity, "12/12");

        // A non-recorder archive (no provenance) renders "-" for the eval
        // and "?" for scope/fidelity; a marker without the image entry
        // renders "?" for size.
        let foreign = launch::ArchiveCandidate::fixture(
            &"aa".repeat(32),
            None,
            "",
            0,
            "2026-06-02T10:00:00Z",
            json!({}),
        );
        let r = row(&foreign);
        assert_eq!(r.hydra_eval, "-");
        assert_eq!(r.scope, "?");
        assert_eq!(r.size, "?");
        assert_eq!(r.fidelity, "?");
    }

    #[test]
    fn incomplete_prefixes_render_flagged_rows_and_a_removal_hint() {
        // A marker-less prefix (interrupted publish or delete) renders its
        // short id — the handle `replay delete` accepts — an INCOMPLETE
        // flag with the object count, and the total size; everything a
        // marker would carry is "-". The empty `created` cell makes these
        // rows sort after every dated recording in the newest-first table.
        let prefix = launch::IncompletePrefix {
            archive_id_short: "8b919129046e0f60".to_string(),
            objects: 2,
            bytes: 3 * 1024 * 1024 * 1024,
        };
        let r = incomplete_row(&prefix);
        assert_eq!(r.short_id, "8b919129046e0f60");
        assert_eq!(r.hydra_eval, "-");
        assert_eq!(r.scope, "INCOMPLETE (2 objects)");
        assert_eq!(r.created, "");
        assert_eq!(r.size, "3.0 GiB");
        assert_eq!(r.fidelity, "-");
        let single = launch::IncompletePrefix {
            archive_id_short: "8b919129046e0f60".to_string(),
            objects: 1,
            bytes: 1024,
        };
        assert_eq!(incomplete_row(&single).scope, "INCOMPLETE (1 object)");

        // The footer names the removal command only when residue exists —
        // the discoverable path from a flagged row to its cleanup.
        let with_residue = footer(3, 2, "rio-chunks", "replay/archives/");
        assert!(with_residue.contains("3 recording(s)"), "{with_residue}");
        assert!(
            with_residue.contains("2 INCOMPLETE prefix(es)"),
            "{with_residue}"
        );
        assert!(
            with_residue.contains("cargo xtask replay delete <short id>"),
            "{with_residue}"
        );
        let clean = footer(3, 0, "rio-chunks", "replay/archives/");
        assert!(!clean.contains("INCOMPLETE"), "{clean}");
        assert!(!clean.contains("delete"), "{clean}");
    }
}
