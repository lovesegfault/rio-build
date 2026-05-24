//! SSH transport and daemon-channel pool for `xtask k8s replay`.
//!
//! rio-gateway is an SSH server: clients authenticate with a public key (the
//! key's *comment* selects the tenant; the username is ignored — "rio" by
//! convention), open a session channel, and `exec "nix-daemon --stdio"` to
//! get a Nix worker-protocol byte stream. The replay engine drives many
//! concurrent daemon sessions, so this module owns the two transport layers:
//!
//! - [`GatewayPool`]: a fixed set of authenticated SSH connections. The
//!   gateway accepts at most [`CHANNELS_PER_CONNECTION`] concurrently open
//!   channels per connection, so the pool keeps a slot budget per connection
//!   and [`GatewayPool::open_channel`] waits when every slot is busy.
//! - [`DaemonChannel`]: one exec'd channel with the worker-protocol handshake
//!   already done, exposing per-operation deadline-wrapped wrappers around
//!   the rio-nix client ops the replay engine needs.
//!
//! Host-key verification is policy-driven ([`HostKeyPolicy`]): loopback
//! targets (the provider port-forward case) may accept whatever key the
//! tunnel presents, anything else must be verified against
//! `~/.ssh/known_hosts` or an explicit `--ssh-host-key` pin. The policy
//! always fails closed.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rio_nix::protocol::client::{
    ClientOpError, KeyedBuildResult, StoreEntry, client_add_multiple_to_store,
    client_add_to_store_nar, client_build_paths_with_results, client_handshake,
    client_query_path_info, client_query_valid_paths,
};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use russh::keys::ssh_key::Fingerprint;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use tokio::io::{BufWriter, ReadHalf, WriteHalf};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

/// Channels the gateway accepts per SSH connection; further opens (or execs)
/// are rejected until one closes.
///
/// Mirrors `MAX_CHANNELS_PER_CONNECTION` in
/// `rio-gateway/src/server/connection.rs` — there is no wire-level way to
/// discover the budget, so if the gateway's constant changes this one must
/// change with it.
pub const CHANNELS_PER_CONNECTION: usize = 4;

/// The exec command the gateway expects (it requires the command string to
/// contain `nix-daemon --stdio`).
const DAEMON_COMMAND: &str = "nix-daemon --stdio";

/// Deadline for per-channel setup after a pool slot is acquired: exec
/// confirmation plus the worker-protocol handshake. The gateway enforces its
/// own 30s exec→handshake deadline, so a longer client-side budget would only
/// move the same failure later.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Client→gateway keepalive ping interval. The gateway pings every 30s from
/// its side; this detects a dead gateway or collapsed tunnel from ours.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Keepalives sent without a reply before russh declares the connection dead.
const KEEPALIVE_MAX: usize = 4;

/// Default number of SSH connections for a desired concurrent-session count:
/// `ceil(max_sessions / 4)` (the gateway's per-connection channel budget),
/// never less than one connection.
pub fn default_connections(max_sessions: usize) -> usize {
    max_sessions.div_ceil(CHANNELS_PER_CONNECTION).max(1)
}

/// Total daemon-channel capacity of a pool with `connections` connections.
fn pool_capacity(connections: usize) -> usize {
    connections * CHANNELS_PER_CONNECTION
}

/// Pick the connection to try first for a new channel: among connections
/// that are `open` and have at least one free slot, the one with the most
/// free slots (ties keep the lowest index).
///
/// Closed connections are never picked — a connection the gateway has
/// dropped reads as having a full budget of free slots, which would
/// otherwise make the most-free heuristic prefer exactly the connections
/// that cannot take a channel.
fn pick_connection(open: &[bool], free_slots: &[usize]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (index, (&is_open, &free)) in open.iter().zip(free_slots).enumerate() {
        if !is_open || free == 0 {
            continue;
        }
        if best.is_none_or(|(_, best_free)| free > best_free) {
            best = Some((index, free));
        }
    }
    best.map(|(index, _)| index)
}

/// How to verify the gateway's host key.
#[derive(Debug, Clone)]
pub enum HostKeyPolicy {
    /// Accept any key, allowed ONLY for loopback endpoints (the provider
    /// port-forward case, where the tunnel endpoint is 127.0.0.1).
    AcceptLoopback,
    /// Verify against `~/.ssh/known_hosts`.
    KnownHosts,
    /// Pin to a specific public key: a path to a public-key file or a
    /// `SHA256:...` fingerprint string.
    Pinned(String),
}

