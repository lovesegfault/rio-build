//! Helpers all three providers (kind/k3s/eks) use.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand::{Rng, RngCore, distr::Alphanumeric};

use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::k8s::provider::BuiltImages;
use crate::sh::{self, cmd, repo_root, shell};
use crate::{git, kube, ui};

/// Number of docker images in `nix/docker.nix`'s dockerImages
/// linkFarm. All providers push this many (eks: ×2 arches + manifest;
/// k3s/kind: ×1 arch import/load). Bump when adding/removing an image.
pub const IMAGE_COUNT: u64 = 9;

/// Subcharts listed in Chart.yaml's `dependencies:`. Helm validates
/// charts/ against Chart.yaml BEFORE evaluating `condition: *.enabled`,
/// so every entry must be symlinked even when disabled for a given
/// provider (eks uses Aurora+S3, k3s uses Rook).
const SUBCHARTS: &[&str] = &["postgresql", "rustfs"];

/// Symlink all subcharts from their nix-store derivations into
/// `infra/helm/rio-build/charts/`. Gitignored.
pub fn chart_deps() -> Result<()> {
    let sh = shell()?;
    let charts = repo_root().join("infra/helm/rio-build/charts");
    std::fs::create_dir_all(&charts)?;
    for name in SUBCHARTS {
        let attr = format!(".#helm-{name}");
        let path = sh::read(cmd!(sh, "nix build --no-link --print-out-paths {attr}"))?;
        let link = charts.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(path.trim(), &link)?;
    }
    Ok(())
}

/// Build the dockerImages linkFarm for the host arch only.
/// Shared by k3s (`ctr import`) and kind (`kind load image-archive`) —
/// both run on the local machine so only need the host arch.
pub async fn build_host_arch(_cfg: &XtaskConfig) -> Result<BuiltImages> {
    let sh = shell()?;
    let repo = git::open()?;
    let tag = git::image_tag(&repo)?;

    let sys = match std::env::consts::ARCH {
        "x86_64" => "x86_64-linux",
        "aarch64" => "aarch64-linux",
        other => bail!("unsupported host arch: {other}"),
    };

    let dir = tempfile::tempdir()?;
    let link = dir.path().join("images");
    let link_s = link.to_str().unwrap();
    let attr = format!(".#packages.{sys}.dockerImages");

    ui::step(&format!("nix build {attr}"), || {
        sh::run(cmd!(sh, "nix build {attr} -L --out-link {link_s}"))
    })
    .await?;
    Ok(BuiltImages { dir, tag })
}

/// Create the two postgres Secrets for the in-cluster bitnami path
/// (kind/k3s). Generates a random password on first deploy; reuses
/// the existing one on upgrades so the DB doesn't get locked out.
///
/// - `rio-postgres-auth` key `password` — raw password, what bitnami
///   reads via `auth.existingSecret`
/// - `rio-postgres` key `url` — full connection URL, what store/
///   scheduler read via `RIO_DATABASE_URL` secretKeyRef
///
/// Keeps the password out of helm values (so `helm get values`
/// doesn't leak it) and out of git. EKS uses ESO for the same
/// contract; VM tests keep the hardcoded `rio` password via
/// `postgres-secret.yaml` (airgapped, xtask doesn't run there).
/// Write the `rio-gateway-ssh` Secret. When `tenant` is `None` and
/// `RIO_SSH_TENANT` is unset, preserves the existing Secret's comment
/// instead of clobbering to `default` (I-100: a bare `xtask deploy`
/// after smoke wrote `smoke-test` would otherwise break tenant routing
/// — `unknown tenant: default`). Falls through to [`crate::ssh::DEFAULT_TENANT`]
/// only on first deploy (Secret absent).
pub async fn ensure_gateway_ssh_secret(
    client: &kube::Client,
    cfg: &XtaskConfig,
    tenant: Option<&str>,
) -> Result<()> {
    let tenant = match tenant.or(cfg.ssh_tenant.as_deref()) {
        explicit @ Some(_) => explicit.map(str::to_owned),
        None => kube::get_secret_key(client, NS, "rio-gateway-ssh", "authorized_keys")
            .await?
            .as_deref()
            .and_then(crate::ssh::parse_tenant_comment),
    };
    let authorized = crate::ssh::authorized_keys(cfg, tenant.as_deref())?;
    kube::apply_secret(
        client,
        NS,
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), authorized)]),
    )
    .await
}

pub async fn ensure_pg_secrets(client: &kube::Client) -> Result<()> {
    let pass = match kube::get_secret_key(client, NS, "rio-postgres-auth", "password").await? {
        Some(p) => p,
        None => rand::rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect(),
    };
    kube::apply_secret(
        client,
        NS,
        "rio-postgres-auth",
        BTreeMap::from([("password".into(), pass.clone())]),
    )
    .await?;
    // Service name: bitnami's default is <release>-postgresql, release
    // is "rio". Subchart renders in the helm release namespace (rio-
    // system) — spelled out so the store's copy resolves across the ns
    // boundary. Password is alphanumeric-only so no urlencoding needed.
    let url = format!("postgres://rio:{pass}@rio-postgresql.{NS}:5432/rio");
    // ADR-019: store moved to rio-store. Secrets are ns-scoped, so the
    // store's Deployment can't read rio-system/rio-postgres. Duplicate
    // the url secret; scheduler reads the rio-system copy, store reads
    // the rio-store copy. Same connection string (postgresql Service
    // lives in rio-system either way).
    for ns in [NS, super::NS_STORE] {
        kube::apply_secret(
            client,
            ns,
            "rio-postgres",
            BTreeMap::from([("url".into(), url.clone())]),
        )
        .await?;
    }
    Ok(())
}

