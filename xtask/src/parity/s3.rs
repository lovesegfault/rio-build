//! S3 layout + small read helpers for campaign artifacts.
//!
//! Everything lives in the existing chunk bucket under the `parity/`
//! prefix (own IRSA policy, infra/eks/parity.tf):
//!
//! ```text
//! parity/archives/<archive-id-short>/{archive.dwarfs,manifest.json,
//!   complete.json}                                         (replay archives)
//! parity/archives/by-recipe/<recipe-digest>.json           (recorder pointers)
//! parity/campaigns/<campaign-id>/{campaign.json,progress.json,
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
/// `parity/campaigns`).
pub fn campaign_key(campaign_id: &str, rel: &str) -> String {
    format!("{S3_PREFIX}/campaigns/{campaign_id}/{rel}")
}

/// Prefix every published replay archive lives under
/// (`parity/archives/`): one [`archive_prefix`] per archive, plus the
/// recorder-owned [`by_recipe_prefix`] pointer tree. Matches the
/// recorder's `ArchiveS3` layout with the engine's default `parity` root.
pub fn archives_prefix() -> String {
    format!("{S3_PREFIX}/{ARCHIVES_PREFIX_SEGMENT}/")
}

/// Key prefix of one published archive (no trailing slash):
/// `parity/archives/<archive-id-short>` — holds `archive.dwarfs`,
/// `manifest.json` and the `complete.json` upload marker.
pub fn archive_prefix(archive_id_short: &str) -> String {
    format!("{S3_PREFIX}/{ARCHIVES_PREFIX_SEGMENT}/{archive_id_short}")
}

/// Prefix of the recorder-owned by-recipe idempotency pointers
/// (`parity/archives/by-recipe/`): one `<recipe-digest>.json` per recorded
/// recipe, written after the archive's `complete.json`.
pub fn by_recipe_prefix() -> String {
    format!("{S3_PREFIX}/{ARCHIVES_PREFIX_SEGMENT}/{BY_RECIPE_SEGMENT}/")
}

/// List the immediate "subdirectory" segment names under `prefix` (which
/// must end with `/`): one ListObjectsV2 delimiter walk, returning each
/// CommonPrefix with the leading `prefix` and trailing `/` stripped.
/// `parity launch` uses it to discover the per-archive
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
    /// written it yet.
    pub async fn fetch_campaign_doc(&self, campaign_id: &str, rel: &str) -> Result<Option<String>> {
        get_text(&self.region, &self.bucket, &campaign_key(campaign_id, rel)).await
    }
}

/// Download an S3 object as UTF-8 text. `Ok(None)` when the key does not
/// exist yet (campaign/stage not reached); other errors propagate.
pub async fn get_text(region: &str, bucket: &str, key: &str) -> Result<Option<String>> {
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
    String::from_utf8(bytes.to_vec())
        .with_context(|| format!("s3://{bucket}/{key} is not UTF-8"))
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_key_layout_matches_design() {
        assert_eq!(
            campaign_key("parity-leaf-20260601-ab12", "progress.json"),
            "parity/campaigns/parity-leaf-20260601-ab12/progress.json"
        );
        assert_eq!(
            campaign_key("c1", "report/summary.md"),
            "parity/campaigns/c1/report/summary.md"
        );
    }

    #[test]
    fn archive_prefix_layout_matches_design() {
        assert_eq!(archives_prefix(), "parity/archives/");
        assert_eq!(
            archive_prefix("0123456789abcdef"),
            "parity/archives/0123456789abcdef"
        );
        assert_eq!(by_recipe_prefix(), "parity/archives/by-recipe/");
        // The pointer key launch GETs must be exactly the key the recorder
        // writes (rio_replay::s3::by_recipe_key under the same `parity`
        // root) — a drift here makes every by-recipe lookup miss.
        let digest = "ab".repeat(32);
        assert_eq!(
            format!("{}{digest}.json", by_recipe_prefix()),
            rio_replay::s3::by_recipe_key(super::S3_PREFIX, &digest)
        );
    }
}