/// A gateway SSH endpoint (host + port).
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Host name or IP address.
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// Errors the replay engine distinguishes when driving daemon operations.
///
/// Hand-rolled `Display`/`Error` impls (xtask does not depend on thiserror).
#[derive(Debug)]
pub enum ReplayClientError {
    /// The daemon/gateway refused the operation (or refused it in a way that
    /// raced session teardown). The request should be retried once on a fresh
    /// channel and otherwise reported as an upload/build rejection.
    Refused(String),
    /// The operation exceeded its deadline.
    Timeout(Duration),
    /// Transport or protocol failure; the channel/connection is unusable.
    Other(anyhow::Error),
}

impl std::fmt::Display for ReplayClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(msg) => write!(f, "daemon refused: {msg}"),
            Self::Timeout(deadline) => write!(f, "operation timed out after {deadline:?}"),
            Self::Other(err) => std::fmt::Display::fmt(err, f),
        }
    }
}

impl std::error::Error for ReplayClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(_) | Self::Timeout(_) => None,
            // Mirror thiserror's `#[error(transparent)]`: delegate to the
            // wrapped error's source so the chain is not duplicated.
            Self::Other(err) => {
                let inner: &(dyn std::error::Error + 'static) = err.as_ref();
                inner.source()
            }
        }
    }
}

impl From<anyhow::Error> for ReplayClientError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

/// Map a rio-nix [`ClientOpError`] to the replay engine's error taxonomy.
///
/// `Daemon` errors are clean refusals (protocol framing intact). `Wire`
/// errors normally mean the channel is unusable — except during the two
/// upload ops (`upload = true`), where the daemon may refuse mid-payload and
/// tear the session down before the client finishes writing, surfacing as an
/// I/O error; those are reported as [`ReplayClientError::Refused`] so the
/// caller treats them like a rejection rather than a transport bug.
fn map_client_op_error(err: ClientOpError, op: &str, upload: bool) -> ReplayClientError {
    match err {
        ClientOpError::Daemon(daemon_err) => ReplayClientError::Refused(daemon_err.message),
        ClientOpError::Wire(wire_err) if upload => ReplayClientError::Refused(format!(
            "transport failed during upload (may be a refusal racing session teardown): {wire_err}"
        )),
        ClientOpError::Wire(wire_err) => ReplayClientError::Other(
            anyhow::Error::new(wire_err).context(format!("{op} failed on the daemon channel")),
        ),
    }
}

/// Run one rio-nix client op under a deadline and map its outcome.
async fn run_op<T>(
    op_future: impl Future<Output = std::result::Result<T, ClientOpError>>,
    deadline: Duration,
    op: &str,
    upload: bool,
) -> std::result::Result<T, ReplayClientError> {
    match tokio::time::timeout(deadline, op_future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(map_client_op_error(err, op, upload)),
        Err(_elapsed) => Err(ReplayClientError::Timeout(deadline)),
    }
}

/// Decide whether `host` counts as loopback for [`HostKeyPolicy::AcceptLoopback`].
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Evaluate a host-key policy against the key the gateway offered.
///
/// Returns `Ok(true)` when the key is acceptable, `Ok(false)` when it is not
/// (e.g. a pinned key/fingerprint that does not match, or a host absent from
/// `known_hosts` — the SSH handler turns that into a hard, descriptive
/// error), and `Err` when the policy cannot be evaluated safely (non-loopback
/// target under [`HostKeyPolicy::AcceptLoopback`], unreadable pin file, a
/// `known_hosts` entry recording a *different* key). Mismatches are never
/// reported as `Ok(true)`: every path fails closed.
fn evaluate_host_key(
    policy: &HostKeyPolicy,
    host: &str,
    port: u16,
    offered: &PublicKey,
) -> Result<bool> {
    match policy {
        HostKeyPolicy::AcceptLoopback => {
            if is_loopback_host(host) {
                Ok(true)
            } else {
                bail!(
                    "refusing to accept an unverified host key from non-loopback target \
                     {host}:{port}; pass --ssh-host-key (public-key file or SHA256:... \
                     fingerprint) or add the host to ~/.ssh/known_hosts"
                )
            }
        }
        HostKeyPolicy::KnownHosts => {
            // russh's helper handles plain and hashed (`|1|...`) entries and
            // returns Err(KeyChanged) when the recorded key differs — that
            // propagates as an error here, never a silent accept.
            russh::keys::check_known_hosts(host, port, offered).map_err(|err| {
                anyhow::Error::new(err).context(format!(
                    "~/.ssh/known_hosts verification failed for {host}:{port} \
                     (a changed host key is never accepted)"
                ))
            })
        }
        HostKeyPolicy::Pinned(pin) => evaluate_pinned_key(pin, offered),
    }
}

