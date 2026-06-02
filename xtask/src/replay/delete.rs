//! `cargo xtask replay delete` — delete one recorded replay archive from S3.
//!
//! The deletion is driven by what exists: a ListObjectsV2 sweep of the
//! archive prefix removes every object found there, `complete.json`
//! strictly first — its removal atomically unpublishes the prefix for
//! `replay list`/`replay launch` (candidacy is the marker) and reopens it
//! for write-once publishing — then the data objects. Because the sweep
//! deletes what is listed rather than a fixed object set behind a marker
//! precondition, the resolve step tolerates a missing `complete.json`: an
//! interrupted delete is re-runnable, and the marker-less leftovers of an
//! interrupted publish (which `list`/`launch` hide and the write-once
//! publisher refuses to overwrite) are deletable. Every interrupted state
//! converges on an empty prefix.
//!
//! The recorder's by-recipe idempotency pointer is removed only when it
//! still points at the deleted archive — pointers are last-writer-wins,
//! so a newer re-record of the same recipe owns it.
//!
//! Campaigns that pinned the deleted archive keep their own S3 artifacts
//! (results, reports) but lose `replay repro` and pod-reschedule resume:
//! both re-fetch the archive by its pin.

use anyhow::{Result, ensure};
use clap::Args;
use rio_replay::archive::s3::{
    ARCHIVE_COMPLETE_OBJECT, ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT,
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

/// Order the sweep over one archive prefix: `complete.json` strictly
/// first — its removal atomically unpublishes the prefix — then the
/// remaining objects in name order. Pure for testability.
fn sweep_order(mut keys: Vec<String>, complete_key: &str) -> Vec<String> {
    keys.sort_by(|a, b| (a != complete_key, a).cmp(&(b != complete_key, b)));
    keys
}

/// The object name of one swept key, relative to the archive prefix (for
/// step labels and the confirmation listing).
fn object_name<'a>(key: &'a str, prefix: &str) -> &'a str {
    key.strip_prefix(prefix)
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(key)
}

/// The pre-deletion summary line: what `replay list` shows for this
/// archive, so the operator confirms against the same facts the listing
/// presented. Pure for testability.
fn deletion_summary(candidate: &launch::ArchiveCandidate) -> String {
    let size = candidate
        .marker()
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
        candidate.archive_id_short(),
        eval,
        candidate.scope_summary(),
        candidate.created_at,
        size,
        candidate.fidelity_summary(),
    )
}

