// SSH server implementation using russh

use anyhow::{Context, Result};
use rand_core::OsRng;
use russh::keys::ssh_encoding::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, CryptoVec};
use std::net::SocketAddr;
use std::path::Path;
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
#[derive(Clone)]
#[allow(dead_code)]
pub struct SshHandler {}

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
    pub async fn load_or_generate_host_key(path: Option<&Path>) -> Result<PrivateKey> {
        if let Some(key_path) = path {
            // Try to load existing key
            if key_path.exists() {
                info!("Loading SSH host key from {:?}", key_path);
                let key_data = tokio::fs::read(key_path)
                    .await
                    .context("Failed to read SSH host key file")?;

                let key =
                    PrivateKey::from_openssh(&key_data).context("Failed to parse SSH host key")?;

                info!("Successfully loaded SSH host key");
                return Ok(key);
            } else {
                info!("Host key not found at {:?}, generating new key", key_path);
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
                    .context("Failed to create host key directory")?;
            }

            // Save key in OpenSSH format
            let key_data = key
                .to_openssh(LineEnding::LF)
                .context("Failed to serialize SSH host key")?;

            tokio::fs::write(key_path, key_data)
                .await
                .context("Failed to write SSH host key file")?;

            // Set restrictive permissions on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = tokio::fs::metadata(key_path)
                    .await
                    .context("Failed to get key file metadata")?
                    .permissions();
                perms.set_mode(0o600);
                tokio::fs::set_permissions(key_path, perms)
                    .await
                    .context("Failed to set key file permissions")?;
            }

            info!("Saved SSH host key to {:?}", key_path);
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