/// Compare the offered key against a pin: a `SHA256:...`/`SHA512:...`
/// fingerprint string, or a path to an OpenSSH public-key file.
fn evaluate_pinned_key(pin: &str, offered: &PublicKey) -> Result<bool> {
    if pin.starts_with("SHA256:") || pin.starts_with("SHA512:") {
        let pinned: Fingerprint = pin
            .parse()
            .map_err(|err| anyhow!("invalid pinned host-key fingerprint {pin:?}: {err}"))?;
        Ok(offered.fingerprint(pinned.algorithm()) == pinned)
    } else {
        let pinned = russh::keys::load_public_key(pin)
            .map_err(|err| anyhow!("failed to load pinned host key file {pin:?}: {err}"))?;
        // Compare key material only — comments differ between the file and
        // the key offered on the wire.
        Ok(pinned.key_data() == offered.key_data())
    }
}

/// russh client handler: the only callback that matters for the replay
/// client is host-key verification.
struct ReplayHandler {
    policy: Arc<HostKeyPolicy>,
    host: String,
    port: u16,
}

impl russh::client::Handler for ReplayHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256);
        let accepted = evaluate_host_key(&self.policy, &self.host, self.port, server_public_key)
            .with_context(|| {
                format!(
                    "host key verification failed for {}:{} (offered key {fingerprint})",
                    self.host, self.port
                )
            })?;
        if !accepted {
            // Fail closed with an actionable message instead of russh's
            // generic "unknown key" error.
            let expected = match self.policy.as_ref() {
                HostKeyPolicy::AcceptLoopback => "the loopback-only policy".to_string(),
                HostKeyPolicy::KnownHosts => {
                    "any ~/.ssh/known_hosts entry for this host".to_string()
                }
                HostKeyPolicy::Pinned(pin) => format!("the pinned host key {pin}"),
            };
            bail!(
                "gateway host key {fingerprint} for {}:{} does not match {expected}",
                self.host,
                self.port
            );
        }
        tracing::debug!(host = %self.host, port = self.port, key = %fingerprint, "gateway host key accepted");
        Ok(true)
    }
}

/// One authenticated SSH connection plus its channel-slot budget.
struct PoolConnection {
    /// The authenticated SSH connection. Behind an `RwLock` so a connection
    /// the gateway has dropped can be replaced (re-dialed) in place: liveness
    /// checks and channel opens take the read side, a re-dial takes the
    /// write side.
    handle: RwLock<russh::client::Handle<ReplayHandler>>,
    /// Free channel slots on this connection (the gateway's per-connection
    /// budget). Slots are held by [`DaemonChannel`]s — including ones whose
    /// underlying connection has died; those free up as their ops fail and
    /// the channels get dropped.
    slots: Arc<Semaphore>,
    /// Position in [`GatewayPool::connections`], for logs and errors.
    index: usize,
}

/// A pool of authenticated SSH connections to rio-gateway.
///
/// Every connection carries a [`CHANNELS_PER_CONNECTION`]-permit semaphore
/// mirroring the gateway's per-connection channel cap. [`Self::open_channel`]
/// prefers the open connection with the most free slots and waits (without
/// deadlocking) when all `connections × 4` slots are in use; the slot is
/// released when the returned [`DaemonChannel`] is dropped or
/// [abandoned](DaemonChannel::abandon).
///
/// The gateway disconnects an SSH connection as soon as its last channel
/// closes, so pool connections routinely go dead between requests. The pool
/// treats that as normal: closed connections are skipped when picking a slot
/// and re-dialed lazily (one attempt per acquisition) when they are needed
/// again. In-flight operations on a connection that dies still fail — the
/// caller's retry-on-a-fresh-channel path is the recovery mechanism, not a
/// transparent transport retry.
///
/// The pool owns the SSH connections: keep it alive for as long as any
/// [`DaemonChannel`] handed out from it is in use.
pub struct GatewayPool {
    connections: Vec<PoolConnection>,
    /// Gateway endpoint, kept for lazy re-dials and error messages.
    endpoint: Endpoint,
    /// Decoded client key, kept for lazy re-dials.
    key: Arc<russh::keys::PrivateKey>,
    /// Where `key` was loaded from — error messages only.
    key_path: PathBuf,
    /// Host-key policy, applied again on every re-dial.
    policy: Arc<HostKeyPolicy>,
}

