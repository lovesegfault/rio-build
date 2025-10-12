// SSH server implementation using russh

use anyhow::{Context, Result};
use camino::Utf8Path;
use rand_core::OsRng;
use russh::keys::ssh_encoding::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, CryptoVec};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

/// SSH server configuration
#[derive(Clone)]
#[allow(dead_code)]
pub struct SshConfig {
    pub addr: SocketAddr,
    pub host_key: PrivateKey,
}

/// Handler for SSH sessions
///
/// Each SSH session will have its own DispatcherStore instance and protocol adapter.
/// We need to bridge russh's callback-based API with the async stream-based
/// Nix daemon protocol.
#[derive(Clone)]
#[allow(dead_code)]
pub struct SshHandler {
    // TODO: Will need to create per-session state for protocol adapter
}

#[allow(dead_code)]
impl SshHandler {
    pub fn new() -> Self {
        Self {}
    }
}

/// SSH server wrapper
#[allow(dead_code)]
pub struct SshServer {
    config: SshConfig,
}

#[allow(dead_code)]
impl SshServer {
    pub fn new(config: SshConfig) -> Self {
        Self { config }
    }

    /// Generate or load SSH host key
    pub async fn load_or_generate_host_key(path: Option<&Utf8Path>) -> Result<PrivateKey> {
        if let Some(key_path) = path {
            // Try to load existing key
            if key_path.exists() {
                info!("Loading SSH host key from {}", key_path);
                let key_data = tokio::fs::read(key_path)
                    .await
                    .with_context(|| format!("Failed to read SSH host key file: {}", key_path))?;

                let key = PrivateKey::from_openssh(&key_data).with_context(|| {
                    format!("Failed to parse SSH host key from file: {}", key_path)
                })?;

                info!("Successfully loaded SSH host key from {}", key_path);
                return Ok(key);
            } else {
                info!("Host key not found at {}, generating new key", key_path);
            }
        } else {
            info!("No host key path specified, generating temporary key");
        }

        // Generate new Ed25519 key
        info!("Generating new Ed25519 SSH host key");
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
            .context("Failed to generate SSH host key")?;

        // Save key if path was provided
        if let Some(key_path) = path {
            // Create parent directory if needed
            if let Some(parent) = key_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("Failed to create host key directory: {}", parent))?;
            }

            // Save key in OpenSSH format
            let key_data = key
                .to_openssh(LineEnding::LF)
                .context("Failed to serialize SSH host key to OpenSSH format")?;

            tokio::fs::write(key_path, key_data)
                .await
                .with_context(|| format!("Failed to write SSH host key file: {}", key_path))?;

            // Set restrictive permissions on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let metadata = tokio::fs::metadata(key_path).await.with_context(|| {
                    format!("Failed to get file metadata for key file: {}", key_path)
                })?;
                let mut new_perms = metadata.permissions();
                new_perms.set_mode(0o600);
                tokio::fs::set_permissions(key_path, new_perms)
                    .await
                    .with_context(|| {
                        format!("Failed to set permissions (0600) on key file: {}", key_path)
                    })?;
            }

            info!("Saved SSH host key to {}", key_path);
        }

        Ok(key)
    }

    /// Start the SSH server
    pub async fn start(self, handler: SshHandler) -> Result<()> {
        info!("Starting SSH server on {}", self.config.addr);

        let config = Arc::new(russh::server::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            auth_rejection_time: std::time::Duration::from_secs(3),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![self.config.host_key],
            ..Default::default()
        });

        // Bind TCP listener
        let socket = TcpListener::bind(&self.config.addr)
            .await
            .context("Failed to bind SSH server socket")?;

        info!("SSH server listening on {}", self.config.addr);

        let mut sh = handler.clone();
        sh.run_on_socket(config, &socket)
            .await
            .context("SSH server failed")?;

        Ok(())
    }
}

impl server::Server for SshHandler {
    type Handler = Self;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        debug!("New SSH client connection");
        self.clone()
    }
}

impl server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        info!("Channel open session request (channel_id={})", channel.id());
        Ok(true)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        info!("Public key auth attempt for user: {}", user);

        // TODO: Implement proper public key authentication
        // For now, accept all keys for development
        warn!("Accepting all public keys (development mode)");

        Ok(Auth::Accept)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Received {} bytes on channel {}", data.len(), channel);

        // TODO: Forward data to Nix protocol handler
        // For now, just echo it back for testing
        session.data(channel, CryptoVec::from(data.to_vec()))?;

        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        info!("Channel {} closed", channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Channel {} EOF", channel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_generate_temporary_key() {
        // Should generate a key without saving to disk
        let key = SshServer::load_or_generate_host_key(None)
            .await
            .expect("Failed to generate temporary key");

        // Verify we got a valid key that can be serialized
        let serialized = key
            .to_openssh(LineEnding::LF)
            .expect("Should be able to serialize key");
        assert!(!serialized.is_empty());
    }

    #[tokio::test]
    async fn test_generate_and_save_key() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let key_path = Utf8Path::from_path(temp_dir.path())
            .expect("Path is not UTF-8")
            .join("test_host_key");

        // Generate and save key
        let key1 = SshServer::load_or_generate_host_key(Some(&key_path))
            .await
            .expect("Failed to generate key");

        // Verify file was created
        assert!(key_path.exists(), "Key file should exist");

        // Verify file content is valid OpenSSH format
        let content = std::fs::read_to_string(key_path.as_std_path())
            .expect("Should be able to read key file");
        assert!(content.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));

        // Verify permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata =
                std::fs::metadata(key_path.as_std_path()).expect("Failed to get metadata");
            let mode = metadata.permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "Key file should have 0600 permissions");
        }

        // Load the key back
        let key2 = SshServer::load_or_generate_host_key(Some(&key_path))
            .await
            .expect("Failed to load key");

        // Verify both keys can be serialized (they're valid)
        let ser1 = key1
            .to_openssh(LineEnding::LF)
            .expect("Key1 should serialize");
        let ser2 = key2
            .to_openssh(LineEnding::LF)
            .expect("Key2 should serialize");

        // The serialized keys should be identical
        assert_eq!(
            ser1.as_str(),
            ser2.as_str(),
            "Loaded key should match saved key"
        );
    }

    #[tokio::test]
    async fn test_load_nonexistent_key_generates_new() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let key_path = Utf8Path::from_path(temp_dir.path())
            .expect("Path is not UTF-8")
            .join("nonexistent_key");

        // Should generate a new key since file doesn't exist
        let key = SshServer::load_or_generate_host_key(Some(&key_path))
            .await
            .expect("Failed to generate key");

        assert!(key_path.exists(), "Key file should have been created");

        // Verify it's a valid key
        let serialized = key
            .to_openssh(LineEnding::LF)
            .expect("Should be able to serialize key");
        assert!(!serialized.is_empty());
    }

    #[tokio::test]
    async fn test_create_parent_directory() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let key_path = Utf8Path::from_path(temp_dir.path())
            .expect("Path is not UTF-8")
            .join("nested")
            .join("directories")
            .join("host_key");

        // Should create parent directories automatically
        let _key = SshServer::load_or_generate_host_key(Some(&key_path))
            .await
            .expect("Failed to generate key with nested directories");

        assert!(key_path.exists(), "Key file should exist");
        assert!(
            key_path.parent().unwrap().exists(),
            "Parent directory should exist"
        );
    }
}
