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
//! interrupted publish (which the write-once publisher refuses to
//! overwrite) are deletable.
//!
//! Concurrency is handled at both human gates and at the sweep itself.
//! The operator confirms against a listing that is re-resolved after the
//! prompt: an unbounded confirmation pause cannot act on a stale
//! snapshot — any drift in the key set refuses and asks for a re-run.
//! The sweep then re-lists and repeats until the prefix lists empty, so
//! objects landing mid-sweep — a publisher of the same archive id racing
//! this delete; the confirmed intent "this archive id must not exist"
//! covers whatever it writes — are removed on a later pass, and the
//! terminal empty LIST is the convergence proof: the command succeeds
//! only after observing the prefix empty. What it cannot exclude (S3 has
//! no cross-key transactions) is a racing publisher's marker landing
//! after that final LIST, its data objects already swept: that leaves a
//! marker-only prefix, visible in `replay list` and removed by re-running
//! this command. Every interrupted state converges on an empty prefix —
//! within one run for everything the sweep can observe, across a re-run
//! for a marker that slips into that milliseconds-wide window.
//!
//! The recorder's by-recipe idempotency pointer is removed only when it
//! still points at the deleted archive — pointers are last-writer-wins,
//! so a newer re-record of the same recipe owns it. That ownership check
//! is a GET → compare → DELETE on a key with no conditional-delete
//! support (S3 general-purpose buckets), so one residual race is
//! tolerated rather than closed: a re-record's pointer PUT landing
//! inside the GET→DELETE window is deleted even though it names the
//! newer archive. The newer archive itself is untouched and stays
//! published; what is lost is its by-recipe resolution — `replay launch
//! --eval` reports the recipe as never recorded, and the recorder's
//! idempotency gate would re-record it from scratch. The delete logs the
//! pointer body it verified ownership against immediately before the
//! DELETE, so a destroyed pointer stays attributable from logs (any
//! "wrote by-recipe pointer" the recorder logged after that witness was
//! the casualty); recovery is `replay launch --archive <short-id>`
//! (by-ref launch never reads pointers) or a re-record, which writes a
//! fresh pointer.
//!
//! Every S3 operation this command performs — the LIST→sweep loop, the
//! pointer GET→DELETE pair, the orphan-manifest read — routes through
//! the [`DeleteOps`] boundary, so each one can be interleaved against a
//! scripted concurrent history in tests; a raw client call would sit
//! outside every scriptable history, which is exactly how check-then-act
//! races stay untestable. The `s3_calls_route_through_the_ops_boundary`
//! lint pins that no such call exists in this module outside the
//! [`LiveOps`] impl.
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
    /// Archive handle to delete — the S3 prefix segment under
    /// `replay/archives/` as shown by `replay list` (recorder archives
    /// use the 16-char short id; INCOMPLETE residue from out-of-band
    /// writes may carry any segment name, and is equally deletable).
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
/// delete what we cannot attribute). The GET→DELETE pair applying this
/// rule is non-atomic — see the module doc for the tolerated residual.
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

/// Cap on LIST→sweep passes. A pass beyond the first only happens when a
/// publisher landed objects mid-sweep; one publish adds at most three, so
/// repeated non-empty re-lists mean an active stream of publishers this
/// command should not fight silently.
const MAX_SWEEP_PASSES: usize = 5;