impl GatewayPool {
    /// Open `connections` authenticated SSH connections to the endpoint.
    ///
    /// The private key at `key_path` must be passphrase-less; its public half
    /// must be authorized on the gateway (the key comment selects the
    /// tenant). The host key offered by the gateway is checked against
    /// `policy` on every connection.
    pub async fn connect(
        endpoint: &Endpoint,
        connections: usize,
        key_path: &Path,
        policy: HostKeyPolicy,
    ) -> Result<Self> {
        ensure!(
            connections > 0,
            "the gateway pool needs at least one SSH connection"
        );

        let key = russh::keys::load_secret_key(key_path, None).map_err(|err| match err {
            russh::keys::Error::KeyIsEncrypted => anyhow!(
                "SSH key {} is passphrase-protected; the replay client only supports \
                 passphrase-less keys",
                key_path.display()
            ),
            other => anyhow::Error::new(other).context(format!(
                "failed to load SSH private key {}",
                key_path.display()
            )),
        })?;
        let key = Arc::new(key);
        let policy = Arc::new(policy);

        let connections = futures_util::future::try_join_all((0..connections).map(|index| {
            let key = key.clone();
            let policy = policy.clone();
            async move {
                let handle = connect_one(endpoint, &key, &policy, key_path, index).await?;
                Ok::<_, anyhow::Error>(PoolConnection {
                    handle: RwLock::new(handle),
                    slots: Arc::new(Semaphore::new(CHANNELS_PER_CONNECTION)),
                    index,
                })
            }
        }))
        .await?;

        tracing::debug!(
            connections = connections.len(),
            capacity = pool_capacity(connections.len()),
            endpoint = %endpoint,
            "gateway pool connected"
        );
        Ok(Self {
            connections,
            endpoint: endpoint.clone(),
            key,
            key_path: key_path.to_path_buf(),
            policy,
        })
    }

    /// Total channel capacity (connections × 4).
    pub fn capacity(&self) -> usize {
        pool_capacity(self.connections.len())
    }

