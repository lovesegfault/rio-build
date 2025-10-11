// SSH server implementation using russh

use anyhow::{Context, Result};
use russh::keys::{PrivateKey, PublicKey};
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
