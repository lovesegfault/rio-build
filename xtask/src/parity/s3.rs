//! S3 layout + small read helpers for campaign artifacts.
//!
//! Everything lives in the existing chunk bucket under the `parity/`
//! prefix (own IRSA policy, infra/eks/parity.tf):
//!
//! ```text
//! parity/evals/<hydra-eval-id>/<key-digest>/…              (eval sets)
//! parity/campaigns/<campaign-id>/{campaign.json,progress.json,
//!   results.jsonl,buckets/,logs/,report/summary.md,…}      (campaigns)
//! ```

use anyhow::{Context, Result};

use super::S3_PREFIX;

/// Key of a campaign-scoped artifact, e.g. `progress.json` or
/// `report/summary.md`. Matches the engine's campaign sync layout
/// (`<prefix>/<campaign-id>/<rel>` with the engine's default prefix
/// `parity/campaigns`).
pub fn campaign_key(campaign_id: &str, rel: &str) -> String {
    format!("{S3_PREFIX}/campaigns/{campaign_id}/{rel}")
}

/// Prefix all eval sets for one Hydra eval live under (the eval CLI
/// appends its own `<key-digest>/` segment per eval set).
pub fn evals_prefix(hydra_eval_id: u64) -> String {
    format!("{S3_PREFIX}/evals/{hydra_eval_id}/")
}

/// List the immediate "subdirectory" segment names under `prefix` (which
/// must end with `/`): one ListObjectsV2 delimiter walk, returning each
/// CommonPrefix with the leading `prefix` and trailing `/` stripped.
/// `parity launch` uses it to discover the per-eval-set `<key-digest>/`
/// prefixes under [`evals_prefix`].
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
    fn evals_prefix_layout_matches_design() {
        assert_eq!(evals_prefix(1824219), "parity/evals/1824219/");
    }
}
