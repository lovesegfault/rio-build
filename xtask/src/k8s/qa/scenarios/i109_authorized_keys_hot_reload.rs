//! I-109: gateway hot-reloads `authorized_keys` Secret within ~70s
//! (kubelet ~60s Secret refresh + gateway's 10s mtime poll), no
//! restart required.
//!
//! Append a fresh key, poll the gateway log for the load message
//! reflecting the new count, then strip the key.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{NS_SYSTEM, first_pod, logs_since_contain, poll_until};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::k8s::{client as kube, shared};
use crate::ssh;

pub struct AuthorizedKeysHotReload;

#[async_trait]
impl Scenario for AuthorizedKeysHotReload {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i109-authorized-keys-hot-reload",
            i_ref: Some(109),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Gateway],
            },
            timeout: Duration::from_secs(150),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let gw_pod = first_pod(ctx, NS_SYSTEM, "rio-gateway")?;

        let before =
            kube::get_secret_key(&ctx.kube, NS_SYSTEM, "rio-gateway-ssh", "authorized_keys")
                .await?
                .unwrap_or_default();
        let before_count = before.lines().filter(|l| !l.trim().is_empty()).count();

        let comment = "qa-i109-probe";
        let (_, pub_line) = ssh::generate(comment)?;
        shared::merge_authorized_keys_batch(&ctx.kube, &[&pub_line]).await?;

        // r[gw.keys.hot-reload]: kubelet ~60s + gateway 10s poll.
        let want = before_count + 1;
        let reloaded = poll_until(Duration::from_secs(90), Duration::from_secs(5), || async {
            let lines = logs_since_contain(ctx, NS_SYSTEM, &gw_pod, 95, "loaded authorized keys")?;
            // Any line whose count >= want proves the new key was seen.
            Ok(lines
                .iter()
                .filter_map(|l| {
                    l.split("count").nth(1).and_then(|s| {
                        s.chars()
                            .filter(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse::<usize>()
                            .ok()
                    })
                })
                .any(|n| n >= want)
                .then_some(()))
        })
        .await?;

        // Cleanup regardless of verdict.
        shared::remove_authorized_keys_by_comment_prefix(&ctx.kube, comment).await?;

        if reloaded.is_some() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "gateway did not log reload to count≥{want} within 90s — \
                 authorized_keys hot-reload not firing"
            )))
        }
    }
}
