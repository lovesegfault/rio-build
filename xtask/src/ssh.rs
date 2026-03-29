//! SSH pubkey validation. Replaces `infra/lib/ssh-key.sh`.
//!
//! The gateway maps the authorized_keys comment field to tenant_name
//! (rio-gateway/src/server.rs); the scheduler resolves it against the
//! `tenants` table. Empty comment → single-tenant mode (no PG lookup).

use std::path::PathBuf;

use anyhow::{Context, Result};
use ssh_key::{LineEnding, PublicKey};

use crate::config::XtaskConfig;

/// Default tenant name written to the authorized_keys comment when
/// neither `--tenant` nor `RIO_SSH_TENANT` is set. Matches the tenant
/// the operator is expected to bootstrap (e.g. via `rio-cli
/// create-tenant default` or the bootstrap-job).
///
/// Was empty-string (→ single-tenant mode) pre-P0477. Changed because
/// JWT-enabled deploys (now the kind/k3s default) need a non-empty
/// `sub` claim — the gateway can't mint a token for an anonymous
/// session (see `r[gw.jwt.issue]`).
pub const DEFAULT_TENANT: &str = "default";

// r[impl sched.tenant.resolve]
/// Read, validate, and comment-adjust the user's SSH pubkey.
///
/// `ssh-key` refuses to parse private keys as public, so the bash's
/// `grep '^ssh-'` heuristic is unnecessary — a private-key path gives
/// a clean parse error here.
///
/// The comment field becomes the gateway's `tenant_name` — precedence:
/// `tenant` arg (from `--tenant`) > `cfg.ssh_tenant` (from
/// `RIO_SSH_TENANT`) > [`DEFAULT_TENANT`].
pub fn authorized_keys(cfg: &XtaskConfig, tenant: Option<&str>) -> Result<String> {
    let path = pubkey_path(cfg);

    let mut key = PublicKey::read_openssh_file(&path).with_context(|| {
        format!(
            "{} is not a valid OpenSSH public key\n\
             (set RIO_SSH_PUBKEY in .env.local — and check you didn't point at the private key)",
            path.display()
        )
    })?;

    let tenant = tenant
        .or(cfg.ssh_tenant.as_deref())
        .unwrap_or(DEFAULT_TENANT);
    key.set_comment(tenant);
    Ok(key.to_openssh().map(|s| s + "\n")?)
}

/// Resolve RIO_SSH_PUBKEY with tilde-expansion. dotenv loaders
/// (direnv, dotenvy) don't expand `~` — `RIO_SSH_PUBKEY=~/.ssh/foo.pub`
/// arrives as a literal `~`. Default: `~/.ssh/id_ed25519.pub`.
pub fn pubkey_path(cfg: &XtaskConfig) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    match cfg.ssh_pubkey.clone() {
        Some(p) => match p.strip_prefix("~") {
            Ok(rest) => home().join(rest),
            Err(_) => p,
        },
        None => home().join(".ssh/id_ed25519.pub"),
    }
}

/// Private key paired with RIO_SSH_PUBKEY. Strips `.pub` if present;
/// if the pubkey path has no extension (e.g. `coder` not `coder.pub`),
/// checks for `<path>.pub` to confirm it's actually the pubkey, then
/// the sibling file is the private key.
pub fn privkey_path(cfg: &XtaskConfig) -> Result<PathBuf> {
    let pub_path = pubkey_path(cfg);
    let priv_path = if pub_path.extension().is_some_and(|e| e == "pub") {
        pub_path.with_extension("")
    } else {
        // No .pub extension — assume RIO_SSH_PUBKEY already points at
        // the pubkey by convention and the private key is the same
        // name minus any suffix the user uses. Try as-is first.
        pub_path.clone()
    };
    anyhow::ensure!(
        priv_path.exists(),
        "private key {} not found\n\
         (RIO_SSH_PUBKEY={} — private key should be the same path without .pub)",
        priv_path.display(),
        pub_path.display()
    );
    Ok(priv_path)
}

/// Generate a fresh ed25519 keypair with the given comment.
/// Returns (private_openssh, public_openssh).
pub fn generate(comment: &str) -> Result<(String, String)> {
    let priv_key =
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)?;
    let mut pub_key = priv_key.public_key().clone();
    pub_key.set_comment(comment);
    Ok((
        priv_key.to_openssh(LineEnding::LF)?.to_string(),
        pub_key.to_openssh()? + "\n",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_tenant(ssh_tenant: Option<&str>) -> XtaskConfig {
        XtaskConfig {
            ssh_tenant: ssh_tenant.map(String::from),
            ..Default::default()
        }
    }

    /// authorized_keys needs a real pubkey file. Generate one into a
    /// tempdir and point ssh_pubkey at it.
    fn cfg_with_key(ssh_tenant: Option<&str>) -> (tempfile::TempDir, XtaskConfig) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id.pub");
        let (_, pub_key) = generate("original-comment").unwrap();
        std::fs::write(&path, pub_key).unwrap();
        let mut cfg = cfg_with_tenant(ssh_tenant);
        cfg.ssh_pubkey = Some(path);
        (dir, cfg)
    }

    // r[verify sched.tenant.resolve]
    #[test]
    fn authorized_keys_default_tenant() {
        let (_d, cfg) = cfg_with_key(None);
        let out = authorized_keys(&cfg, None).unwrap();
        // Comment is the third whitespace-separated field.
        let comment = out.split_whitespace().nth(2);
        assert_eq!(comment, Some(DEFAULT_TENANT));
    }

    #[test]
    fn authorized_keys_tenant_precedence() {
        let (_d, cfg) = cfg_with_key(Some("from-env"));
        // arg overrides cfg
        let out = authorized_keys(&cfg, Some("from-flag")).unwrap();
        assert_eq!(out.split_whitespace().nth(2), Some("from-flag"));
        // cfg overrides default
        let out = authorized_keys(&cfg, None).unwrap();
        assert_eq!(out.split_whitespace().nth(2), Some("from-env"));
    }
}