/// Recipe digest read straight from a prefix's `manifest.json`, for
/// marker-less prefixes (an interrupted delete removes the marker first,
/// but the manifest may survive and its recipe still owns a pointer).
/// `None` when the manifest is gone or unreadable too — the pointer is
/// then left dangling, which is harmless: the recorder probes archive
/// existence before trusting one, and a re-record overwrites it.
async fn orphan_recipe_digest(region: &str, bucket: &str, prefix: &str) -> Result<Option<String>> {
    let manifest_key = format!("{prefix}/{ARCHIVE_MANIFEST_OBJECT}");
    let Some(text) = s3::get_text(region, bucket, &manifest_key).await? else {
        return Ok(None);
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(None);
    };
    Ok(manifest["provenance"]["recipe_digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .map(str::to_string))
}

pub async fn run(a: DeleteArgs) -> Result<()> {
    // The sweep deletes everything under the derived prefix, so the short
    // id must be exactly an archive prefix segment — never e.g. the
    // sibling `by-recipe/` pointer tree or an empty segment.
    ensure!(
        a.short_id.len() == 16
            && a.short_id
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{:?} is not an archive short id (16 lowercase hex characters, as shown by \
         `cargo xtask replay list`)",
        a.short_id
    );
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let prefix = s3::archive_prefix(&a.short_id);
    let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");

    // -- Resolve ------------------------------------------------------------
    // Driven by what exists, NOT by the completion marker: an interrupted
    // delete already removed the marker, and an interrupted publish never
    // wrote it — both leave objects this command must still remove
    // (`replay list`/`replay launch` hide such prefixes, so this is the
    // in-tool way out the write-once publisher's refusal points at).
    let keys = s3::list_keys(&region, &bucket, &format!("{prefix}/")).await?;
    ensure!(
        !keys.is_empty(),
        "no such recording: s3://{bucket}/{prefix}/ has no objects — `cargo xtask replay list` \
         shows the published recordings"
    );
    // The candidate (marker + manifest) provides the operator-facing
    // summary and the recipe digest for the pointer cleanup. A marker-less
    // or corrupted prefix yields no candidate but is still swept; the
    // recipe digest is then read straight from the manifest if it
    // survives.
    let candidate = launch::read_candidate(&region, &bucket, &a.short_id).await?;
    let recipe_digest = match &candidate {
        Some(candidate) if !candidate.recipe_digest.is_empty() => {
            Some(candidate.recipe_digest.clone())
        }
        Some(_) => None,
        None => orphan_recipe_digest(&region, &bucket, &prefix).await?,
    };

    // -- Confirm ------------------------------------------------------------
    match &candidate {
        Some(candidate) => tracing::info!("will delete: {}", deletion_summary(candidate)),
        None => tracing::info!(
            "will delete: {} (not a published recording — no usable \
             {ARCHIVE_COMPLETE_OBJECT}; these are the leftovers of an interrupted publish or \
             delete)",
            a.short_id
        ),
    }
    let listing: Vec<&str> = keys.iter().map(|key| object_name(key, &prefix)).collect();
    tracing::info!(
        "objects: {} under s3://{bucket}/{prefix}/",
        listing.join(", ")
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

    // -- Delete the archive objects ------------------------------------------
    // complete.json strictly first: its removal atomically unpublishes the
    // prefix (list/launch candidacy and the publisher's write-once probe
    // all key on it), so no consumer can resolve a half-deleted archive.
    // The data objects follow. Each delete is idempotent and a re-run
    // sweeps whatever is still listed, so every interrupted state
    // converges.
    for key in sweep_order(keys, &complete_key) {
        let object = object_name(&key, &prefix).to_string();
        ui::step(&format!("delete {object}"), || {
            s3::delete_object(&region, &bucket, &key)
        })
        .await?;
    }

    // -- By-recipe pointer -----------------------------------------------------
    // Only recorder archives have one (recipe digest in the provenance),
    // and only the archive the pointer currently names may take it down.
    if let Some(digest) = &recipe_digest {
        let pointer_key = format!("{}{digest}.json", s3::by_recipe_prefix());
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
    fn sweep_deletes_the_marker_first_and_converges() {
        let prefix = "replay/archives/8b919129046e0f60";
        let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");

        // A fully published prefix: the marker goes strictly first, the
        // data objects follow.
        let keys = vec![
            format!("{prefix}/{ARCHIVE_IMAGE_OBJECT}"),
            format!("{prefix}/{ARCHIVE_MANIFEST_OBJECT}"),
            complete_key.clone(),
        ];
        let order = sweep_order(keys, &complete_key);
        assert_eq!(order[0], complete_key, "the marker is unpublished first");
        assert_eq!(order.len(), 3);

        // An interrupted delete's leftovers (marker already gone) and an
        // interrupted publish's partial objects (marker never written) are
        // swept as-is: the order is total over whatever exists, so a
        // re-run over the remainder converges on an empty prefix.
        let leftovers = vec![
            format!("{prefix}/{ARCHIVE_MANIFEST_OBJECT}"),
            format!("{prefix}/{ARCHIVE_IMAGE_OBJECT}"),
        ];
        let order = sweep_order(leftovers, &complete_key);
        assert_eq!(
            order,
            vec![
                format!("{prefix}/{ARCHIVE_IMAGE_OBJECT}"),
                format!("{prefix}/{ARCHIVE_MANIFEST_OBJECT}"),
            ]
        );

        // Object names rendered for the confirmation listing are relative
        // to the prefix.
        assert_eq!(object_name(&complete_key, prefix), ARCHIVE_COMPLETE_OBJECT);
        assert_eq!(object_name("unrelated/key", prefix), "unrelated/key");
    }

    #[test]
    fn deletion_summary_mirrors_the_list_row() {
        let archive_id = "8b919129046e0f60".to_string() + &"a".repeat(48);
        let candidate = launch::ArchiveCandidate::fixture(
            &archive_id,
            Some(1024 * 1024),
            &"fe".repeat(32),
            1824219,
            "2026-06-01T10:00:00Z",
            json!({
                "provenance": {
                    "fidelity": {"checked": 12, "matched": 12, "divergent": false},
                    "scope": {"kind": "jobs", "jobs": ["a", "b"]},
                    "systems": ["x86_64-linux"],
                },
            }),
        );
        let summary = deletion_summary(&candidate);
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
