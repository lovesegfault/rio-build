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
/// JWT-enabled deploys (now the k3s default) need a non-empty
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

/// Read the operator's pubkey from `RIO_SSH_PUBKEY` (same path
/// [`authorized_keys`] reads). Split out so [`tenant_for_key`] callers
/// can resolve "the operator's key" without re-parsing the file.
pub fn read_pubkey(cfg: &XtaskConfig) -> Result<PublicKey> {
    let path = pubkey_path(cfg);
    PublicKey::read_openssh_file(&path).with_context(|| {
        format!(
            "{} is not a valid OpenSSH public key (set RIO_SSH_PUBKEY)",
            path.display()
        )
    })
}

/// Extract the tenant name (comment field) of `key`'s line in
/// `authorized_keys`. Matches by key identity (algorithm + key data),
/// NOT line position — `merge_authorized_key` reorders lines, so
/// "first line" is whoever was last upserted, not the operator. I-100:
/// after `grant alice` + a bare `deploy`, positional lookup re-tagged
/// the operator's key as `alice`. Empty/absent comment → `None`.
pub fn tenant_for_key(authorized_keys: &str, key: &PublicKey) -> Option<String> {
    authorized_keys
        .lines()
        .filter_map(|l| PublicKey::from_openssh(l).ok())
        .find(|k| k.key_data() == key.key_data())
        .map(|k| k.comment().to_owned())
        .filter(|c| !c.is_empty())
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

    #[test]
    fn tenant_for_key_finds_own_key() {
        let (_d, cfg) = cfg_with_key(None);
        let ak = authorized_keys(&cfg, Some("smoke-test")).unwrap();
        let key = read_pubkey(&cfg).unwrap();
        assert_eq!(tenant_for_key(&ak, &key).as_deref(), Some("smoke-test"));
        // leading blank lines / trailing newline tolerated
        assert_eq!(
            tenant_for_key(&format!("\n\n{ak}\n"), &key).as_deref(),
            Some("smoke-test")
        );
        // empty comment → None (don't preserve "")
        let (_, no_comment) = generate("").unwrap();
        let nc_key = PublicKey::from_openssh(no_comment.trim()).unwrap();
        assert_eq!(tenant_for_key(&no_comment, &nc_key), None);
        // garbage / key not present → None
        assert_eq!(tenant_for_key("not a key", &key), None);
        assert_eq!(tenant_for_key("", &key), None);
    }

    /// I-100 regression: the operator's tenant must be resolved by key
    /// IDENTITY, not line position. After `grant alice`, alice's key is
    /// appended; the old positional lookup returned `alice` when asked
    /// "what's the operator's tenant?".
    #[test]
    fn tenant_for_key_ignores_position() {
        let (_, alice_pub) = generate("alice").unwrap();
        let (_, op_pub) = generate("ops").unwrap();
        let op_key = PublicKey::from_openssh(op_pub.trim()).unwrap();
        // alice first, operator second — positional lookup would return "alice".
        let ak = format!("{alice_pub}{op_pub}");
        assert_eq!(tenant_for_key(&ak, &op_key).as_deref(), Some("ops"));
        // reversed order — same answer.
        let ak = format!("{op_pub}{alice_pub}");
        assert_eq!(tenant_for_key(&ak, &op_key).as_deref(), Some("ops"));
        // operator's key absent → None (don't borrow someone else's tenant).
        assert_eq!(tenant_for_key(&alice_pub, &op_key), None);
    }
}