/// Every S3 operation `replay delete` performs, factored into one
/// boundary so the command logic is unit-testable against scripted
/// concurrent histories — a racing publisher's PUT can be interleaved
/// between any two of these calls in a test, which is the only way the
/// command's check-then-act sequences (the confirmed sweep, the pointer
/// GET→DELETE pair) can be pinned against concurrent histories rather
/// than static snapshots. The `s3_calls_route_through_the_ops_boundary`
/// lint keeps the boundary complete: no raw S3 call may exist in this
/// module outside the [`LiveOps`] impl. (The candidate summary shown at
/// the confirmation prompt is read by [`launch::read_candidate`], a
/// helper shared with `replay launch`/`list` that performs no
/// mutations.)
trait DeleteOps {
    /// List every key currently under the archive prefix.
    async fn list(&mut self) -> Result<Vec<String>>;
    /// Delete one key under the archive prefix (idempotent — S3
    /// DeleteObject tolerates absence).
    async fn delete(&mut self, key: &str) -> Result<()>;
    /// Read one small text object (the by-recipe pointer, the orphan
    /// manifest); `None` when the key does not exist.
    async fn get_text(&mut self, key: &str) -> Result<Option<String>>;
    /// Delete the by-recipe pointer object (idempotent, like
    /// [`Self::delete`]; a separate method so scripted histories and the
    /// live `ui::step` label both name the pointer explicitly).
    async fn delete_pointer(&mut self, key: &str) -> Result<()>;
}

/// The real S3 surface: [`s3::list_keys`] over the archive prefix,
/// per-object [`ui::step`] deletes, and the small-text reads.
struct LiveOps<'a> {
    region: &'a str,
    bucket: &'a str,
    prefix: &'a str,
}

impl DeleteOps for LiveOps<'_> {
    async fn list(&mut self) -> Result<Vec<String>> {
        s3::list_keys(self.region, self.bucket, &format!("{}/", self.prefix)).await
    }

    async fn delete(&mut self, key: &str) -> Result<()> {
        let object = object_name(key, self.prefix).to_string();
        ui::step(&format!("delete {object}"), || {
            s3::delete_object(self.region, self.bucket, key)
        })
        .await
    }

    async fn get_text(&mut self, key: &str) -> Result<Option<String>> {
        s3::get_text(self.region, self.bucket, key).await
    }

    async fn delete_pointer(&mut self, key: &str) -> Result<()> {
        ui::step("delete by-recipe pointer", || {
            s3::delete_object(self.region, self.bucket, key)
        })
        .await
    }
}

/// Sweep `initial` (the keys the operator confirmed), then re-list and
/// sweep again until the prefix lists empty — a marker or data object
/// landing mid-sweep (a publisher of the same archive id racing this
/// delete) is removed on the next pass instead of being silently left
/// behind a "successful" delete. The terminal empty LIST is the
/// convergence proof. Returns the number of passes taken; errors out
/// after [`MAX_SWEEP_PASSES`] non-empty re-lists rather than fighting an
/// active publisher stream forever.
async fn sweep_until_empty(
    ops: &mut impl DeleteOps,
    complete_key: &str,
    initial: Vec<String>,
) -> Result<usize> {
    let mut keys = initial;
    for pass in 1..=MAX_SWEEP_PASSES {
        for key in sweep_order(keys, complete_key) {
            ops.delete(&key).await?;
        }
        keys = ops.list().await?;
        if keys.is_empty() {
            return Ok(pass);
        }
        tracing::info!(
            "prefix still lists {} object(s) after sweep pass {pass} — a publisher landed them \
             mid-sweep; sweeping again",
            keys.len()
        );
    }
    anyhow::bail!(
        "the prefix still lists objects after {MAX_SWEEP_PASSES} sweep passes — a publisher is \
         actively writing to it; wait for the publish to finish (or fail against the write-once \
         conditionals), then re-run the delete"
    )
}

/// What the by-recipe pointer step did, returned (not just logged) so
/// scripted-history tests pin which history produced which end state.
#[derive(Debug, PartialEq, Eq)]
enum PointerCleanup {
    /// The pointer named the deleted archive when read — deleted.
    Deleted,
    /// The pointer exists but is not this archive's (a newer re-record
    /// owns it, or the body is unattributable) — kept.
    Kept,
    /// No pointer object exists.
    Absent,
}

