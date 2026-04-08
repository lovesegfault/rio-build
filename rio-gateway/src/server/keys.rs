//! SSH host-key and `authorized_keys` loading + hot-reload watcher.
//!
//! Split out of `server/mod.rs` so the SSH connection-handling logic
//! doesn't carry ~400 lines of file-I/O + polling that has no russh
//! dependency. Everything here is `pub` and re-exported from
//! `server/mod.rs` so external paths (`rio_gateway::server::…` and the
//! crate-root re-exports in `lib.rs`) are unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use arc_swap::ArcSwap;
use rio_common::signal::Token as CancellationToken;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use tracing::{debug, info, warn};

/// Load or generate an SSH host key.
pub fn load_or_generate_host_key(path: &Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        info!(path = %path.display(), "loading SSH host key");
        let key = russh::keys::load_secret_key(path, None)
            .with_context(|| format!("failed to load host key from {}", path.display()))?;
        Ok(key)
    } else {
        warn!(
            path = %path.display(),
            "SSH host key not found, generating a new one (dev mode)"
        );
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .context("failed to generate host key")?;
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(
                error = %e,
                path = %parent.display(),
                "failed to create directory for host key; key will be ephemeral"
            );
        }
        if let Err(e) = std::fs::write(path, key.to_openssh(ssh_key::LineEnding::LF)?) {
            warn!(error = %e, "could not save generated host key (continuing with ephemeral key)");
        }
        Ok(key)
    }
}

// r[impl sec.boundary.ssh-auth]
/// Load authorized public keys from a file in standard `authorized_keys` format.
pub fn load_authorized_keys(path: &Path) -> anyhow::Result<Vec<PublicKey>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read authorized_keys from {}", path.display()))?;

    let mut keys = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.parse::<PublicKey>() {
            Ok(key) => {
                debug!(line = i + 1, "loaded authorized key");
                keys.push(key);
            }
            Err(e) => {
                warn!(
                    line = i + 1,
                    error = %e,
                    "skipping invalid authorized_keys entry"
                );
            }
        }
    }

    if keys.is_empty() {
        anyhow::bail!(
            "no valid authorized keys loaded from {}; server would reject all SSH connections",
            path.display()
        );
    }

    info!(count = keys.len(), "loaded authorized keys");
    Ok(keys)
}

/// Shared hot-swappable authorized-key set. `ArcSwap` so the auth path
/// (`.load()` per offered key — read-heavy) never blocks the watcher's
/// rare `.store()`. Wrapped in an outer `Arc` so the watcher task,
/// `GatewayServer`, and every `ConnectionHandler` share one instance.
pub type AuthorizedKeys = Arc<ArcSwap<Vec<PublicKey>>>;

/// Poll interval for [`spawn_authorized_keys_watcher`]. kubelet refreshes
/// projected-Secret mounts ~60s after the Secret changes; 10s polling
/// means the gateway picks the swap up within ~70s of `kubectl apply`.
/// Coarse on purpose — inotify on the file itself misses the kubelet
/// `..data` symlink swap, and inotify on the parent dir is more moving
/// parts than a 10s mtime poll for a file that changes ~never.
pub const AUTHORIZED_KEYS_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Watch `path` for content changes and hot-swap `keys` when it does.
///
/// I-109: previously the gateway loaded `authorized_keys` once at
/// startup. Rotating a tenant key (operator edits the K8s Secret) had
/// no effect until pod restart — the in-memory set was a startup
/// snapshot. Now: poll the file's mtime every `poll_interval`; on
/// change, re-parse and atomically swap. In-flight SSH handshakes see
/// the new set on their next `auth_publickey_offered` call (each
/// `.load()` reads the current `Arc`, not a per-connection snapshot).
///
/// **Why mtime polling, not inotify**: kubelet mounts Secrets via a
/// `..data → ..YYYY_MM_DD_hh_mm_ss.NNN/` symlink and refreshes by
/// atomically retargeting the symlink. An `IN_MODIFY` watch on the
/// file path itself is pinned to the OLD inode and never fires.
/// Watching the parent dir works but adds a dep + event-debounce
/// logic. `std::fs::metadata` follows symlinks, so the mtime we read
/// is the target file's — a swap shows up as a changed mtime. 10s
/// polling is negligible load and bounded latency.
///
/// **Reload failures keep the old set**: if the new file is empty,
/// all-invalid, or transiently unreadable mid-swap, log WARN and
/// retry next tick. Never swap to an empty set (would lock everyone
/// out until the next successful reload).
pub fn spawn_authorized_keys_watcher(
    keys: AuthorizedKeys,
    path: PathBuf,
    poll_interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Seed with the current mtime so the first tick doesn't
        // spuriously reload what `main` just loaded. If the initial
        // stat fails (file vanished between main's load and here —
        // vanishingly rare), `None` means the first successful stat
        // will trigger a reload, which is the safe outcome.
        let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut ticker = tokio::time::interval(poll_interval);
        // First tick fires immediately; we've already seeded mtime, so
        // skip it rather than special-casing the loop body.
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let mtime: Option<SystemTime> =
                match std::fs::metadata(&path).and_then(|m| m.modified()) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        warn!(path = %path.display(), error = %e,
                            "authorized_keys stat failed; keeping current set");
                        continue;
                    }
                };
            if mtime == last_mtime {
                continue;
            }
            match load_authorized_keys(&path) {
                Ok(new_keys) => {
                    info!(count = new_keys.len(), "reloaded authorized keys");
                    keys.store(Arc::new(new_keys));
                    last_mtime = mtime;
                }
                Err(e) => {
                    // Don't advance last_mtime — retry next tick. A
                    // half-written file (non-k8s deploys without atomic
                    // rename) will succeed once the writer finishes.
                    warn!(path = %path.display(), error = %e,
                        "authorized_keys reload failed; keeping current set");
                }
            }
        }
    })
}

