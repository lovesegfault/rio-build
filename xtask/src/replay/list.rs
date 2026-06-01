//! `cargo xtask replay list` — list the recorded replay archives in S3.
//!
//! One row per published archive under `replay/archives/` in the chunk
//! bucket, newest first. "Published" means the prefix carries
//! `complete.json` (the recorder uploads it last): prefixes without it
//! are in-flight or interrupted uploads, invisible to `replay launch`
//! and to this listing alike — and therefore also not deletable by
//! `replay delete`, which shares the same definition of "a recording".

use anyhow::Result;
use clap::Args;
use rio_replay::archive::s3::{ARCHIVE_COMPLETE_OBJECT, ARCHIVE_IMAGE_OBJECT, CompleteMarker};

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

/// Build one row from a candidate and its (parsed) completion marker.
fn row(candidate: &launch::ArchiveCandidate, marker: &CompleteMarker) -> Row {
    Row {
        short_id: candidate.archive_id_short.clone(),
        // 0 = the archive's provenance names no Hydra eval (published via
        // `launch --archive`, not the recorder).
        hydra_eval: match candidate.hydra_eval_id {
            0 => "-".to_string(),
            eval => eval.to_string(),
        },
        scope: candidate.scope_summary(),
        created: candidate.created_at.clone(),
        size: marker
            .objects
            .get(ARCHIVE_IMAGE_OBJECT)
            .map(|digest| s3::human_bytes(digest.size))
            .unwrap_or_else(|| "?".to_string()),
        fidelity: candidate.fidelity_summary(),
    }
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

    // Every prefix with a manifest is a candidate; only those whose
    // completion marker exists are published recordings (the marker also
    // carries the image size the table shows).
    let candidates = ui::step("list recorded archives", || {
        launch::listed_candidates(&region, &bucket)
    })
    .await?;
    let mut rows: Vec<(String, Row)> = Vec::new();
    for candidate in &candidates {
        let complete_key = format!("{}/{ARCHIVE_COMPLETE_OBJECT}", candidate.s3_prefix);
        let Some(text) = s3::get_text(&region, &bucket, &complete_key).await? else {
            tracing::debug!(
                "skipping {}: no {ARCHIVE_COMPLETE_OBJECT} (upload in flight or interrupted)",
                candidate.archive_id_short
            );
            continue;
        };
        let Ok(marker) = serde_json::from_str::<CompleteMarker>(&text) else {
            tracing::warn!(
                "skipping {}: malformed {ARCHIVE_COMPLETE_OBJECT}",
                candidate.archive_id_short
            );
            continue;
        };
        rows.push((candidate.created_at.clone(), row(candidate, &marker)));
    }

    if rows.is_empty() {
        println!(
            "no recordings found under s3://{bucket}/{}",
            s3::archives_prefix()
        );
        return Ok(());
    }
    // Newest first: created_at is RFC3339, so the lexicographic order is
    // the chronological order.
    rows.sort_by(|(a, _), (b, _)| b.cmp(a));
    let rows: Vec<Row> = rows.into_iter().map(|(_, r)| r).collect();
    println!("{}", render_table(&rows));
    println!(
        "\n{} recording(s) under s3://{bucket}/{}",
        rows.len(),
        s3::archives_prefix()
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
        use std::collections::BTreeMap;

        use rio_replay::archive::schema::MemberDigest;
        use serde_json::json;

        // A recorder candidate renders its eval id, scope, and fidelity; the
        // image size comes from the completion marker.
        let candidate = launch::ArchiveCandidate {
            archive_id_short: "8b919129046e0f60".into(),
            s3_prefix: s3::archive_prefix("8b919129046e0f60"),
            archive_id: "8b".repeat(32),
            recipe_digest: "fe".repeat(32),
            hydra_eval_id: 1824219,
            created_at: "2026-06-01T10:00:00Z".into(),
            manifest: json!({
                "provenance": {
                    "fidelity": {"checked": 12, "matched": 12, "divergent": false},
                    "scope": {"kind": "constituents", "aggregate_job": "tested"},
                    "systems": ["x86_64-linux"],
                },
            }),
        };
        let marker = CompleteMarker {
            archive_id: "8b".repeat(32),
            archive_id_short: "8b919129046e0f60".into(),
            objects: BTreeMap::from([(
                ARCHIVE_IMAGE_OBJECT.to_string(),
                MemberDigest {
                    sha256: "ab".repeat(32),
                    size: 3 * 1024 * 1024 * 1024,
                },
            )]),
            uploaded_at: "2026-06-01T11:00:00Z".parse().unwrap(),
            uploader: "rio-replay-eval/0.1.0".into(),
        };
        let r = row(&candidate, &marker);
        assert_eq!(r.hydra_eval, "1824219");
        assert_eq!(r.scope, "constituents:tested");
        assert_eq!(r.size, "3.0 GiB");
        assert_eq!(r.fidelity, "12/12");

        // A non-recorder archive (no provenance) renders "-" for the eval
        // and "?" for scope/fidelity; a marker without the image entry
        // renders "?" for size.
        let foreign = launch::ArchiveCandidate {
            archive_id_short: "aaaaaaaaaaaaaaaa".into(),
            s3_prefix: s3::archive_prefix("aaaaaaaaaaaaaaaa"),
            archive_id: "aa".repeat(32),
            recipe_digest: String::new(),
            hydra_eval_id: 0,
            created_at: "2026-06-02T10:00:00Z".into(),
            manifest: json!({}),
        };
        let empty_marker = CompleteMarker {
            archive_id: "aa".repeat(32),
            archive_id_short: "aaaaaaaaaaaaaaaa".into(),
            objects: BTreeMap::new(),
            uploaded_at: "2026-06-02T11:00:00Z".parse().unwrap(),
            uploader: "xtask-replay-launch".into(),
        };
        let r = row(&foreign, &empty_marker);
        assert_eq!(r.hydra_eval, "-");
        assert_eq!(r.scope, "?");
        assert_eq!(r.size, "?");
        assert_eq!(r.fidelity, "?");
    }
}
