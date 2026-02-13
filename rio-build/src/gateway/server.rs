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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        // Try to save for reuse
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write the key in OpenSSH format
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
            channels: HashMap::new(),
        }
    }
}

/// Per-connection handler that manages SSH channels.
pub struct ConnectionHandler {
    peer_addr: Option<SocketAddr>,
    store: Arc<dyn Store>,
    authorized_keys: Arc<Vec<PublicKey>>,
    /// Active protocol sessions, indexed by channel ID.
    channels: HashMap<ChannelId, tokio::task::JoinHandle<()>>,
}

impl Handler for ConnectionHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let key_matches = self.authorized_keys.iter().any(|authorized| {
            // Compare the key data (algorithm + public key bytes)
            authorized.key_data() == key.key_data()
        });

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

        let store = Arc::clone(&self.store);

        // Spawn a task bridging the SSH channel to the Nix protocol handler.
        //
        // Architecture: two duplex pipes connect the SSH channel to the protocol:
        //   SSH channel (client data) → pipe_writer → [pipe_reader] → protocol reader
        //   protocol writer → [resp_writer] → resp_reader → SSH channel (server data)
        let handle = tokio::spawn(async move {
            let mut ch = channel;

            // Pipe for client→server data
            let (pipe_reader, mut pipe_writer) = tokio::io::duplex(256 * 1024);
            // Pipe for server→client data
            let (resp_reader, resp_writer) = tokio::io::duplex(256 * 1024);

            // Protocol handler task: reads from pipe_reader, writes to resp_writer
            let proto_handle = tokio::spawn(async move {
                let (mut pr, mut rw) = (pipe_reader, resp_writer);
                if let Err(e) = run_protocol(&mut pr, &mut rw, store.as_ref()).await {
                    error!(error = %e, "protocol session error");
                }
            });

            // Bridge task: shuttle bytes between SSH channel and the pipes
            let mut resp_reader = resp_reader;
            loop {
                tokio::select! {
                    // Data from SSH channel → protocol handler
                    msg = ch.wait() => {
                        match msg {
                            Some(russh::ChannelMsg::Data { data }) => {
                                if pipe_writer.write_all(&data).await.is_err() {
                                    break;
                                }
                                if pipe_writer.flush().await.is_err() {
                                    break;
                                }
                            }
                            Some(russh::ChannelMsg::Eof) | None => {
                                debug!("SSH channel EOF or closed");
                                drop(pipe_writer);
                                break;
                            }
                            _ => {}
                        }
                    }

                    // Data from protocol handler → SSH channel
                    n = async {
                        let mut buf = vec![0u8; 64 * 1024];
                        resp_reader.read(&mut buf).await.map(|n| (n, buf))
                    } => {
                        match n {
                            Ok((0, _)) => break,
                            Ok((n, buf)) => {
                                if ch.data(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "error reading protocol response");
                                break;
                            }
                        }
                    }
                }
            }

            let _ = proto_handle.await;
            debug!(channel = ?channel_id, "protocol session ended");
        });

        self.channels.insert(channel_id, handle);
        Ok(true)
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel = ?channel, "SSH channel closed");
        if let Some(handle) = self.channels.remove(&channel) {
            handle.abort();
        }
        Ok(())
    }
}