    /// Open a daemon session: pick a connection with a free channel slot,
    /// open a channel, exec `nix-daemon --stdio`, run the worker-protocol
    /// handshake, and return the channel.
    ///
    /// Waits if all slots are momentarily busy. The wait is unbounded by
    /// design — callers are expected to do their own admission control (the
    /// replay engine bounds concurrency before asking for a channel). It
    /// cannot deadlock because every slot is released when its
    /// [`DaemonChannel`] drops. Connections the gateway has closed in the
    /// meantime (it disconnects whenever a connection's last channel closes)
    /// are skipped and re-dialed lazily.
    pub async fn open_channel(&self) -> Result<DaemonChannel> {
        let (connection, slot) = self.acquire_slot().await?;

        let setup = async {
            let channel = exec_daemon(connection, &self.endpoint).await?;
            let (mut reader, write_half) = tokio::io::split(channel.into_stream());
            // The rio-nix wire helpers issue many small writes (3 per
            // string); buffer them so each opcode goes out in a few packets.
            // The ops flush at the protocol-correct points.
            let mut writer = BufWriter::new(write_half);
            let handshake = client_handshake(&mut reader, &mut writer)
                .await
                .context("nix worker-protocol handshake with the gateway daemon failed")?;
            Ok::<_, anyhow::Error>((reader, writer, handshake))
        };
        let (reader, writer, handshake) = tokio::time::timeout(SETUP_TIMEOUT, setup)
            .await
            .map_err(|_| {
                anyhow!(
                    "daemon session setup (exec + handshake) on SSH connection {} to {} \
                     timed out after {}s",
                    connection.index,
                    self.endpoint,
                    SETUP_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| {
                format!(
                    "failed to open a daemon session on SSH connection {} to {}",
                    connection.index, self.endpoint
                )
            })?;

        tracing::debug!(
            connection = connection.index,
            negotiated_version = handshake.negotiated_version(),
            "daemon channel ready"
        );
        Ok(DaemonChannel {
            reader,
            writer,
            negotiated_version: handshake.negotiated_version(),
            connection_index: connection.index,
            _slot: slot,
        })
    }

    /// Acquire one channel slot.
    ///
    /// Preference order: an open connection with the most free slots; then
    /// closed connections (the gateway drops a connection whenever its last
    /// channel closes, so finding some is routine), each re-dialed at most
    /// once per pass; finally, when every usable connection is fully busy,
    /// wait for the first slot to free anywhere and re-validate. Errors only
    /// when no connection is open and every re-dial failed.
    async fn acquire_slot(&self) -> Result<(&PoolConnection, OwnedSemaphorePermit)> {
        let mut last_redial_error: Option<anyhow::Error> = None;
        loop {
            // Snapshot free slots and liveness. `try_read` so a re-dial in
            // progress (write-locked) counts as closed for this pass instead
            // of stalling it; the revival path below waits for it if needed.
            let free: Vec<usize> = self
                .connections
                .iter()
                .map(|connection| connection.slots.available_permits())
                .collect();
            let open: Vec<bool> = self
                .connections
                .iter()
                .map(|connection| {
                    connection
                        .handle
                        .try_read()
                        .map(|handle| !handle.is_closed())
                        .unwrap_or(false)
                })
                .collect();

            // Fast path: an open connection with a free slot, most free first.
            if let Some(index) = pick_connection(&open, &free) {
                let connection = self
                    .connections
                    .get(index)
                    .context("pick_connection returned an out-of-range index")?;
                if let Ok(permit) = connection.slots.clone().try_acquire_owned() {
                    return Ok((connection, permit));
                }
                // Lost the race for that slot; re-snapshot.
                continue;
            }

            // No open connection has a free slot. Revive closed connections
            // (most free slots first), one dial attempt each; collect
            // everything usable for the waiting path below.
            let mut usable: Vec<&PoolConnection> = self
                .connections
                .iter()
                .enumerate()
                .filter(|(index, _)| open.get(*index).copied().unwrap_or(false))
                .map(|(_, connection)| connection)
                .collect();
            let mut closed: Vec<usize> = (0..self.connections.len())
                .filter(|index| !open.get(*index).copied().unwrap_or(false))
                .collect();
            closed.sort_by_key(|index| std::cmp::Reverse(free.get(*index).copied().unwrap_or(0)));
            for index in closed {
                let Some(connection) = self.connections.get(index) else {
                    continue;
                };
                match self.ensure_open(connection).await {
                    Ok(()) => {
                        if let Ok(permit) = connection.slots.clone().try_acquire_owned() {
                            return Ok((connection, permit));
                        }
                        // Re-dialed, but its slots are still held by stale
                        // channels that have not been dropped yet.
                        usable.push(connection);
                    }
                    Err(err) => {
                        tracing::warn!(
                            connection = index,
                            error = %format!("{err:#}"),
                            "re-dial of closed SSH connection failed; skipping it for this slot"
                        );
                        last_redial_error = Some(err);
                    }
                }
            }

            if usable.is_empty() {
                let detail = last_redial_error
                    .as_ref()
                    .map(|err| format!("{err:#}"))
                    .unwrap_or_else(|| "no re-dial was attempted".to_string());
                bail!(
                    "all {} SSH connections to {} are closed (the gateway disconnects a \
                     connection when its last channel closes); re-dial failed: {detail}",
                    self.connections.len(),
                    self.endpoint
                );
            }

            // Every usable connection is fully busy: wait for the first slot
            // to free on any of them, then re-validate — the winner may have
            // been closed by the gateway while we waited. Each semaphore
            // queue is FIFO, so waiters cannot starve; losing acquire futures
            // are dropped without consuming a permit.
            let waiters: Vec<_> = usable
                .iter()
                .map(|connection| Box::pin(connection.slots.clone().acquire_owned()))
                .collect();
            let (permit, winner, _rest) = futures_util::future::select_all(waiters).await;
            let permit = permit.context("gateway pool slot semaphore closed")?;
            let connection = *usable
                .get(winner)
                .context("gateway pool slot index out of range")?;
            match self.ensure_open(connection).await {
                Ok(()) => return Ok((connection, permit)),
                Err(err) => {
                    tracing::warn!(
                        connection = connection.index,
                        error = %format!("{err:#}"),
                        "connection closed while waiting for a slot and its re-dial failed"
                    );
                    last_redial_error = Some(err);
                    // Other connections may still be usable; take another pass.
                }
            }
        }
    }

    /// Make sure `connection` has a live SSH connection, re-dialing it once
    /// if the gateway has dropped it. Concurrent callers serialize on the
    /// connection's write lock; whoever loses that race finds a fresh handle
    /// and skips its own re-dial.
    async fn ensure_open(&self, connection: &PoolConnection) -> Result<()> {
        if !connection.handle.read().await.is_closed() {
            return Ok(());
        }
        let mut handle = connection.handle.write().await;
        // Re-check under the write lock: another acquirer may have re-dialed
        // while we waited for it.
        if !handle.is_closed() {
            return Ok(());
        }
        tracing::debug!(
            connection = connection.index,
            endpoint = %self.endpoint,
            "SSH connection closed (gateway disconnects on last channel close); re-dialing"
        );
        *handle = connect_one(
            &self.endpoint,
            &self.key,
            &self.policy,
            &self.key_path,
            connection.index,
        )
        .await
        .with_context(|| {
            format!(
                "re-dial of SSH connection {} to {} failed",
                connection.index, self.endpoint
            )
        })?;
        Ok(())
    }
}

/// Dial and authenticate one SSH connection to the gateway. Used for the
/// initial pool dial and for lazily re-dialing a connection the gateway has
/// closed (it disconnects a connection whenever its last channel closes).
async fn connect_one(
    endpoint: &Endpoint,
    key: &Arc<russh::keys::PrivateKey>,
    policy: &Arc<HostKeyPolicy>,
    key_path: &Path,
    index: usize,
) -> Result<russh::client::Handle<ReplayHandler>> {
    let config = Arc::new(russh::client::Config {
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        nodelay: true,
        ..Default::default()
    });
    let handler = ReplayHandler {
        policy: policy.clone(),
        host: endpoint.host.clone(),
        port: endpoint.port,
    };
    let mut handle =
        russh::client::connect(config, (endpoint.host.as_str(), endpoint.port), handler)
            .await
            .with_context(|| format!("failed to establish SSH connection {index} to {endpoint}"))?;

    // The gateway only advertises publickey auth. For RSA keys ask which
    // rsa-sha2 variant the server supports; for anything else (ed25519 in
    // practice) the hash parameter is ignored.
    let hash_alg = if key.algorithm().is_rsa() {
        handle
            .best_supported_rsa_hash()
            .await
            .context("failed to query the gateway's supported RSA signature hashes")?
            .flatten()
    } else {
        None
    };
    let auth = handle
        .authenticate_publickey("rio", PrivateKeyWithHashAlg::new(key.clone(), hash_alg))
        .await
        .with_context(|| {
            format!("publickey authentication failed on connection {index} to {endpoint}")
        })?;
    ensure!(
        auth.success(),
        "the gateway rejected the SSH key {} — its public half must be in the gateway's \
         authorized keys with the comment naming the tenant; grant it with \
         `cargo xtask k8s grant <pubkey> --tenant <name>`, or use the key `deploy` installed \
         (the private half of RIO_SSH_PUBKEY)",
        key_path.display()
    );
    tracing::debug!(index, endpoint = %endpoint, "gateway SSH connection authenticated");
    Ok(handle)
}

/// Open one session channel on `connection`, exec the daemon command, and
/// wait for the gateway to confirm it. `exec` only sends the request; the
/// accept/reject verdict arrives as a channel message (the gateway re-checks
/// its 4-channel budget at exec time).
async fn exec_daemon(
    connection: &PoolConnection,
    endpoint: &Endpoint,
) -> Result<russh::Channel<russh::client::Msg>> {
    let mut channel = {
        let handle = connection.handle.read().await;
        if handle.is_closed() {
            bail!(
                "SSH connection {} to {endpoint} is closed (gateway disconnect or keepalive \
                 timeout)",
                connection.index
            );
        }
        handle
            .channel_open_session()
            .await
            .context("failed to open an SSH session channel")?
    };
    channel
        .exec(true, DAEMON_COMMAND)
        .await
        .context("failed to send the nix-daemon exec request")?;
    // Bailing on any arm below (or later, before the handshake completes)
    // does not leak the server-side slot: our dropped channel sends an SSH
    // close, and the gateway's own 30s handshake reaper tears down sessions
    // that never complete the worker-protocol handshake.
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Success) => return Ok(channel),
            Some(russh::ChannelMsg::Failure) => bail!(
                "gateway refused `{DAEMON_COMMAND}` on a new channel \
                 (per-connection channel budget exhausted?)"
            ),
            // Flow-control noise can arrive ahead of the exec verdict.
            Some(russh::ChannelMsg::WindowAdjusted { .. }) => continue,
            Some(other) => bail!(
                "unexpected SSH channel message while waiting for the exec verdict: {other:?}"
            ),
            None => bail!("SSH channel closed before the gateway confirmed the exec request"),
        }
    }
}

