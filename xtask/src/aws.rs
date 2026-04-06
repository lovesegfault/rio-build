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

use aws_config::{Region, SdkConfig, identity::IdentityCache};
use tokio::sync::OnceCell;

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
