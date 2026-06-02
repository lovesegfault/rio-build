//! S3 layout + small read helpers for campaign artifacts.
//!
//! Everything lives in the existing chunk bucket under the `replay/`
//! prefix (own IRSA policy, infra/eks/replay.tf):
//!
//! ```text
//! replay/archives/<archive-id-short>/{archive.dwarfs,manifest.json,
//!   complete.json}                                         (replay archives)
//! replay/archives/by-recipe/<recipe-digest>.json           (recorder pointers)
//! replay/campaigns/<campaign-id>/{campaign.json,progress.json,
//!   results.jsonl,buckets/,logs/,report/summary.md,…}      (campaigns)
//! ```

use anyhow::{Context, Result};
use rio_replay::archive::s3::ARCHIVES_PREFIX_SEGMENT;
use rio_replay::s3::BY_RECIPE_SEGMENT;

use super::S3_PREFIX;
use crate::k8s::eks::TF_DIR;
use crate::tofu;

/// Key of a campaign-scoped artifact, e.g. `progress.json` or
/// `report/summary.md`. Matches the engine's campaign sync layout
/// (`<prefix>/<campaign-id>/<rel>` with the engine's default prefix
/// `replay/campaigns`).
pub fn campaign_key(campaign_id: &str, rel: &str) -> String {
    format!("{S3_PREFIX}/campaigns/{campaign_id}/{rel}")
}

/// Prefix every published replay archive lives under
/// (`replay/archives/`): one [`archive_prefix`] per archive, plus the
/// recorder-owned [`by_recipe_prefix`] pointer tree. Matches the
/// recorder's `ArchiveS3` layout with the engine's default `replay` root.
pub fn archives_prefix() -> String {
    format!("{S3_PREFIX}/{ARCHIVES_PREFIX_SEGMENT}/")
}

/// Key prefix of one published archive (no trailing slash):
/// `replay/archives/<archive-id-short>` — holds `archive.dwarfs`,
/// `manifest.json` and the `complete.json` upload marker.
pub fn archive_prefix(archive_id_short: &str) -> String {
    format!("{S3_PREFIX}/{ARCHIVES_PREFIX_SEGMENT}/{archive_id_short}")
}

/// Prefix of the recorder-owned by-recipe idempotency pointers
/// (`replay/archives/by-recipe/`): one `<recipe-digest>.json` per recorded
/// recipe, written after the archive's `complete.json`.
pub fn by_recipe_prefix() -> String {
    format!("{S3_PREFIX}/{ARCHIVES_PREFIX_SEGMENT}/{BY_RECIPE_SEGMENT}/")
}

/// Is `segment` a per-archive prefix handle under [`archives_prefix`] —
/// a segment `replay list` renders a row for and `replay delete` sweeps?
///
/// ONE predicate for both surfaces, so they cannot disagree: every
/// INCOMPLETE row the listing surfaces carries the footer's
/// `replay delete <short id>` hint, and the delete gate must accept
/// exactly those handles. The recorder always writes 16-lowercase-hex
/// segments, but out-of-band writes can park any segment name here, and
/// the listing's whole point is making such residue removable in-tool.
/// Deliberately shape-agnostic beyond the structural requirements: a
/// single non-empty path segment (the sweep prefix is derived from it,
/// so a multi-segment or empty handle would address the wrong subtree)
/// that is not the recorder-owned `by-recipe/` pointer tree.
pub fn is_archive_handle(segment: &str) -> bool {
    !segment.is_empty() && segment != BY_RECIPE_SEGMENT && !segment.contains('/')
}

