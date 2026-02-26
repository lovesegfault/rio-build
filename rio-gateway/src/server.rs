//! SSH server using `russh` that terminates connections and speaks the
//! Nix worker protocol on each session channel, delegating operations
//! to gRPC store and scheduler services.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::store::store_service_client::StoreServiceClient;
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Handler, Msg, Server as _, Session};
use russh::{ChannelId, CryptoVec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tonic::transport::Channel;
use tracing::{Instrument, debug, error, info, warn};

use crate::session::run_protocol;

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

/// The SSH server that accepts connections and spawns protocol sessions.
pub struct GatewayServer {
    store_client: StoreServiceClient<Channel>,
    scheduler_client: SchedulerServiceClient<Channel>,
    authorized_keys: Arc<Vec<PublicKey>>,
}

impl GatewayServer {
    pub fn new(
        store_client: StoreServiceClient<Channel>,
        scheduler_client: SchedulerServiceClient<Channel>,
        authorized_keys: Vec<PublicKey>,
    ) -> Self {
        if authorized_keys.is_empty() {
            warn!("no authorized keys configured; all SSH connections will be rejected");
        }
        GatewayServer {
            store_client,
            scheduler_client,
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
            store_client: self.store_client.clone(),
            scheduler_client: self.scheduler_client.clone(),
            authorized_keys: Arc::clone(&self.authorized_keys),
            sessions: HashMap::new(),
        }
    }
}

/// State for an active protocol session on one SSH channel.
struct ChannelSession {
    /// Send client data to the protocol handler.
    client_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Protocol handler task.
    proto_task: tokio::task::JoinHandle<()>,
    /// Response pump task.
    response_task: tokio::task::JoinHandle<()>,
}

impl Drop for ChannelSession {
    fn drop(&mut self) {
        self.proto_task.abort();
        self.response_task.abort();
        // Gauge decrement lives here so it fires on ALL drop paths: normal
        // channel_close, connection drop (HashMap clears), and session removal
        // after a dead protocol task. Avoids gauge leak on abnormal paths.
        metrics::gauge!("rio_gateway_channels_active").decrement(1.0);
    }
}

/// Per-connection handler that manages SSH channels.
pub struct ConnectionHandler {
    peer_addr: Option<SocketAddr>,
    store_client: StoreServiceClient<Channel>,
    scheduler_client: SchedulerServiceClient<Channel>,
    authorized_keys: Arc<Vec<PublicKey>>,
    /// Active protocol sessions, indexed by channel ID.
    sessions: HashMap<ChannelId, ChannelSession>,
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        metrics::gauge!("rio_gateway_connections_active").decrement(1.0);
        // Channel gauge decrement is handled by ChannelSession::Drop when
        // the sessions HashMap is cleared.
        debug!(
            peer = ?self.peer_addr,
            remaining_channels = self.sessions.len(),
            "SSH connection handler dropped"
        );
    }
}

impl Handler for ConnectionHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        warn!(peer = ?self.peer_addr, "rejecting password authentication");
        Ok(Auth::reject())
    }

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
        channel: russh::Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let channel_id = channel.id();
        info!(channel = ?channel_id, "SSH session channel opened");
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = match String::from_utf8(data.to_vec()) {
            Ok(s) => s,
            Err(_) => {
                warn!(channel = ?channel_id, "rejecting exec request: command is not valid UTF-8");
                session.channel_failure(channel_id)?;
                return Ok(());
            }
        };
        info!(channel = ?channel_id, command = %command, "exec request");

        let args: Vec<&str> = command.split_whitespace().collect();
        let is_nix_daemon = args.len() >= 2
            && args[args.len() - 2].ends_with("nix-daemon")
            && args[args.len() - 1] == "--stdio";
        if !is_nix_daemon {
            warn!(command = %command, "rejecting non-nix-daemon exec request");
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        session.channel_success(channel_id)?;
        metrics::gauge!("rio_gateway_channels_active").increment(1.0);

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let (inbound_reader, mut inbound_writer) = tokio::io::duplex(256 * 1024);
        let (mut outbound_reader, outbound_writer) = tokio::io::duplex(256 * 1024);

        // Task: forward SSH client data -> inbound pipe
        let client_pump = rio_common::task::spawn_monitored("client-pump", async move {
            while let Some(data) = client_rx.recv().await {
                if let Err(e) = inbound_writer.write_all(&data).await {
                    debug!(error = %e, "client pump: inbound write failed");
                    break;
                }
            }
            drop(inbound_writer);
        });

        // Task: run the protocol handler with gRPC clients
        let mut store_client = self.store_client.clone();
        let mut scheduler_client = self.scheduler_client.clone();
        let proto_task = rio_common::task::spawn_monitored(
            "proto-task",
            async move {
                let mut reader = inbound_reader;
                let mut writer = outbound_writer;
                if let Err(e) = run_protocol(
                    &mut reader,
                    &mut writer,
                    &mut store_client,
                    &mut scheduler_client,
                )
                .await
                {
                    error!(error = %e, "protocol session error");
                }
                debug!("protocol handler finished");
            }
            .instrument(tracing::info_span!("channel", channel = ?channel_id)),
        );

        // Task: pump protocol responses -> SSH client
        let handle = session.handle();
        let response_task = rio_common::task::spawn_monitored("response-task", async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match outbound_reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = CryptoVec::from_slice(&buf[..n]);
                        if handle.data(channel_id, data).await.is_err() {
                            warn!(channel = ?channel_id, "response pump: SSH send failed");
                            metrics::counter!("rio_gateway_errors_total", "type" => "ssh_send")
                                .increment(1);
                            break;
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "error reading protocol response");
                        break;
                    }
                }
            }
            if let Err(e) = handle.eof(channel_id).await {
                warn!(channel = ?channel_id, error = ?e, "failed to send EOF to SSH client");
            }
            if let Err(e) = handle.close(channel_id).await {
                warn!(channel = ?channel_id, error = ?e, "failed to close SSH channel");
            }
            if let Err(e) = client_pump.await
                && e.is_panic()
            {
                error!(channel = ?channel_id, "client pump task panicked: {e}");
            }
        });

        self.sessions.insert(
            channel_id,
            ChannelSession {
                client_tx: Some(client_tx),
                proto_task,
                response_task,
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
            if let Some(tx) = &session.client_tx {
                debug!(channel = ?channel, len = data.len(), "forwarding client data to protocol");
                if tx.send(data.to_vec()).await.is_err() {
                    warn!(channel = ?channel, "protocol session dead, closing channel");
                    // Gauge decrement handled by ChannelSession::Drop.
                    self.sessions.remove(&channel);
                    return Ok(());
                }
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
        if let Some(session) = self.sessions.get_mut(&channel) {
            session.client_tx.take();
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel = ?channel, "SSH channel closed");
        // Gauge decrement handled by ChannelSession::Drop.
        self.sessions.remove(&channel);
        Ok(())
    }
}
