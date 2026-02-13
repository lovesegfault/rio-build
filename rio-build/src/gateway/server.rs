//! SSH server using `russh` that terminates connections and speaks the
//! Nix worker protocol on each session channel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Handler, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use super::session::run_protocol;
use crate::store::Store;

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
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
            .context("failed to generate host key")?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(path, key.to_openssh(ssh_key::LineEnding::LF)?) {
            warn!(error = %e, "could not save generated host key (continuing with ephemeral key)");
        }
        Ok(key)
    }
}

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

    info!(count = keys.len(), "loaded authorized keys");
    Ok(keys)
}

/// The SSH server that accepts connections and spawns protocol sessions.
pub struct GatewayServer {
    store: Arc<dyn Store>,
    authorized_keys: Arc<Vec<PublicKey>>,
}

impl GatewayServer {
    pub fn new(store: Arc<dyn Store>, authorized_keys: Vec<PublicKey>) -> Self {
        GatewayServer {
            store,
            authorized_keys: Arc::new(authorized_keys),
        }
    }

    /// Start the SSH server on the given address.
    pub async fn run(mut self, host_key: PrivateKey, addr: SocketAddr) -> anyhow::Result<()> {
        let config = russh::server::Config {
            keys: vec![host_key],
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            auth_rejection_time: std::time::Duration::from_secs(1),
            ..Default::default()
        };
        let config = Arc::new(config);

        info!(addr = %addr, "starting SSH server");

        let socket = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind SSH server to {addr}"))?;

        self.run_on_socket(config, &socket).await?;
        Ok(())
    }
}

impl russh::server::Server for GatewayServer {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        info!(peer = ?peer_addr, "new SSH connection");
        ConnectionHandler {
            peer_addr,
            store: Arc::clone(&self.store),
            authorized_keys: Arc::clone(&self.authorized_keys),
            pending_channels: HashMap::new(),
            active_sessions: HashMap::new(),
        }
    }
}

/// Per-connection handler that manages SSH channels.
///
/// Lifecycle for each channel:
/// 1. `channel_open_session` — accept channel, store it as pending
/// 2. `exec_request("nix-daemon --stdio")` — start the protocol bridge
/// 3. `data` — forward bytes to the protocol handler (only for channels
///    where exec_request hasn't been received yet; after that the bridge task
///    reads via `Channel::wait()`)
pub struct ConnectionHandler {
    peer_addr: Option<SocketAddr>,
    store: Arc<dyn Store>,
    authorized_keys: Arc<Vec<PublicKey>>,
    /// Channels awaiting an exec request before starting the protocol.
    pending_channels: HashMap<ChannelId, Channel<Msg>>,
    /// Running protocol session tasks.
    active_sessions: HashMap<ChannelId, tokio::task::JoinHandle<()>>,
}

impl Handler for ConnectionHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let key_matches = self
            .authorized_keys
            .iter()
            .any(|authorized| authorized.key_data() == key.key_data());

        if key_matches {
            info!(
                user = user,
                peer = ?self.peer_addr,
                "SSH public key authentication accepted"
            );
            Ok(Auth::Accept)
        } else {
            warn!(
                user = user,
                peer = ?self.peer_addr,
                "SSH public key authentication rejected"
            );
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let channel_id = channel.id();
        info!(channel = ?channel_id, "SSH session channel opened");
        // Store the channel; protocol starts when exec_request arrives
        self.pending_channels.insert(channel_id, channel);
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data);
        info!(channel = ?channel_id, command = %command, "exec request");

        // Nix sends "nix-daemon --stdio" for ssh-ng:// connections
        if !command.contains("nix-daemon") || !command.contains("--stdio") {
            warn!(command = %command, "rejecting non-nix-daemon exec request");
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        session.channel_success(channel_id)?;
        session.flush()?;

        // Take the pending channel and start the protocol bridge
        let Some(channel) = self.pending_channels.remove(&channel_id) else {
            warn!(channel = ?channel_id, "exec_request for unknown channel");
            return Ok(());
        };

        let store = Arc::clone(&self.store);
        let handle = tokio::spawn(async move {
            run_channel_protocol(channel, store, channel_id).await;
        });

        self.active_sessions.insert(channel_id, handle);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel = ?channel, "SSH channel closed");
        self.pending_channels.remove(&channel);
        if let Some(handle) = self.active_sessions.remove(&channel) {
            handle.abort();
        }
        Ok(())
    }
}

/// Run the Nix worker protocol on an SSH channel.
///
/// Converts the channel into a `ChannelStream` (which implements
/// `AsyncRead + AsyncWrite`) and runs the protocol directly on it.
async fn run_channel_protocol(
    mut channel: Channel<Msg>,
    store: Arc<dyn Store>,
    channel_id: ChannelId,
) {
    // Use make_reader + make_writer to get AsyncRead + AsyncWrite on the channel.
    // make_writer() returns an owned writer, make_reader() borrows the channel.
    let writer = channel.make_writer();
    let reader = channel.make_reader();

    // Combine into a single stream for the protocol handler
    let mut stream = tokio::io::join(reader, writer);

    if let Err(e) = run_protocol(&mut stream, store.as_ref()).await {
        error!(error = %e, "protocol session error");
    }

    debug!(channel = ?channel_id, "protocol session ended");
}
