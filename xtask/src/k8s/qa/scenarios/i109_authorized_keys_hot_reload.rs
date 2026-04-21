//! I-109: gateway hot-reloads `authorized_keys` Secret within ~70s
//! (kubelet ~60s Secret refresh + gateway's 10s mtime poll), no
//! restart required.
//!
//! Append a fresh key, poll the gateway log for the load message
//! reflecting the new count, then strip the key.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{NS_SYSTEM, poll_until};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::k8s::shared;
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
        // Functional assertion (not log-count grep): append a fresh key,
        // poll until an SSH connection USING that key is accepted. The
        // log-count approach raced (Secret count vs gateway's loaded
        // count drift across runs; i064 restarts; kubelet lag on PRIOR
        // changes). "Key accepted" is the user-observable behavior
        // I-109 actually guarantees.
        let comment = "qa-i109-probe";
        let (priv_pem, pub_line) = ssh::generate(comment)?;
        shared::merge_authorized_keys_batch(&ctx.kube, &[&pub_line]).await?;
        // ssh::generate returns the PEM BYTES, not a path. Write to a
        // 0600 temp file (ssh refuses group/other-readable keys).
        use std::os::unix::fs::PermissionsExt;
        let key_dir = tempfile::tempdir()?;
        let key = key_dir.path().join("i109");
        std::fs::write(&key, priv_pem)?;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        let key = key.display().to_string();

        let (port, _guard) = shared::port_forward(NS_SYSTEM, "svc/rio-gateway", 0, 22).await?;
        // The gateway is an ssh-ng store protocol server, NOT a shell —
        // `ssh ... cmd` authenticates then hangs (no exec channel).
        // `nix store ping` is the actual user flow: SSH auth + ssh-ng
        // handshake. NIX_SSHOPTS gets BatchMode/IdentitiesOnly so a
        // key-reject fails fast.
        let store = format!("ssh-ng://rio@localhost:{port}?ssh-key={key}");
        let sshopts = format!(
            "{} -o ConnectTimeout=5",
            crate::k8s::shared::NIX_SSHOPTS_BASE
        );
        // r[gw.keys.hot-reload]: kubelet ≤60s + gateway 10s poll = ~70s
        // ceiling. 120s gives 50s slack for phase-2 contention.
        let accepted = poll_until(Duration::from_secs(120), Duration::from_secs(5), || async {
            let s = crate::sh::shell()?;
            let ok = crate::sh::try_read(
                crate::sh::cmd!(s, "timeout 10 nix store ping --store {store}")
                    .env("NIX_SSHOPTS", &sshopts),
            )
            .is_ok();
            Ok(ok.then_some(()))
        })
        .await?;

        // Cleanup regardless of verdict.
        shared::remove_authorized_keys_by_comment_prefix(&ctx.kube, comment).await?;

        if accepted.is_some() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(
                "gateway did not accept the newly-appended key within 120s — \
                 authorized_keys hot-reload not firing"
                    .into(),
            ))
        }
    }
}
