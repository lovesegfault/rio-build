//! `cargo xtask replay delete` — delete one recorded replay archive from S3.
//!
//! Deletion order is the reverse of the recorder's write-once publish
//! order: `complete.json` goes first, which makes the prefix atomically
//! invisible to `replay list`/`replay launch` (both treat the marker as
//! the existence test) and reopens it for write-once publishing; then
//! `manifest.json` and the bulk `archive.dwarfs`. The recorder's
//! by-recipe idempotency pointer is removed only when it still points at
//! the deleted archive — pointers are last-writer-wins, so a newer
//! re-record of the same recipe owns it.
//!
//! Campaigns that pinned the deleted archive keep their own S3 artifacts
//! (results, reports) but lose `replay repro` and pod-reschedule resume:
//! both re-fetch the archive by its pin.

use anyhow::{Context, Result, ensure};
use clap::Args;
use rio_replay::archive::s3::{
    ARCHIVE_COMPLETE_OBJECT, ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT, CompleteMarker,
};
use rio_replay::s3::ByRecipePointer;

use super::{launch, s3};
use crate::k8s::eks::TF_DIR;
use crate::{tofu, ui};

#[derive(Args)]
pub struct DeleteArgs {
    /// Archive id to delete — the 16-char short form shown by
    /// `replay list` (the S3 prefix segment under `replay/archives/`).
    pub short_id: String,
    /// Skip the confirmation prompt. Required for non-interactive runs.
    #[arg(long)]
    pub yes: bool,
}

/// Whether the recorder's by-recipe pointer should be deleted along with
/// archive `short_id`: only when it still names that archive. Pointers
/// are last-writer-wins — a newer re-record of the same recipe overwrote
/// it, and deleting the older archive must not take the newer archive's
/// pointer with it. Pure (pointer JSON in → decision out) so the
/// keep/delete rule is unit-testable; unparsable pointers are kept (never
/// delete what we cannot attribute).
fn pointer_owned_by(pointer_json: &str, short_id: &str) -> bool {
    serde_json::from_str::<ByRecipePointer>(pointer_json)
        // names_archive(): drift-tolerant reads turn garbage into empty
        // fields, and an empty pointer is owned by nobody (it must never
        // compare equal to anything, not even an empty short id).
        .map(|pointer| pointer.names_archive() && pointer.archive_id_short == short_id)
        .unwrap_or(false)
}

/// The pre-deletion summary line: what `replay list` shows for this
/// archive, so the operator confirms against the same facts the listing
/// presented. Pure for testability.
fn deletion_summary(candidate: &launch::ArchiveCandidate, marker: &CompleteMarker) -> String {
    let size = marker
        .objects
        .get(ARCHIVE_IMAGE_OBJECT)
        .map(|digest| s3::human_bytes(digest.size))
        .unwrap_or_else(|| "?".to_string());
    let eval = match candidate.hydra_eval_id {
        0 => "-".to_string(),
        eval => eval.to_string(),
    };
    format!(
        "{} | hydra eval {} | {} | created {} | {} | fidelity {}",
        candidate.archive_id_short,
        eval,
        candidate.scope_summary(),
        candidate.created_at,
        size,
        candidate.fidelity_summary(),
    )
}