/// List the immediate "subdirectory" segment names under `prefix` (which
/// must end with `/`): one ListObjectsV2 delimiter walk, returning each
/// CommonPrefix with the leading `prefix` and trailing `/` stripped.
/// `replay launch` uses it to discover the per-archive
/// `<archive-id-short>/` prefixes under [`archives_prefix`].
pub async fn list_subprefixes(region: &str, bucket: &str, prefix: &str) -> Result<Vec<String>> {
    let s3 = aws_sdk_s3::Client::new(crate::aws::config(Some(region)).await);
    let mut out = Vec::new();
    let mut pages = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .delimiter("/")
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.with_context(|| format!("ListObjectsV2 s3://{bucket}/{prefix}"))?;
        for cp in page.common_prefixes() {
            let Some(p) = cp.prefix() else { continue };
            let segment = p.strip_prefix(prefix).unwrap_or(p).trim_end_matches('/');
            if !segment.is_empty() {
                out.push(segment.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// List every object key under `prefix` (one ListObjectsV2 paginator
/// walk, no delimiter), sorted. `replay delete` sweeps one archive prefix
/// with it so the deletion is driven by what exists, not by a marker
/// object an earlier interrupted run may already have removed.
pub async fn list_keys(region: &str, bucket: &str, prefix: &str) -> Result<Vec<String>> {
    Ok(list_objects(region, bucket, prefix)
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect())
}

/// [`list_keys`] with each object's size: one `(key, bytes)` pair per
/// object, sorted by key. `replay list` renders marker-less prefixes
/// (interrupted publishes or deletes) from this — their object count and
/// total size are the only facts S3 has about them.
pub async fn list_objects(region: &str, bucket: &str, prefix: &str) -> Result<Vec<(String, u64)>> {
    let s3 = aws_sdk_s3::Client::new(crate::aws::config(Some(region)).await);
    let mut out = Vec::new();
    let mut pages = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.with_context(|| format!("ListObjectsV2 s3://{bucket}/{prefix}"))?;
        for object in page.contents() {
            if let Some(key) = object.key() {
                out.push((key.to_string(), object.size().unwrap_or(0).max(0) as u64));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Where the campaign artifacts live: chunk bucket + region, resolved
/// once from the EKS tofu outputs (one `tofu output -json` spawn) so
/// `status --watch` and `report` share the same
/// tofu-outputs → bucket → [`campaign_key`] → [`get_text`] sequence and
/// a watch loop never re-shells-out to tofu per poll.
pub struct CampaignStore {
    region: String,
    bucket: String,
}

impl CampaignStore {
    /// Resolve the region and chunk bucket from the EKS tofu outputs.
    pub fn discover() -> Result<Self> {
        let tf = tofu::outputs(TF_DIR)?;
        Ok(Self {
            region: tf.get("region")?,
            bucket: tf.get("chunk_bucket_name")?,
        })
    }

    /// `s3://…` URI of one campaign-scoped artifact, for messages.
    pub fn uri(&self, campaign_id: &str, rel: &str) -> String {
        format!("s3://{}/{}", self.bucket, campaign_key(campaign_id, rel))
    }

    /// Download one campaign-scoped artifact (`rel` as in
    /// [`campaign_key`]) as text. `Ok(None)` when the engine has not
    /// written it yet. For the atomically rewritten documents
    /// (campaign.json, progress.json, report files), which are always
    /// complete UTF-8; the JSONL streams must go through
    /// [`Self::fetch_campaign_bytes`] instead.
    pub async fn fetch_campaign_doc(&self, campaign_id: &str, rel: &str) -> Result<Option<String>> {
        get_text(&self.region, &self.bucket, &campaign_key(campaign_id, rel)).await
    }

    /// Download one campaign-scoped artifact as raw bytes. The engine
    /// syncs its JSONL streams byte-verbatim while it is still appending
    /// to them, so a fetched copy can end in a torn tail — possibly cut
    /// mid multi-byte character. Consumers split that tail off with the
    /// engine's own rule (`rio_replay::run::state::split_torn_tail`)
    /// before parsing; decoding to text first would reject exactly those
    /// copies.
    pub async fn fetch_campaign_bytes(
        &self,
        campaign_id: &str,
        rel: &str,
    ) -> Result<Option<Vec<u8>>> {
        get_bytes(&self.region, &self.bucket, &campaign_key(campaign_id, rel)).await
    }
}

/// Delete one S3 object. Deleting a missing object is not an error (S3
/// DeleteObject is idempotent), so an interrupted `replay delete` re-run
/// converges instead of failing on the objects it already removed.
pub async fn delete_object(region: &str, bucket: &str, key: &str) -> Result<()> {
    let s3 = aws_sdk_s3::Client::new(crate::aws::config(Some(region)).await);
    s3.delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("DeleteObject s3://{bucket}/{key}"))?;
    Ok(())
}

/// Render a byte count in human-readable binary units — the precision an
/// operator needs to eyeball an archive's transfer cost, not an exact
/// accounting (`replay record`'s summary and `replay list` both use it).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Download an S3 object as UTF-8 text. `Ok(None)` when the key does not
/// exist yet (campaign/stage not reached); other errors propagate.
pub async fn get_text(region: &str, bucket: &str, key: &str) -> Result<Option<String>> {
    match get_bytes(region, bucket, key).await? {
        Some(bytes) => String::from_utf8(bytes)
            .with_context(|| format!("s3://{bucket}/{key} is not UTF-8"))
            .map(Some),
        None => Ok(None),
    }
}

/// Download an S3 object as raw bytes. `Ok(None)` when the key does not
/// exist yet (campaign/stage not reached); other errors propagate.
pub async fn get_bytes(region: &str, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
    let s3 = aws_sdk_s3::Client::new(crate::aws::config(Some(region)).await);
    let resp = match s3.get_object().bucket(bucket).key(key).send().await {
        Ok(o) => o,
        Err(e) => {
            if e.as_service_error().is_some_and(|se| se.is_no_such_key()) {
                return Ok(None);
            }
            return Err(e).with_context(|| format!("GetObject s3://{bucket}/{key}"));
        }
    };
    let bytes = resp
        .body
        .collect()
        .await
        .with_context(|| format!("read body of s3://{bucket}/{key}"))?
        .into_bytes();
    Ok(Some(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_key_layout_matches_design() {
        assert_eq!(
            campaign_key("replay-leaf-20260601-ab12", "progress.json"),
            "replay/campaigns/replay-leaf-20260601-ab12/progress.json"
        );
        assert_eq!(
            campaign_key("c1", "report/summary.md"),
            "replay/campaigns/c1/report/summary.md"
        );
    }

    #[test]
    fn human_bytes_picks_the_right_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_bytes(2_147_483_648), "2.0 GiB");
        // Beyond the largest unit it keeps scaling in TiB instead of
        // overflowing into nonsense.
        assert_eq!(human_bytes(5 * 1024_u64.pow(4)), "5.0 TiB");
        assert_eq!(human_bytes(2048 * 1024_u64.pow(4)), "2048.0 TiB");
    }

    #[test]
    fn archive_handle_predicate_is_shared_by_list_and_delete() {
        // One predicate decides BOTH list-row rendering (and with it the
        // footer's `replay delete <short id>` hint) and delete's
        // acceptance, so the listing can never advertise a removal
        // command that refuses the handle. Domain: every segment an S3
        // delimiter walk under replay/archives/ can yield (non-empty,
        // slash-free), plus the raw operator argument delete receives.

        // Admitted: recorder-conformant short ids AND any out-of-band
        // residue segment the listing would flag INCOMPLETE.
        for handle in ["0123456789abcdef", "test", "0123456789ABCDEF", "x"] {
            assert!(is_archive_handle(handle), "{handle:?} must be deletable");
        }
        // Refused: nothing the listing renders. The by-recipe pointer
        // tree is excluded from listing by this same predicate; empty
        // and multi-segment strings cannot come out of the delimiter
        // walk but CAN arrive as the raw delete argument, where they
        // would derive a sweep prefix addressing the wrong subtree.
        for handle in ["", BY_RECIPE_SEGMENT, "a/b", "by-recipe/x"] {
            assert!(!is_archive_handle(handle), "{handle:?} must be refused");
        }
    }

    #[test]
    fn archive_prefix_layout_matches_design() {
        assert_eq!(archives_prefix(), "replay/archives/");
        assert_eq!(
            archive_prefix("0123456789abcdef"),
            "replay/archives/0123456789abcdef"
        );
        assert_eq!(by_recipe_prefix(), "replay/archives/by-recipe/");
        // The pointer key launch GETs must be exactly the key the recorder
        // writes (rio_replay::s3::by_recipe_key under the same `replay`
        // root) — a drift here makes every by-recipe lookup miss.
        let digest = "ab".repeat(32);
        assert_eq!(
            format!("{}{digest}.json", by_recipe_prefix()),
            rio_replay::s3::by_recipe_key(super::S3_PREFIX, &digest)
        );
    }
}
