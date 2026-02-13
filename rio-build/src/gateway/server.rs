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
use russh::{ChannelId, CryptoVec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{Instrument, debug, error, info, warn};

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
        metrics::counter!("rio_gateway_connections_total", "result" => "new").increment(1);
        metrics::gauge!("rio_gateway_connections_active").increment(1.0);
        info!(peer = ?peer_addr, "new SSH connection");
        ConnectionHandler {
            peer_addr,
            store: Arc::clone(&self.store),
            authorized_keys: Arc::clone(&self.authorized_keys),
            sessions: HashMap::new(),
        }
    }
}

/// State for an active protocol session on one SSH channel.
///
/// Data flow:
///   client SSH data → Handler::data() → `client_tx` → protocol reader
///   protocol writer → `response_rx` → response pump task → Handle::data() → client
struct ChannelSession {
    /// Send client data to the protocol handler.
    client_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Protocol handler task.
    _proto_task: tokio::task::JoinHandle<()>,
    /// Response pump task (reads protocol output, sends via Handle::data).
    _response_task: tokio::task::JoinHandle<()>,
}

/// Per-connection handler that manages SSH channels.
///
/// Data flow for each channel:
/// 1. `channel_open_session` — accept channel, store ID
/// 2. `exec_request("nix-daemon --stdio")` — set up pipes + protocol handler
/// 3. `data` callback — forward incoming bytes to the protocol handler
/// 4. Response pump task — forward protocol output to client via Handle::data()
pub struct ConnectionHandler {
    peer_addr: Option<SocketAddr>,
    store: Arc<dyn Store>,
    authorized_keys: Arc<Vec<PublicKey>>,
    /// Active protocol sessions, indexed by channel ID.
    sessions: HashMap<ChannelId, ChannelSession>,
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        metrics::gauge!("rio_gateway_connections_active").decrement(1.0);
        // Also clean up any remaining channel gauges
        let remaining_channels = self.sessions.len();
        if remaining_channels > 0 {
            metrics::gauge!("rio_gateway_channels_active").decrement(remaining_channels as f64);
        }
        debug!(
            peer = ?self.peer_addr,
            remaining_channels = remaining_channels,
            "SSH connection handler dropped"
        );
    }
}

impl Handler for ConnectionHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let key_matches = self
            .authorized_keys
            .iter()
            .any(|authorized| authorized.key_data() == key.key_data());

        if key_matches {
            metrics::counter!("rio_gateway_connections_total", "result" => "accepted").increment(1);
            info!(
                user = user,
                peer = ?self.peer_addr,
                "SSH public key authentication accepted"
            );
            Ok(Auth::Accept)
        } else {
            metrics::counter!("rio_gateway_connections_total", "result" => "rejected").increment(1);
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
        _channel: russh::Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let channel_id = _channel.id();
        info!(channel = ?channel_id, "SSH session channel opened");
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
        metrics::gauge!("rio_gateway_channels_active").increment(1.0);

        // Data pipeline using two independent DuplexStreams:
        //   client SSH data → mpsc → pump → inbound_writer ↔ inbound_reader → protocol reads
        //   protocol writes → outbound_writer ↔ outbound_reader → pump → Handle::data()

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Inbound pipe: SSH client data → protocol handler
        let (inbound_reader, mut inbound_writer) = tokio::io::duplex(256 * 1024);
        // Outbound pipe: protocol handler → SSH client
        let (mut outbound_reader, outbound_writer) = tokio::io::duplex(256 * 1024);

        // Task: forward SSH client data → inbound pipe
        let client_pump = tokio::spawn(async move {
            while let Some(data) = client_rx.recv().await {
                if inbound_writer.write_all(&data).await.is_err() {
                    break;
                }
            }
            let _ = inbound_writer.shutdown().await;
        });

        // Task: run the protocol handler with separate reader/writer
        let store = Arc::clone(&self.store);
        let proto_task = tokio::spawn(
            async move {
                let mut reader = inbound_reader;
                let mut writer = outbound_writer;
                if let Err(e) = run_protocol(&mut reader, &mut writer, store.as_ref()).await {
                    error!(error = %e, "protocol session error");
                }
                debug!("protocol handler finished");
            }
            .instrument(tracing::info_span!("channel", channel = ?channel_id)),
        );

        // Task: pump protocol responses → SSH client via Handle::data()
        let handle = session.handle();
        let response_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match outbound_reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = CryptoVec::from_slice(&buf[..n]);
                        if handle.data(channel_id, data).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "error reading protocol response");
                        break;
                    }
                }
            }
            let _ = handle.eof(channel_id).await;
            let _ = client_pump.await;
        });

        self.sessions.insert(
            channel_id,
            ChannelSession {
                client_tx,
                _proto_task: proto_task,
                _response_task: response_task,
            },
        );

        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(session) = self.sessions.get(&channel) {
            debug!(channel = ?channel, len = data.len(), "forwarding client data to protocol");
            if session.client_tx.send(data.to_vec()).await.is_err() {
                debug!(channel = ?channel, "protocol session gone, dropping data");
            }
        } else {
            debug!(channel = ?channel, len = data.len(), "data for channel with no session");
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel = ?channel, "SSH channel EOF");
        if self.sessions.remove(&channel).is_some() {
            metrics::gauge!("rio_gateway_channels_active").decrement(1.0);
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel = ?channel, "SSH channel closed");
        if self.sessions.remove(&channel).is_some() {
            metrics::gauge!("rio_gateway_channels_active").decrement(1.0);
        }
        Ok(())
    }
}