pub async fn run(a: DeleteArgs) -> Result<()> {
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let prefix = s3::archive_prefix(&a.short_id);

    // -- Resolve ----------------------------------------------------------
    // A recording exists iff its completion marker does — the same
    // definition `replay list` and `replay launch` use, so delete can
    // never remove something the other commands still consider absent.
    let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");
    let marker_text = s3::get_text(&region, &bucket, &complete_key)
        .await?
        .with_context(|| {
            format!(
                "no such recording: s3://{bucket}/{prefix}/{ARCHIVE_COMPLETE_OBJECT} does not \
                 exist — `cargo xtask replay list` shows the deletable recordings. (If a \
                 previous delete was interrupted, leftover objects under s3://{bucket}/{prefix}/ \
                 can be removed with `aws s3 rm --recursive`.)"
            )
        })?;
    let marker: CompleteMarker = serde_json::from_str(&marker_text)
        .with_context(|| format!("parse s3://{bucket}/{complete_key}"))?;
    // The manifest provides the operator-facing summary and the recipe
    // digest for the by-recipe pointer cleanup. A recording with a
    // missing/unreadable manifest can still be deleted — there is just
    // less to show and no pointer to consider.
    let candidate = launch::read_candidate(&region, &bucket, &a.short_id).await?;

    // -- Confirm ------------------------------------------------------------
    match &candidate {
        Some(candidate) => tracing::info!("will delete: {}", deletion_summary(candidate, &marker)),
        None => tracing::info!(
            "will delete: {} (its manifest.json is missing or unreadable — no further metadata \
             to show)",
            a.short_id
        ),
    }
    tracing::info!(
        "objects: {ARCHIVE_COMPLETE_OBJECT}, {ARCHIVE_MANIFEST_OBJECT}, {ARCHIVE_IMAGE_OBJECT} \
         under s3://{bucket}/{prefix}/"
    );
    if !a.yes {
        let confirmed = ui::confirm_held(&format!(
            "Delete recording {} from s3://{bucket}/{prefix}/?",
            a.short_id
        ))?;
        ensure!(
            confirmed,
            "delete cancelled (pass --yes to skip the prompt)"
        );
    }

    // -- Delete the archive objects ----------------------------------------
    // Marker first (atomic disappearance from list/launch and write-once
    // probes), then metadata, then the bulk image. Each delete is
    // idempotent, so re-running after an interruption converges.
    for object in [
        ARCHIVE_COMPLETE_OBJECT,
        ARCHIVE_MANIFEST_OBJECT,
        ARCHIVE_IMAGE_OBJECT,
    ] {
        let key = format!("{prefix}/{object}");
        ui::step(&format!("delete {object}"), || {
            s3::delete_object(&region, &bucket, &key)
        })
        .await?;
    }

    // -- By-recipe pointer ---------------------------------------------------
    // Only recorder archives have one (recipe digest in the provenance),
    // and only the archive the pointer currently names may take it down.
    if let Some(candidate) = &candidate
        && !candidate.recipe_digest.is_empty()
    {
        let pointer_key = format!("{}{}.json", s3::by_recipe_prefix(), candidate.recipe_digest);
        match s3::get_text(&region, &bucket, &pointer_key).await? {
            Some(pointer_json) if pointer_owned_by(&pointer_json, &a.short_id) => {
                ui::step("delete by-recipe pointer", || {
                    s3::delete_object(&region, &bucket, &pointer_key)
                })
                .await?;
            }
            Some(_) => tracing::info!(
                "keeping s3://{bucket}/{pointer_key}: it points at a different (newer) archive \
                 of the same recipe"
            ),
            None => tracing::debug!("no by-recipe pointer at s3://{bucket}/{pointer_key}"),
        }
    }

    tracing::warn!(
        "recording {} deleted. Campaigns that pinned this archive keep their own S3 artifacts \
         (results, reports), but `replay repro` against them and pod-reschedule resume of any \
         still-running campaign will fail — both re-fetch the archive by its pin.",
        a.short_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rio_replay::archive::schema::MemberDigest;
    use serde_json::json;

    use super::*;

    #[test]
    fn pointer_ownership_decides_deletion() {
        // The pointer names this archive → delete it together with the
        // archive.
        let owned = json!({
            "archive_id": "8b".repeat(32),
            "archive_id_short": "8b919129046e0f60",
            "recorded_at": "2026-06-01T11:00:00Z",
        })
        .to_string();
        assert!(pointer_owned_by(&owned, "8b919129046e0f60"));

        // The pointer was overwritten by a newer re-record → keep it.
        assert!(!pointer_owned_by(&owned, "aaaaaaaaaaaaaaaa"));

        // Garbage or empty pointers are never attributed to anyone (and so
        // never deleted) — drift-tolerant reads turn unknown shapes into
        // empty fields, and an empty short id matches no archive.
        assert!(!pointer_owned_by("{}", "8b919129046e0f60"));
        assert!(!pointer_owned_by("not json", "8b919129046e0f60"));
        assert!(!pointer_owned_by("{}", ""));
    }

    #[test]
    fn deletion_summary_mirrors_the_list_row() {
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
                    "scope": {"kind": "jobs", "jobs": ["a", "b"]},
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
                    size: 1024 * 1024,
                },
            )]),
            uploaded_at: "2026-06-01T11:00:00Z".parse().unwrap(),
            uploader: "rio-replay-eval/0.1.0".into(),
        };
        let summary = deletion_summary(&candidate, &marker);
        for needle in [
            "8b919129046e0f60",
            "hydra eval 1824219",
            "jobs:2",
            "created 2026-06-01T10:00:00Z",
            "1.0 MiB",
            "fidelity 12/12",
        ] {
            assert!(summary.contains(needle), "{summary}");
        }
    }
}