/// The bidirectional byte stream of one exec'd gateway channel.
type GatewayStream = russh::ChannelStream<russh::client::Msg>;

/// One `nix-daemon --stdio` session over an SSH channel, handshake done.
///
/// Methods take `&mut self`: the gateway runs the daemon protocol
/// sequentially per channel, so a channel can only ever run one operation at
/// a time. Run independent operations on separate channels from
/// [`GatewayPool::open_channel`].
///
/// After any error ([`ReplayClientError`]) the channel should be dropped (or
/// [abandoned](Self::abandon)) and a fresh one opened: a refused upload or a
/// timed-out read leaves the wire position unknown and it cannot be
/// resynchronized.
///
/// Dropping the channel (with or without `abandon`) releases its pool slot
/// and makes russh send a best-effort SSH channel close; the gateway treats
/// channel teardown as the client going away and cancels that session's
/// in-flight builds.
pub struct DaemonChannel {
    reader: ReadHalf<GatewayStream>,
    writer: BufWriter<WriteHalf<GatewayStream>>,
    negotiated_version: u64,
    /// Index of the pool connection this channel runs on (for triage logs).
    connection_index: usize,
    /// Slot on the owning connection; released on drop.
    _slot: OwnedSemaphorePermit,
}

impl DaemonChannel {
    /// Worker-protocol version negotiated during the handshake.
    pub fn negotiated_version(&self) -> u64 {
        self.negotiated_version
    }

    /// Index (within the pool) of the SSH connection this channel runs on —
    /// for correlating channel failures with a specific connection in logs.
    pub fn connection_index(&self) -> usize {
        self.connection_index
    }