/// Remove the by-recipe pointer iff it still names the deleted archive.
/// GET → ownership check → DELETE is a non-atomic check-then-act on a
/// last-writer-wins key (S3 general-purpose buckets have no conditional
/// DeleteObject), so a re-record's pointer PUT landing inside the
/// GET→DELETE window is destroyed even though it names the newer archive
/// — the tolerated residual named in the module doc, pinned by the
/// scripted-history test `pointer_race_inside_the_get_delete_window`.
/// The witnessed pointer body is logged immediately before the DELETE so
/// that destruction stays attributable from logs.
async fn remove_pointer_if_owned(
    ops: &mut impl DeleteOps,
    pointer_key: &str,
    short_id: &str,
) -> Result<PointerCleanup> {
    let Some(pointer_json) = ops.get_text(pointer_key).await? else {
        return Ok(PointerCleanup::Absent);
    };
    if !pointer_owned_by(&pointer_json, short_id) {
        return Ok(PointerCleanup::Kept);
    }
    // The ownership witness: everything the decision was based on, logged
    // before the unconditional DELETE. A "wrote by-recipe pointer" log
    // line from a recorder, stamped after this witness, identifies the
    // pointer the residual race would have destroyed.
    let witnessed: ByRecipePointer = serde_json::from_str(&pointer_json).unwrap_or_default();
    tracing::info!(
        key = %pointer_key,
        archive_id = %witnessed.archive_id,
        recorded_at = %witnessed.recorded_at,
        "by-recipe pointer still names this archive — deleting it"
    );
    ops.delete_pointer(pointer_key).await?;
    Ok(PointerCleanup::Deleted)
}