/// JWT keypair as base64'd helm `--set` values. Both 32 raw bytes → b64
/// string. `seed` goes in `jwt.signingSeed` (helm's `b64enc` double-
/// wraps for Secret.data; gateway decodes both layers — see
/// `jwt-signing-secret.yaml`). `pubkey` goes in `jwt.publicKey` →
/// ConfigMap data (single b64; verify-side decodes once).
pub struct JwtKeypair {
    pub seed: String,
    pub pubkey: String,
}

// r[impl gw.jwt.issue]
/// Generate or reuse the JWT ed25519 keypair for kind/k3s deploys.
/// Idempotent: first deploy generates a fresh 32-byte seed; subsequent
/// deploys read it back from the helm-rendered `rio-jwt-signing` Secret
/// so tokens stay valid across `xtask k8s deploy` reruns.
///
/// EKS skips this — production keys come from AWS Secrets Manager via
/// ESO (see `external-secrets.yaml`), not xtask.
///
/// Returns the b64 seed + derived b64 pubkey for passing to helm:
/// `--set jwt.enabled=true --set jwt.signingSeed=<seed> --set jwt.publicKey=<pubkey>`.
/// Both go through helm values (visible in `helm get values`) — fine for
/// dev clusters; the seed is ephemeral, the cluster is local. Contrast
/// with `ensure_pg_secrets` which avoids helm values because that DB
/// password can unlock persisted data.
pub async fn ensure_jwt_keypair(client: &kube::Client) -> Result<JwtKeypair> {
    let seed = match kube::get_secret_key(client, NS, "rio-jwt-signing", "ed25519_seed").await? {
        // helm-rendered Secret stores our b64 seed (k8s decodes the
        // outer Secret.data b64; what we read back is the operator's
        // b64 string — same one we passed in via --set).
        Some(s) => s,
        None => {
            let mut raw = [0u8; 32];
            rand::rng().fill_bytes(&mut raw);
            B64.encode(raw)
        }
    };
    // Derive pubkey from seed. `SigningKey::from_bytes` takes the raw
    // 32-byte seed; `verifying_key().to_bytes()` gives the 32-byte pub.
    // Both match what gateway/scheduler expect per `_helpers.tpl`.
    let raw: [u8; 32] = B64
        .decode(&seed)
        .context("rio-jwt-signing secret ed25519_seed is not valid base64")?
        .try_into()
        .map_err(|v: Vec<u8>| {
            anyhow::anyhow!(
                "rio-jwt-signing ed25519_seed decodes to {} bytes, expected 32",
                v.len()
            )
        })?;
    let sk = ed25519_dalek::SigningKey::from_bytes(&raw);
    let pubkey = B64.encode(sk.verifying_key().to_bytes());
    Ok(JwtKeypair { seed, pubkey })
}

/// Label selector matching every rio Deployment. Set by the chart's
/// `rio.labels` helper on every rendered resource; also set on
/// namespaces by `kube::ensure_namespace` (NetworkPolicy namespaceSelector
/// rules match by it).
pub const RIO_LABEL_SELECTOR: &str = "app.kubernetes.io/part-of=rio-build";

/// Rollout-restart every rio Deployment across all rio namespaces.
/// Called by kind/k3s after a same-tag `:dev` push: the Deployment spec
/// is unchanged (same image tag), so kube won't re-pull the image on its
/// own. EKS skips this — git-SHA tags change on every push, so helm
/// upgrade already triggers a rollout.
///
/// Restarts are fire-and-forget: no `wait_rollout` here. If the caller
/// wants to block until pods are healthy, `helm --wait` on the upgrade
/// already covers it (kind does this), or call `wait_rollout` per
/// deployment after.
pub async fn rollout_restart_rio(client: &kube::Client) -> Result<()> {
    let mut all = Vec::new();
    for &(ns, _) in super::NAMESPACES {
        let names = kube::rollout_restart_all(client, ns, RIO_LABEL_SELECTOR).await?;
        all.extend(names.into_iter().map(|n| format!("{ns}/{n}")));
    }
    tracing::info!(deployments = ?all, "rollout-restarted (same-tag push)");
    Ok(())
}

/// Guard that kills a child process on drop. Used for port-forward
/// and SSM tunnel processes in smoke tests.
pub struct ProcessGuard(pub tokio::process::Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify gw.jwt.issue]
    #[test]
    fn jwt_keypair_roundtrip() {
        // Fresh seed → b64 → decode → derive pubkey. Mirrors
        // ensure_jwt_keypair's derivation path without a kube client.
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let seed_b64 = B64.encode(raw);

        let decoded: [u8; 32] = B64.decode(&seed_b64).unwrap().try_into().unwrap();
        assert_eq!(decoded, raw);

        let sk = ed25519_dalek::SigningKey::from_bytes(&decoded);
        let pubkey_b64 = B64.encode(sk.verifying_key().to_bytes());
        // Pubkey decodes to 32 bytes (VerifyingKey::from_bytes contract).
        assert_eq!(B64.decode(&pubkey_b64).unwrap().len(), 32);

        // Deterministic: same seed → same pubkey.
        let sk2 = ed25519_dalek::SigningKey::from_bytes(&raw);
        assert_eq!(
            pubkey_b64,
            B64.encode(sk2.verifying_key().to_bytes()),
            "pubkey derivation must be deterministic for idempotent redeploys"
        );
    }

    #[test]
    fn jwt_seed_b64_length() {
        // 32 raw bytes → 44-char b64 string (no padding surprises).
        // Matches what operators pass via `openssl rand -base64 32`.
        let seed = B64.encode([0u8; 32]);
        assert_eq!(seed.len(), 44);
    }
}