// r[verify sec.boundary.ssh-auth]
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Generate a fresh ed25519 public key line for authorized_keys fixtures.
    fn make_valid_pubkey_line() -> anyhow::Result<String> {
        Ok(PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?
            .public_key()
            .to_openssh()?)
    }

    // -----------------------------------------------------------------------
    // load_authorized_keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_authorized_keys_valid_with_comments_and_blanks() -> anyhow::Result<()> {
        let key1 = make_valid_pubkey_line()?;
        let key2 = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), format!("# comment line\n\n{key1}\n{key2}\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 2);
        Ok(())
    }

    // r[verify gw.auth.tenant-from-key-comment]
    /// Key comments in authorized_keys lines are preserved by the parser
    /// and readable via `.comment()`. This is the mechanism for tenant
    /// name extraction in `auth_publickey`.
    #[test]
    fn test_load_authorized_keys_preserves_comment() -> anyhow::Result<()> {
        let key_base = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        // OpenSSH authorized_keys format: <type> <base64> <comment>
        std::fs::write(tmp.path(), format!("{key_base} team-infra\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0].comment(),
            "team-infra",
            "comment field should be preserved from the authorized_keys line"
        );
        Ok(())
    }

    /// Key with NO comment → empty comment string. This is the
    /// single-tenant mode case: empty tenant_name → scheduler gets
    /// empty string → scheduler resolves tenant_id=None.
    #[test]
    fn test_load_authorized_keys_no_comment_is_empty() -> anyhow::Result<()> {
        let key_base = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), format!("{key_base}\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0].comment(),
            "",
            "no comment → empty string (single-tenant mode)"
        );
        Ok(())
    }

    #[test]
    fn test_load_authorized_keys_skips_invalid_entry() -> anyhow::Result<()> {
        let key1 = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), format!("{key1}\nthis is not a valid ssh key\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 1, "invalid line should be skipped");
        Ok(())
    }

    #[test]
    fn test_load_authorized_keys_all_invalid_bails() -> anyhow::Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), "garbage line 1\ngarbage line 2\n")?;

        let err = load_authorized_keys(tmp.path()).expect_err("should bail");
        assert!(
            err.to_string().contains("no valid authorized keys"),
            "got: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_load_authorized_keys_missing_file() {
        let err = load_authorized_keys(Path::new("/nonexistent/rio-test-authkeys"))
            .expect_err("should fail on missing file");
        assert!(err.to_string().contains("failed to read"));
    }

    // -----------------------------------------------------------------------
    // load_or_generate_host_key
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_or_generate_host_key_generates_and_persists() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let key_path = tmp.path().join("subdir/host_key");

        // First call: generates and writes
        let k1 = load_or_generate_host_key(&key_path).expect("should generate");
        assert!(key_path.exists(), "key should be persisted");
        let fp1 = k1.public_key().fingerprint(Default::default());

        // Second call: loads the same key
        let k2 = load_or_generate_host_key(&key_path).expect("should load");
        let fp2 = k2.public_key().fingerprint(Default::default());
        assert_eq!(fp1.to_string(), fp2.to_string(), "same key on reload");
        Ok(())
    }

    #[test]
    fn test_load_or_generate_host_key_loads_existing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let key_path = tmp.path().join("host_key");

        // Write a key manually
        let orig = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        std::fs::write(&key_path, orig.to_openssh(ssh_key::LineEnding::LF)?)?;
        let orig_fp = orig.public_key().fingerprint(Default::default());

        let loaded = load_or_generate_host_key(&key_path).expect("should load");
        assert_eq!(
            loaded
                .public_key()
                .fingerprint(Default::default())
                .to_string(),
            orig_fp.to_string()
        );
        Ok(())
    }

    /// When the directory is unwritable, the key is still generated (ephemeral)
    /// but NOT persisted. The function returns Ok and logs a warning.
    #[test]
    fn test_load_or_generate_host_key_unwritable_dir_ephemeral() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        // Make the tempdir read-only so write fails. The key_path itself
        // doesn't exist so create_dir_all won't hit the read-only perms
        // (parent already exists) but fs::write will.
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555))?;
        let key_path = tmp.path().join("host_key");

        let result = load_or_generate_host_key(&key_path);

        // Restore perms so tempdir cleanup works regardless of outcome.
        let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755));

        let _key = result.expect("should return ephemeral key despite write failure");
        assert!(
            !key_path.exists(),
            "key should NOT be persisted (write failed)"
        );
        Ok(())
    }
}