/// The refusal raised when the prefix's key set changed between the
/// operator's confirmation and the sweep: what would be deleted is no
/// longer what was shown. `None` when the sets are identical (both sides
/// come from [`s3::list_keys`], which sorts). Pure for testability.
fn confirmation_drift(confirmed: &[String], fresh: &[String]) -> Option<String> {
    if confirmed == fresh {
        return None;
    }
    let added: Vec<&str> = fresh
        .iter()
        .filter(|key| !confirmed.contains(key))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = confirmed
        .iter()
        .filter(|key| !fresh.contains(key))
        .map(String::as_str)
        .collect();
    let mut drift = Vec::new();
    if !added.is_empty() {
        drift.push(format!("appeared: {}", added.join(", ")));
    }
    if !removed.is_empty() {
        drift.push(format!("vanished: {}", removed.join(", ")));
    }
    Some(drift.join("; "))
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
async fn orphan_recipe_digest(
    ops: &mut impl DeleteOps,
    manifest_key: &str,
) -> Result<Option<String>> {
    let Some(text) = ops.get_text(manifest_key).await? else {
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
    // The sweep deletes everything under the derived prefix, so the
    // handle must be a single archive prefix segment — never the sibling
    // `by-recipe/` pointer tree, an empty segment, or a multi-segment
    // path. The predicate is shared with `replay list`, which renders a
    // row (and the footer's delete hint) for exactly the segments
    // accepted here — including non-hex residue left by out-of-band
    // writes, which only this command's sweep can remove in-tool.
    ensure!(
        s3::is_archive_handle(&a.short_id),
        "{:?} is not an archive handle (a single non-empty path segment under \
         replay/archives/, excluding by-recipe/) — `cargo xtask replay list` shows the \
         deletable prefixes",
        a.short_id
    );
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let prefix = s3::archive_prefix(&a.short_id);
    let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");
    let mut ops = LiveOps {
        region: &region,
        bucket: &bucket,
        prefix: &prefix,
    };

    // -- Resolve ------------------------------------------------------------
    // Driven by what exists, NOT by the completion marker: an interrupted
    // delete already removed the marker, and an interrupted publish never
    // wrote it — both leave objects this command must still remove
    // (`replay list` flags such prefixes INCOMPLETE, and this command is
    // the in-tool way out that the write-once publisher's refusal and the
    // listing's removal hint both point at).
    let keys = ops.list().await?;
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
        None => {
            orphan_recipe_digest(&mut ops, &format!("{prefix}/{ARCHIVE_MANIFEST_OBJECT}")).await?
        }
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

    // -- Re-resolve after the human gate --------------------------------------
    // The confirmation pause is unbounded; act only on the key set the
    // operator actually saw. Any drift — a publish completing into the
    // prefix, another delete racing this one — refuses, so what gets
    // swept first is always what was confirmed.
    let fresh = ops.list().await?;
    if let Some(drift) = confirmation_drift(&keys, &fresh) {
        anyhow::bail!(
            "the prefix changed while awaiting confirmation ({drift}) — re-run \
             `cargo xtask replay delete {}` to see and confirm the current state",
            a.short_id
        );
    }

    // -- Delete the archive objects ------------------------------------------
    // complete.json strictly first: its removal atomically unpublishes the
    // prefix (list/launch candidacy and the publisher's write-once probe
    // all key on it), so no consumer can resolve a half-deleted archive.
    // The data objects follow, then the prefix is re-listed and swept
    // until it lists empty — convergence is observed, not assumed.
    sweep_until_empty(&mut ops, &complete_key, fresh).await?;

    // -- By-recipe pointer -----------------------------------------------------
    // Only recorder archives have one (recipe digest in the provenance),
    // and only the archive the pointer currently names may take it down.
    if let Some(digest) = &recipe_digest {
        let pointer_key = format!("{}{digest}.json", s3::by_recipe_prefix());
        match remove_pointer_if_owned(&mut ops, &pointer_key, &a.short_id).await? {
            PointerCleanup::Deleted => {}
            PointerCleanup::Kept => tracing::info!(
                "keeping s3://{bucket}/{pointer_key}: it does not name this archive (a newer \
                 re-record of the same recipe owns it, or the pointer body is unattributable)"
            ),
            PointerCleanup::Absent => {
                tracing::debug!("no by-recipe pointer at s3://{bucket}/{pointer_key}")
            }
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

    /// A scripted prefix-and-pointer: records every operation in arrival
    /// order, serves the queued LIST results (empty once the script runs
    /// out), holds the by-recipe pointer as last-writer-wins state, and
    /// can land a racing pointer PUT immediately after the next pointer
    /// GET — the interleaving a live S3 cannot be made to reproduce on
    /// demand.
    struct ScriptedPrefix {
        lists: std::collections::VecDeque<Vec<String>>,
        deleted: Vec<String>,
        /// Current by-recipe pointer object body (None = absent).
        pointer: Option<String>,
        /// A racing re-record's pointer PUT, applied right after the next
        /// `get_text` serves — the last-writer-wins write landing inside
        /// the GET→DELETE window.
        pointer_put_after_get: Option<String>,
        /// Every op in arrival order, for history assertions.
        ops: Vec<String>,
    }

    impl ScriptedPrefix {
        fn new(lists: Vec<Vec<String>>) -> Self {
            Self {
                lists: lists.into(),
                deleted: Vec::new(),
                pointer: None,
                pointer_put_after_get: None,
                ops: Vec::new(),
            }
        }
    }

    impl DeleteOps for ScriptedPrefix {
        async fn list(&mut self) -> Result<Vec<String>> {
            self.ops.push("list".to_string());
            Ok(self.lists.pop_front().unwrap_or_default())
        }

        async fn delete(&mut self, key: &str) -> Result<()> {
            self.ops.push(format!("delete {key}"));
            self.deleted.push(key.to_string());
            Ok(())
        }

        async fn get_text(&mut self, key: &str) -> Result<Option<String>> {
            self.ops.push(format!("get {key}"));
            let served = self.pointer.clone();
            if let Some(racer) = self.pointer_put_after_get.take() {
                self.pointer = Some(racer);
            }
            Ok(served)
        }

        async fn delete_pointer(&mut self, key: &str) -> Result<()> {
            self.ops.push(format!("delete-pointer {key}"));
            self.pointer = None;
            Ok(())
        }
    }

    /// A pointer body naming `short_id`, as the recorder writes it.
    fn pointer_naming(short_id: &str, recorded_at: &str) -> String {
        json!({
            "archive_id": short_id.repeat(4),
            "archive_id_short": short_id,
            "recorded_at": recorded_at,
        })
        .to_string()
    }

    #[tokio::test]
    async fn sweep_converges_when_a_marker_lands_mid_sweep() {
        let prefix = "replay/archives/8b919129046e0f60";
        let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");
        let image_key = format!("{prefix}/{ARCHIVE_IMAGE_OBJECT}");
        let manifest_key = format!("{prefix}/{ARCHIVE_MANIFEST_OBJECT}");

        // The confirmed listing was marker-less (an in-flight publish);
        // while the data objects are being swept, the racing publisher's
        // marker lands. The first re-list catches it, the second pass
        // sweeps it, and the terminal LIST proves the prefix empty —
        // without the loop this delete would have "succeeded" leaving a
        // marker claiming completeness over nothing.
        let mut prefix_state = ScriptedPrefix::new(vec![vec![complete_key.clone()], vec![]]);
        let passes = sweep_until_empty(
            &mut prefix_state,
            &complete_key,
            vec![image_key.clone(), manifest_key.clone()],
        )
        .await
        .unwrap();
        assert_eq!(passes, 2);
        assert_eq!(
            prefix_state.deleted,
            vec![image_key, manifest_key, complete_key],
            "the mid-sweep marker is swept on the second pass"
        );
        assert!(
            prefix_state.lists.is_empty(),
            "the sweep observed the terminal empty LIST"
        );
    }

    #[tokio::test]
    async fn sweep_refuses_an_actively_refilling_prefix() {
        let prefix = "replay/archives/8b919129046e0f60";
        let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");
        let image_key = format!("{prefix}/{ARCHIVE_IMAGE_OBJECT}");

        // Every re-list finds new objects: an active publisher stream.
        // The sweep must give up loudly after the pass cap instead of
        // fighting forever (or declaring success while objects remain).
        let mut prefix_state = ScriptedPrefix::new(vec![vec![image_key.clone()]; MAX_SWEEP_PASSES]);
        let err = sweep_until_empty(&mut prefix_state, &complete_key, vec![image_key])
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("actively writing"),
            "the refusal names the racing publisher: {message}"
        );
        assert!(
            message.contains("re-run the delete"),
            "the refusal names the recovery: {message}"
        );
        assert_eq!(prefix_state.deleted.len(), MAX_SWEEP_PASSES);
    }

    #[tokio::test]
    async fn pointer_cleanup_deletes_an_owned_pointer() {
        // No race: the pointer still names the deleted archive, so the
        // cleanup removes it — exactly one GET then one DELETE, in that
        // order (must-delete direction of the ownership rule).
        let pointer_key = "replay/archives/by-recipe/feed.json";
        let mut state = ScriptedPrefix::new(vec![]);
        state.pointer = Some(pointer_naming("8b919129046e0f60", "2026-06-01T11:00:00Z"));

        let outcome = remove_pointer_if_owned(&mut state, pointer_key, "8b919129046e0f60")
            .await
            .unwrap();
        assert_eq!(outcome, PointerCleanup::Deleted);
        assert_eq!(state.pointer, None, "the owned pointer is gone");
        assert_eq!(
            state.ops,
            vec![
                format!("get {pointer_key}"),
                format!("delete-pointer {pointer_key}"),
            ],
            "the pair is GET then DELETE, nothing else"
        );
    }

    #[tokio::test]
    async fn pointer_cleanup_keeps_a_newer_pointer_and_an_unattributable_one() {
        // Must-keep direction: a re-record's pointer PUT that landed
        // BEFORE the GET is observed and kept — no delete is issued, the
        // newer archive keeps its by-recipe resolution.
        let pointer_key = "replay/archives/by-recipe/feed.json";
        let mut state = ScriptedPrefix::new(vec![]);
        state.pointer = Some(pointer_naming("aaaaaaaaaaaaaaaa", "2026-06-01T12:00:00Z"));

        let outcome = remove_pointer_if_owned(&mut state, pointer_key, "8b919129046e0f60")
            .await
            .unwrap();
        assert_eq!(outcome, PointerCleanup::Kept);
        assert_eq!(
            state.pointer,
            Some(pointer_naming("aaaaaaaaaaaaaaaa", "2026-06-01T12:00:00Z")),
            "the newer pointer survives"
        );
        assert_eq!(state.ops, vec![format!("get {pointer_key}")], "no delete");

        // An unattributable body is kept too: never delete what cannot be
        // attributed.
        let mut state = ScriptedPrefix::new(vec![]);
        state.pointer = Some("not json".to_string());
        let outcome = remove_pointer_if_owned(&mut state, pointer_key, "8b919129046e0f60")
            .await
            .unwrap();
        assert_eq!(outcome, PointerCleanup::Kept);
        assert_eq!(state.pointer, Some("not json".to_string()));

        // And an absent pointer is simply reported absent.
        let mut state = ScriptedPrefix::new(vec![]);
        let outcome = remove_pointer_if_owned(&mut state, pointer_key, "8b919129046e0f60")
            .await
            .unwrap();
        assert_eq!(outcome, PointerCleanup::Absent);
        assert_eq!(state.ops, vec![format!("get {pointer_key}")]);
    }

    #[tokio::test]
    async fn pointer_race_inside_the_get_delete_window() {
        // THE residual race, scripted: the GET witnesses a pointer owned
        // by the archive being deleted; a concurrent re-record of the
        // same recipe lands its last-writer-wins pointer PUT inside the
        // GET→DELETE window; the unconditional DELETE then destroys the
        // NEWER pointer. This end state is the module doc's tolerated
        // residual — S3 general-purpose buckets offer no conditional
        // DeleteObject to close it, the newer archive stays published and
        // launchable by reference, and the witness logged before the
        // DELETE keeps the destruction attributable. If the substrate
        // ever grows a conditional delete, closing the race flips exactly
        // this pin: the racer's pointer must then survive (Kept).
        let pointer_key = "replay/archives/by-recipe/feed.json";
        let racer = pointer_naming("aaaaaaaaaaaaaaaa", "2026-06-01T12:00:00Z");
        let mut state = ScriptedPrefix::new(vec![]);
        state.pointer = Some(pointer_naming("8b919129046e0f60", "2026-06-01T11:00:00Z"));
        state.pointer_put_after_get = Some(racer);

        let outcome = remove_pointer_if_owned(&mut state, pointer_key, "8b919129046e0f60")
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PointerCleanup::Deleted,
            "the cleanup acted on its (stale) ownership witness"
        );
        assert_eq!(
            state.pointer, None,
            "the racer's newer pointer is destroyed — the tolerated residual; its archive \
             stays published, only by-recipe resolution is lost"
        );
        assert_eq!(
            state.ops,
            vec![
                format!("get {pointer_key}"),
                format!("delete-pointer {pointer_key}"),
            ],
            "no second look exists between the GET and the DELETE"
        );
    }

    #[tokio::test]
    async fn orphan_recipe_digest_reads_the_surviving_manifest() {
        // A marker-less prefix (interrupted delete) still has its
        // manifest: the recipe digest is read from it so the pointer
        // cleanup can run. Garbage or absent manifests yield None — the
        // pointer is then left dangling, which the recorder tolerates.
        let manifest_key = "replay/archives/8b919129046e0f60/manifest.json";
        let mut state = ScriptedPrefix::new(vec![]);
        state.pointer = Some(
            json!({"provenance": {"recipe_digest": "ab".repeat(32)}, "version": 1}).to_string(),
        );
        let digest = orphan_recipe_digest(&mut state, manifest_key)
            .await
            .unwrap();
        assert_eq!(digest, Some("ab".repeat(32)));

        let mut state = ScriptedPrefix::new(vec![]);
        state.pointer = Some("not json".to_string());
        assert_eq!(
            orphan_recipe_digest(&mut state, manifest_key)
                .await
                .unwrap(),
            None
        );

        let mut state = ScriptedPrefix::new(vec![]);
        assert_eq!(
            orphan_recipe_digest(&mut state, manifest_key)
                .await
                .unwrap(),
            None
        );
    }

    /// FORCING FUNCTION for the ops boundary's completeness. Domain:
    /// every call-shaped `s3::<fn>(` path and every raw-AWS-client token
    /// in this file's production code (the file's own bytes via
    /// `include_str!`, truncated at this test module — the same shape as
    /// the SLA metrics source lint in rio-scheduler). Expected: exactly
    /// the four calls the [`LiveOps`] impl owns and zero raw client
    /// tokens, so any new S3 traffic added to the command logic fails
    /// here until it is routed through [`DeleteOps`] — where scripted
    /// histories can interleave a racing writer against it.
    #[test]
    fn s3_calls_route_through_the_ops_boundary() {
        let src = include_str!("delete.rs");
        let prod = src
            .split_once("#[cfg(test)]\nmod tests")
            .map(|(prod, _)| prod)
            .unwrap_or(src);

        // Call-shaped occurrences of `s3::<ident>(`, built at runtime so
        // this test's own source cannot match itself.
        let needle = format!("{}{}", "s3", "::");
        let mut calls: Vec<String> = Vec::new();
        let mut at = 0;
        while let Some(found) = prod[at..].find(&needle) {
            let start = at + found + needle.len();
            let ident: String = prod[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if prod[start + ident.len()..].starts_with('(') && !ident.is_empty() {
                calls.push(ident);
            }
            at = start;
        }
        calls.sort();
        assert_eq!(
            calls,
            [
                "archive_prefix",
                "by_recipe_prefix",
                "delete_object",
                "delete_object",
                "get_text",
                "human_bytes",
                "is_archive_handle",
                "list_keys"
            ],
            "every S3 operation must route through the DeleteOps boundary (LiveOps owns \
             list_keys/get_text and the two delete_object sites; archive_prefix, \
             by_recipe_prefix, human_bytes and the is_archive_handle candidacy predicate \
             are pure key/format helpers with no S3 traffic) — a raw call cannot be \
             interleaved against a scripted history"
        );

        // No raw AWS client may be constructed or named here at all.
        for raw in ["aws_sdk_s3", "crate::aws"] {
            assert_eq!(
                prod.matches(raw).count(),
                0,
                "{raw} must not appear in the delete command module"
            );
        }
    }

    #[test]
    fn confirmation_drift_names_what_changed() {
        let confirmed = vec![
            "a/archive.dwarfs".to_string(),
            "a/manifest.json".to_string(),
        ];

        // No drift: the confirmed set is exactly what is still there.
        assert_eq!(confirmation_drift(&confirmed, &confirmed), None);

        // A publish completed into the prefix during the pause: the
        // marker appeared. The drift message names it so the operator
        // knows what they would now be deleting.
        let with_marker = vec![
            "a/archive.dwarfs".to_string(),
            "a/complete.json".to_string(),
            "a/manifest.json".to_string(),
        ];
        let drift = confirmation_drift(&confirmed, &with_marker).unwrap();
        assert!(drift.contains("appeared: a/complete.json"), "{drift}");

        // Another delete raced this one: objects vanished.
        let emptied: Vec<String> = Vec::new();
        let drift = confirmation_drift(&confirmed, &emptied).unwrap();
        assert!(
            drift.contains("vanished: a/archive.dwarfs, a/manifest.json"),
            "{drift}"
        );
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