    /// `wopQueryValidPaths`: which of `paths` does the target already have?
    /// (`substitute = false`, mirroring `nix copy`.)
    pub async fn query_valid_paths(
        &mut self,
        paths: &[String],
        timeout: Duration,
    ) -> std::result::Result<BTreeSet<String>, ReplayClientError> {
        let op = format!("QueryValidPaths ({} paths)", paths.len());
        run_op(
            client_query_valid_paths(&mut self.reader, &mut self.writer, paths, false),
            timeout,
            &op,
            false,
        )
        .await
    }

    /// `wopQueryPathInfo`: one path's [`ValidPathInfo`], `None` if absent.
    pub async fn query_path_info(
        &mut self,
        path: &str,
        timeout: Duration,
    ) -> std::result::Result<Option<ValidPathInfo>, ReplayClientError> {
        let op = format!("QueryPathInfo {path}");
        run_op(
            client_query_path_info(&mut self.reader, &mut self.writer, path),
            timeout,
            &op,
            false,
        )
        .await
    }

    /// `wopAddMultipleToStore`: upload a batch of store paths in one framed
    /// stream (`repair = false`, `dont_check_sigs = true`).
    pub async fn add_multiple_to_store(
        &mut self,
        entries: Vec<StoreEntry>,
        timeout: Duration,
    ) -> std::result::Result<(), ReplayClientError> {
        let op = format!("AddMultipleToStore ({} entries)", entries.len());
        run_op(
            client_add_multiple_to_store(&mut self.reader, &mut self.writer, false, true, entries),
            timeout,
            &op,
            true,
        )
        .await
    }

    /// `wopAddToStoreNar`: upload one store path with its NAR serialization
    /// (`repair = false`, `dont_check_sigs = true`).
    pub async fn add_to_store_nar(
        &mut self,
        entry: StoreEntry,
        timeout: Duration,
    ) -> std::result::Result<(), ReplayClientError> {
        let op = format!("AddToStoreNar {}", entry.store_path);
        run_op(
            client_add_to_store_nar(&mut self.reader, &mut self.writer, entry, false, true),
            timeout,
            &op,
            true,
        )
        .await
    }

    /// `wopBuildPathsWithResults`: build the given derived paths and collect
    /// the daemon's per-path results (submission order).
    pub async fn build_paths_with_results(
        &mut self,
        derived: &[String],
        timeout: Duration,
    ) -> std::result::Result<Vec<KeyedBuildResult>, ReplayClientError> {
        let op = format!("BuildPathsWithResults ({} paths)", derived.len());
        let negotiated_version = self.negotiated_version;
        run_op(
            client_build_paths_with_results(
                &mut self.reader,
                &mut self.writer,
                derived,
                negotiated_version,
            ),
            timeout,
            &op,
            false,
        )
        .await
    }

    /// Drop the channel abruptly without a daemon-level goodbye — used for
    /// replaying recorded client disconnects. The gateway cancels the
    /// session's builds. Releases the pool slot.
    ///
    /// This is the same as dropping the channel: the worker protocol has no
    /// goodbye message, and on drop russh sends a best-effort SSH channel
    /// close, which the gateway treats as the client going away — exactly the
    /// teardown a recorded disconnect should reproduce.
    pub fn abandon(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::protocol::stderr::StderrError;
    use rio_nix::protocol::wire::WireError;
    use russh::keys::{Algorithm, PrivateKey};

    fn ed25519_key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("generate test key")
    }

    /// Pin string for a path-based [`HostKeyPolicy::Pinned`] (temp paths are
    /// valid UTF-8 in these tests).
    fn pin_path(path: &Path) -> String {
        path.to_str().expect("temp path is valid UTF-8").to_string()
    }

