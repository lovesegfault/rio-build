//! Shared AWS SDK config loader.
//!
//! Every `aws_config::from_env().load()` call hits the credential
//! provider chain (SSO cache / IMDS / profile parse). When several
//! up-phases run inside one process — or concurrently across the
//! both-arch AMI build — each one re-resolves credentials, and the
//! lazy identity cache's default 5s `load_timeout` trips under that
//! contention (I-191). Load once, share the `SdkConfig`, and give
//! credential resolution a generous ceiling.

use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::{Region, SdkConfig, identity::IdentityCache};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::JoinSet;
use tracing::info;

/// SSO/IMDS credential resolution can take >5s under contention
/// (concurrent up-phases each hitting the SSO cache). 30s is generous
/// but bounded — a genuinely broken credential setup still fails.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Load AWS SDK config once and share. Region from `region` if `Some`,
/// else `AWS_REGION` / `AWS_DEFAULT_REGION` / profile.
///
/// The cache is process-global and first-call-wins on region. In
/// practice every xtask phase uses the single tofu-output region, so
/// the key collapses; if a future caller genuinely needs a second
/// region, swap the `OnceCell` for a `Mutex<HashMap<Option<String>,
/// SdkConfig>>`.
pub async fn config(region: Option<&str>) -> &'static SdkConfig {
    static CACHE: OnceCell<SdkConfig> = OnceCell::const_new();
    let region = region.map(str::to_owned);
    CACHE
        .get_or_init(|| async move {
            let mut b = aws_config::from_env()
                .identity_cache(IdentityCache::lazy().load_timeout(LOAD_TIMEOUT).build());
            if let Some(r) = region {
                b = b.region(Region::new(r));
            }
            b.load().await
        })
        .await
}

/// Delete every object in `bucket`. Paginates `ListObjectsV2` (1000
/// keys/page), streams pages into a bounded-concurrency `DeleteObjects`
/// fan-out (1000 keys/call, max per S3 API). Bucket itself is left
/// intact.
///
/// `aws s3 rm --recursive` is too slow at >100K objects; lifecycle
/// rules are async (≤24h to fire) so unsuitable for a synchronous
/// `up --wipe`. This is the lister→batched-DeleteObjects pattern at ~9K
/// obj/s with `concurrency=8`.
pub async fn empty_bucket(region: &str, bucket: &str) -> Result<usize> {
    const CONCURRENCY: usize = 8;
    // Adaptive retry: SDK does client-side token-bucket rate limiting
    // when it sees throttling responses (SlowDown 503). At 8×1000-key
    // DeleteObjects in flight, a bucket whose prefix partitioning
    // hasn't auto-scaled yet returns SlowDown — standard mode's 3
    // attempts exhaust under sustained throttle. Adaptive backs the
    // whole client off, and 16 attempts (~30s of backoff at the cap)
    // rides out the partition scale-up.
    use aws_config::retry::RetryConfig;
    let s3 = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(config(Some(region)).await)
            .retry_config(RetryConfig::adaptive().with_max_attempts(16))
            .build(),
    );
    let sem = std::sync::Arc::new(Semaphore::new(CONCURRENCY));
    let mut tasks: JoinSet<Result<usize>> = JoinSet::new();

    let mut pages = s3.list_objects_v2().bucket(bucket).into_paginator().send();
    while let Some(page) = pages.next().await {
        let page = page.with_context(|| format!("ListObjectsV2 on s3://{bucket}"))?;
        let objs: Vec<ObjectIdentifier> = page
            .contents()
            .iter()
            .filter_map(|o| o.key())
            .map(|k| {
                ObjectIdentifier::builder()
                    .key(k)
                    .build()
                    .expect("key is set")
            })
            .collect();
        if objs.is_empty() {
            continue;
        }
        let n = objs.len();
        let s3 = s3.clone();
        let bucket = bucket.to_owned();
        let permit = sem.clone().acquire_owned().await.expect("never closed");
        tasks.spawn(async move {
            let _permit = permit;
            s3.delete_objects()
                .bucket(&bucket)
                .delete(
                    Delete::builder()
                        .set_objects(Some(objs))
                        .quiet(true)
                        .build()
                        .expect("objects is set"),
                )
                .send()
                .await
                .with_context(|| format!("DeleteObjects on s3://{bucket}"))?;
            Ok(n)
        });
    }

    let mut deleted = 0usize;
    while let Some(r) = tasks.join_next().await {
        deleted += r.expect("delete task panicked")?;
    }
    info!("emptied s3://{bucket}: {deleted} objects deleted");
    Ok(deleted)
}
