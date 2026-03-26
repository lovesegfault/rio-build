//! SSH pubkey validation. Replaces `infra/lib/ssh-key.sh`.
//!
//! The gateway maps the authorized_keys comment field to tenant_name
//! (rio-gateway/src/server.rs); the scheduler resolves it against the
//! `tenants` table. Empty comment → single-tenant mode (no PG lookup).

use std::path::PathBuf;

use anyhow::{Context, Result};
use ssh_key::{LineEnding, PublicKey};

use crate::config::XtaskConfig;

/// Read, validate, and comment-adjust the user's SSH pubkey.
///
/// `ssh-key` refuses to parse private keys as public, so the bash's
/// `grep '^ssh-'` heuristic is unnecessary — a private-key path gives
/// a clean parse error here.
pub fn authorized_keys(cfg: &XtaskConfig) -> Result<String> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    // dotenv loaders (direnv, dotenvy) don't tilde-expand —
    // `RIO_SSH_PUBKEY=~/.ssh/foo.pub` arrives as a literal `~`.
    let path = match cfg.ssh_pubkey.clone() {
        Some(p) => match p.strip_prefix("~") {
            Ok(rest) => home().join(rest),
            Err(_) => p,
        },
        None => home().join(".ssh/id_ed25519.pub"),
    };

    let mut key = PublicKey::read_openssh_file(&path).with_context(|| {
        format!(
            "{} is not a valid OpenSSH public key\n\
             (set RIO_SSH_PUBKEY in .env.local — and check you didn't point at the private key)",
            path.display()
        )
    })?;

    key.set_comment(cfg.ssh_tenant.as_deref().unwrap_or(""));
    Ok(key.to_openssh().map(|s| s + "\n")?)
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