    #[test]
    fn host_key_policy_loopback_only_accepts_loopback() {
        let key = ed25519_key();
        let offered = key.public_key();

        for host in ["127.0.0.1", "::1", "localhost", "127.0.0.5", "[::1]"] {
            let accepted = evaluate_host_key(&HostKeyPolicy::AcceptLoopback, host, 2222, offered)
                .unwrap_or_else(|err| panic!("loopback host {host} must be accepted: {err:#}"));
            assert!(accepted, "loopback host {host} must be accepted");
        }

        for host in ["10.0.0.5", "gateway.example.com"] {
            let err = evaluate_host_key(&HostKeyPolicy::AcceptLoopback, host, 22, offered)
                .expect_err("non-loopback hosts must be rejected with an explanation");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("--ssh-host-key"),
                "rejection for {host} must point at --ssh-host-key: {msg}"
            );
        }
    }

    #[test]
    fn host_key_policy_pinned_fingerprint_and_file() -> Result<()> {
        let offered_key = ed25519_key();
        let offered = offered_key.public_key();
        let other_key = ed25519_key();

        // Matching SHA256 fingerprint pin.
        let matching = offered.fingerprint(HashAlg::Sha256).to_string();
        assert!(
            matching.starts_with("SHA256:"),
            "fingerprint format: {matching}"
        );
        assert!(evaluate_host_key(
            &HostKeyPolicy::Pinned(matching),
            "10.0.0.5",
            22,
            offered
        )?);

        // A mismatching fingerprint is Ok(false) — the SSH handler turns that
        // into a hard, descriptive error; it must never be Ok(true).
        let mismatching = other_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        assert!(!evaluate_host_key(
            &HostKeyPolicy::Pinned(mismatching),
            "10.0.0.5",
            22,
            offered
        )?);

        // Pin by public-key file path.
        let dir = tempfile::tempdir()?;
        let matching_path = dir.path().join("gateway_host_key.pub");
        std::fs::write(&matching_path, offered.to_openssh()?)?;
        assert!(evaluate_host_key(
            &HostKeyPolicy::Pinned(pin_path(&matching_path)),
            "10.0.0.5",
            22,
            offered
        )?);

        // A file with a different key must not match.
        let other_path = dir.path().join("other_host_key.pub");
        std::fs::write(&other_path, other_key.public_key().to_openssh()?)?;
        assert!(!evaluate_host_key(
            &HostKeyPolicy::Pinned(pin_path(&other_path)),
            "10.0.0.5",
            22,
            offered
        )?);

        // A missing pin file fails closed with an error.
        assert!(
            evaluate_host_key(
                &HostKeyPolicy::Pinned(pin_path(&dir.path().join("missing.pub"))),
                "10.0.0.5",
                22,
                offered
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn pool_sizing_capacity() {
        // Default connection count: ceil(max_sessions / 4), at least one.
        assert_eq!(default_connections(32), 8);
        assert_eq!(default_connections(1), 1);
        assert_eq!(default_connections(5), 2);
        assert_eq!(default_connections(0), 1);
        // Capacity: connections × 4.
        assert_eq!(pool_capacity(8), 32);
        assert_eq!(pool_capacity(1), 4);
    }

    #[test]
    fn pick_connection_skips_closed_and_prefers_most_free() {
        // A closed connection is never picked, even though the gateway
        // dropping it left it with the most free slots.
        assert_eq!(pick_connection(&[false, true], &[4, 2]), Some(1));
        // Among open connections the most free slots win; ties keep the
        // lowest index.
        assert_eq!(pick_connection(&[true, true, true], &[1, 3, 3]), Some(1));
        // Open but fully busy connections are not picked.
        assert_eq!(pick_connection(&[true, true], &[0, 0]), None);
        // All closed (or an empty pool) yields None — the caller then tries
        // to re-dial closed connections instead.
        assert_eq!(pick_connection(&[false, false], &[4, 4]), None);
        assert_eq!(pick_connection(&[], &[]), None);
    }

    #[test]
    fn replay_client_error_mapping() {
        // Daemon refusal → Refused carrying the daemon's message.
        let daemon = ClientOpError::Daemon(StderrError::simple("rio-gateway", "path not allowed"));
        match map_client_op_error(daemon, "AddToStoreNar /nix/store/x", true) {
            ReplayClientError::Refused(msg) => {
                assert!(msg.contains("path not allowed"), "message: {msg}");
            }
            other => panic!("daemon error must map to Refused, got {other:?}"),
        }

        // Wire error on a non-upload op → Other, context names the op.
        let wire = ClientOpError::Wire(WireError::Io(std::io::Error::other("broken pipe")));
        match map_client_op_error(wire, "QueryPathInfo /nix/store/x", false) {
            ReplayClientError::Other(err) => {
                let chain = format!("{err:#}");
                assert!(
                    chain.contains("QueryPathInfo"),
                    "context must name the op: {chain}"
                );
                assert!(
                    chain.contains("broken pipe"),
                    "chain keeps the cause: {chain}"
                );
            }
            other => panic!("wire error on a query must map to Other, got {other:?}"),
        }

        // Wire error during an upload → Refused (refusal racing teardown).
        let wire = ClientOpError::Wire(WireError::Io(std::io::Error::other("connection reset")));
        match map_client_op_error(wire, "AddMultipleToStore (3 entries)", true) {
            ReplayClientError::Refused(msg) => {
                assert!(msg.contains("racing session teardown"), "message: {msg}");
                assert!(
                    msg.contains("connection reset"),
                    "message keeps the cause: {msg}"
                );
            }
            other => panic!("wire error during upload must map to Refused, got {other:?}"),
        }
    }
}
