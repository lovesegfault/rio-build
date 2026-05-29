//! In-process SSH transport for the gateway's nix-daemon worker protocol.
//!
//! rio-gateway is an SSH server: clients authenticate with a public key (the
//! key's *comment* selects the tenant; the username is ignored — "rio" by
//! convention), open a session channel, and `exec "nix-daemon --stdio"` to
//! get a Nix worker-protocol byte stream. The campaign engine holds one
//! channel per in-flight submission, so this module owns the two transport
//! layers:
//!
//! - [`GatewayPool`]: a fixed budget of authenticated SSH connections, dialed
//!   lazily on first use. The gateway accepts at most
//!   [`CHANNELS_PER_CONNECTION`] concurrently open channels per connection
//!   and drops a connection as soon as its last channel closes, so the pool
//!   keeps a slot budget per connection, skips closed connections, and
//!   re-dials them on demand.
//! - [`DaemonChannel`]: one exec'd channel with the worker-protocol handshake
//!   already done, exposing per-operation deadline-wrapped wrappers around
//!   the rio-nix client ops the engine needs.
//!
//! Host-key verification is pin-only ([`HostKeyPolicy::Pinned`]): the
//! expected gateway host key comes from the campaign spec
//! (`cluster.gateway_host_key`) and every dial verifies the offered key
//! against it. There is no trust-on-first-use mode — a missing pin fails
//! pool construction and a mismatched key fails the dial, both with errors
//! naming what was expected.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rio_nix::protocol::client::{
    ClientOpError, KeyedBuildResult, StoreEntry, client_add_multiple_to_store,
    client_add_to_store_nar, client_build_paths_with_results,
    client_build_paths_with_results_observed, client_handshake, client_query_path_info,
    client_query_valid_paths,
};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use russh::keys::ssh_key::Fingerprint;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use tokio::io::{BufWriter, ReadHalf, WriteHalf};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, RwLock, Semaphore};

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

/// Deadline for dialing one SSH connection: TCP connect, key exchange, and
/// publickey auth against the in-cluster gateway Service. Bounded so a
/// blackholed or half-dead endpoint fails the dial instead of wedging
/// `open_channel` (and the per-connection lock the dial holds) indefinitely;
/// 30s is far above a healthy in-cluster dial and matches the gateway's own
/// setup budget.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Client→gateway keepalive ping interval. The gateway pings every 30s from
/// its side; this detects a dead gateway from ours.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Keepalives sent without a reply before russh declares the connection dead.
const KEEPALIVE_MAX: usize = 4;

/// SSH port assumed when the store URL does not name one.
const DEFAULT_SSH_PORT: u16 = 22;

/// SSH username assumed when the store URL does not name one. The gateway
/// ignores the username (the key comment selects the tenant); "rio" is the
/// documented convention.
const DEFAULT_SSH_USER: &str = "rio";

/// Default number of SSH connections for a desired concurrent-channel count:
/// `ceil(max_in_flight / 4)` (the gateway's per-connection channel budget),
/// never less than one connection.
pub fn default_connections(max_in_flight: usize) -> usize {
    max_in_flight.div_ceil(CHANNELS_PER_CONNECTION).max(1)
}

/// Total daemon-channel capacity of a pool with `connections` connections.
fn pool_capacity(connections: usize) -> usize {
    connections * CHANNELS_PER_CONNECTION
}

/// Pick the connection to try first for a new channel: among connections
/// that are `open` and have at least one free slot, the one with the most
/// free slots (ties keep the lowest index).
///
/// Closed (or not-yet-dialed) connections are never picked — a connection
/// the gateway has dropped reads as having a full budget of free slots,
/// which would otherwise make the most-free heuristic prefer exactly the
/// connections that cannot take a channel.
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

/// How to verify the gateway's host key. Pin-only: every dial compares the
/// offered key against the configured pin and fails closed on mismatch.
#[derive(Debug, Clone)]
pub enum HostKeyPolicy {
    /// Pin to a specific host key: a `SHA256:...`/`SHA512:...` fingerprint,
    /// an inline OpenSSH public-key line (`ssh-ed25519 AAAA… comment`), or a
    /// path to a public-key file. Key material is compared; comments are
    /// ignored.
    Pinned(String),
}

/// Gateway endpoint parsed from the campaign spec's ssh-ng store URL.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayEndpoint {
    /// Host name or IP address of the gateway's SSH listener.
    pub host: String,
    /// TCP port (default 22).
    pub port: u16,
    /// SSH username (default "rio"; the gateway ignores it — the key comment
    /// selects the tenant).
    pub user: String,
    /// Path to the tenant's SSH private key, from the URL's `ssh-key=` query
    /// parameter.
    pub ssh_key_path: PathBuf,
}

impl GatewayEndpoint {
    /// Parse an `ssh-ng://[user@]host[:port]?…&ssh-key=<path>&…` store URL
    /// (the shape `cluster.gateway_store_url` carries).
    ///
    /// Non-`ssh-ng` schemes are rejected; the `ssh-key` query parameter is
    /// required; every other query parameter (e.g. `compress=`) tunes nix's
    /// own ssh transport and is ignored here.
    pub fn parse(store_url: &str) -> Result<Self> {
        let rest = store_url.strip_prefix("ssh-ng://").ok_or_else(|| {
            anyhow!("gateway store URL {store_url:?} must use the ssh-ng:// scheme")
        })?;
        let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
        // Our store URLs carry no path component; tolerate and drop one.
        let authority = authority.split('/').next().unwrap_or(authority);
        let (user, host_port) = match authority.split_once('@') {
            Some((user, host_port)) if !user.is_empty() => (user.to_string(), host_port),
            Some((_, host_port)) => (DEFAULT_SSH_USER.to_string(), host_port),
            None => (DEFAULT_SSH_USER.to_string(), authority),
        };
        let (host, port) = split_host_port(host_port)
            .with_context(|| format!("gateway store URL {store_url:?} has an invalid host/port"))?;
        ensure!(
            !host.is_empty(),
            "gateway store URL {store_url:?} is missing a host"
        );
        let ssh_key_path = query
            .split('&')
            .find_map(|param| param.strip_prefix("ssh-key="))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "gateway store URL {store_url:?} is missing the ssh-key= query parameter \
                     (path to the tenant's SSH private key)"
                )
            })?;
        Ok(Self {
            host,
            port,
            user,
            ssh_key_path,
        })
    }
}

impl std::fmt::Display for GatewayEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// Split `host[:port]` (or `[v6addr][:port]`) into host and port, defaulting
/// the port to [`DEFAULT_SSH_PORT`].
fn split_host_port(host_port: &str) -> Result<(String, u16)> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("unterminated '[' in host {host_port:?}"))?;
        let port = if let Some(port) = after.strip_prefix(':') {
            port.parse::<u16>()
                .with_context(|| format!("invalid port {port:?} in {host_port:?}"))?
        } else if after.is_empty() {
            DEFAULT_SSH_PORT
        } else {
            bail!("unexpected characters after the bracketed host in {host_port:?}")
        };
        return Ok((host.to_string(), port));
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .with_context(|| format!("invalid port {port:?} in {host_port:?}"))?;
            Ok((host.to_string(), port))
        }
        None => Ok((host_port.to_string(), DEFAULT_SSH_PORT)),
    }
}

/// Errors the campaign engine distinguishes when driving daemon operations.
///
/// Hand-rolled `Display`/`Error` impls (the variants carry everything the
/// submitter needs without another derive dependency).
#[derive(Debug)]
pub enum TransportError {
    /// The daemon/gateway refused the operation (or refused it in a way that
    /// raced session teardown). The request should be retried once on a fresh
    /// channel and otherwise reported as an upload/build rejection.
    Refused(String),
    /// The operation exceeded its deadline. The wire position is unknown
    /// afterwards — the channel must not be reused.
    Timeout {
        /// Operation that timed out (e.g. `QueryValidPaths (2000 paths)`).
        op: String,
        /// Pool index of the SSH connection the channel ran on.
        connection: usize,
        /// The per-op deadline that elapsed.
        deadline: Duration,
    },
    /// Transport or protocol failure; the channel/connection is unusable.
    Other(anyhow::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(msg) => write!(f, "daemon refused: {msg}"),
            Self::Timeout {
                op,
                connection,
                deadline,
            } => write!(
                f,
                "{op} on gateway connection {connection} timed out after {deadline:?}"
            ),
            Self::Other(err) => std::fmt::Display::fmt(err, f),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(_) | Self::Timeout { .. } => None,
            // Mirror thiserror's `#[error(transparent)]`: delegate to the
            // wrapped error's source so the chain is not duplicated.
            Self::Other(err) => {
                let inner: &(dyn std::error::Error + 'static) = err.as_ref();
                inner.source()
            }
        }
    }
}

impl From<anyhow::Error> for TransportError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

/// Map a rio-nix [`ClientOpError`] to the transport error taxonomy.
///
/// `Daemon` errors are clean refusals (protocol framing intact). `Wire`
/// errors normally mean the channel is unusable — except during the two
/// upload ops (`upload = true`), where the daemon may refuse mid-payload and
/// tear the session down before the client finishes writing, surfacing as an
/// I/O error; those are reported as [`TransportError::Refused`] so the
/// caller treats them like a rejection rather than a transport bug.
fn map_client_op_error(err: ClientOpError, op: &str, upload: bool) -> TransportError {
    match err {
        ClientOpError::Daemon(daemon_err) => TransportError::Refused(daemon_err.message),
        ClientOpError::Wire(wire_err) if upload => TransportError::Refused(format!(
            "transport failed during upload (may be a refusal racing session teardown): {wire_err}"
        )),
        ClientOpError::Wire(wire_err) => TransportError::Other(
            anyhow::Error::new(wire_err).context(format!("{op} failed on the daemon channel")),
        ),
    }
}

/// Run one rio-nix client op under a deadline and map its outcome.
/// `connection` is the pool index of the SSH connection the channel runs on,
/// carried into timeout errors for triage.
async fn run_op<T>(
    op_future: impl Future<Output = std::result::Result<T, ClientOpError>>,
    deadline: Duration,
    op: &str,
    connection: usize,
    upload: bool,
) -> std::result::Result<T, TransportError> {
    match tokio::time::timeout(deadline, op_future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(map_client_op_error(err, op, upload)),
        Err(_elapsed) => Err(TransportError::Timeout {
            op: op.to_string(),
            connection,
            deadline,
        }),
    }
}

/// Parse a non-fingerprint pin: an inline OpenSSH public-key line
/// (`ssh-ed25519 AAAA… comment`) or a path to a public-key file.
fn parse_pinned_public_key(pin: &str) -> Result<PublicKey> {
    if pin.starts_with("ssh-") {
        PublicKey::from_openssh(pin)
            .map_err(|err| anyhow!("invalid pinned host-key line {pin:?}: {err}"))
    } else {
        russh::keys::load_public_key(pin)
            .map_err(|err| anyhow!("failed to load pinned host key file {pin:?}: {err}"))
    }
}

/// Compare the offered key against a pin: a `SHA256:...`/`SHA512:...`
/// fingerprint string, an inline OpenSSH public-key line, or a path to an
/// OpenSSH public-key file.
///
/// Returns `Ok(true)` when the key matches, `Ok(false)` when it does not
/// (the SSH handler turns that into a hard, descriptive error), and `Err`
/// when the pin itself cannot be evaluated (unparseable fingerprint or key
/// line, unreadable pin file). Mismatches are never reported as `Ok(true)`:
/// every path fails closed.
fn evaluate_pinned_key(pin: &str, offered: &PublicKey) -> Result<bool> {
    if pin.starts_with("SHA256:") || pin.starts_with("SHA512:") {
        let pinned: Fingerprint = pin
            .parse()
            .map_err(|err| anyhow!("invalid pinned host-key fingerprint {pin:?}: {err}"))?;
        Ok(offered.fingerprint(pinned.algorithm()) == pinned)
    } else {
        let pinned = parse_pinned_public_key(pin)?;
        // Compare key material only — comments differ between the pin and
        // the key offered on the wire.
        Ok(pinned.key_data() == offered.key_data())
    }
}

/// SHA-256 fingerprint of the configured pin, for mismatch error messages.
/// `None` when the pin cannot be rendered as a fingerprint (it failed to
/// parse, or the pin file is unreadable) — callers fall back to quoting the
/// pin string itself.
fn pin_fingerprint(pin: &str) -> Option<String> {
    if pin.starts_with("SHA256:") || pin.starts_with("SHA512:") {
        return Some(pin.to_string());
    }
    parse_pinned_public_key(pin)
        .ok()
        .map(|key| key.fingerprint(HashAlg::Sha256).to_string())
}

/// russh client handler: the only callback that matters for this transport
/// is host-key verification against the configured pin.
struct TransportHandler {
    policy: Arc<HostKeyPolicy>,
    host: String,
    port: u16,
}

impl russh::client::Handler for TransportHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        let offered = server_public_key.fingerprint(HashAlg::Sha256);
        let HostKeyPolicy::Pinned(pin) = self.policy.as_ref();
        let accepted = evaluate_pinned_key(pin, server_public_key).with_context(|| {
            format!(
                "host key verification failed for {}:{} (offered key {offered})",
                self.host, self.port
            )
        })?;
        if !accepted {
            // Fail closed with an actionable message naming both keys instead
            // of russh's generic "unknown key" error.
            let expected = pin_fingerprint(pin).unwrap_or_else(|| format!("{pin:?}"));
            bail!(
                "gateway host key mismatch for {}:{}: offered {offered}, expected {expected} \
                 (pinned via cluster.gateway_host_key in the campaign spec)",
                self.host,
                self.port
            );
        }
        tracing::debug!(host = %self.host, port = self.port, key = %offered, "gateway host key matches the configured pin");
        Ok(true)
    }
}

/// One SSH connection slot plus its channel-slot budget. Starts unconnected;
/// the pool dials it lazily the first time a channel is needed on it.
struct PoolConnection {
    /// The authenticated SSH connection, `None` until first dialed. Behind an
    /// `RwLock` so a connection the gateway has dropped can be replaced
    /// (re-dialed) in place: liveness checks and channel opens take the read
    /// side, a (re-)dial takes the write side.
    handle: RwLock<Option<russh::client::Handle<TransportHandler>>>,
    /// Free channel slots on this connection (the gateway's per-connection
    /// budget). Slots are held by [`DaemonChannel`]s — including ones whose
    /// underlying connection has died; those free up as their ops fail and
    /// the channels get dropped.
    slots: Arc<Semaphore>,
    /// Position in [`GatewayPool::connections`], for logs and errors.
    index: usize,
}

/// A pool of authenticated SSH connections to rio-gateway, dialed lazily.
///
/// Every connection carries a [`CHANNELS_PER_CONNECTION`]-permit semaphore
/// mirroring the gateway's per-connection channel cap. [`Self::open_channel`]
/// prefers the open connection with the most free slots and waits (without
/// deadlocking) when all `connections × 4` slots are in use; the slot is
/// released when the returned [`DaemonChannel`] is dropped or
/// [abandoned](DaemonChannel::abandon).
///
/// The gateway disconnects an SSH connection as soon as its last channel
/// closes, so pool connections routinely go dead between submissions. The
/// pool treats that as normal: closed (or never-dialed) connections are
/// skipped when picking a slot and dialed lazily (one attempt per
/// acquisition) when they are needed. In-flight operations on a connection
/// that dies still fail — the caller's retry-on-a-fresh-channel path is the
/// recovery mechanism, not a transparent transport retry.
///
/// The pool owns the SSH connections: keep it alive for as long as any
/// [`DaemonChannel`] handed out from it is in use.
pub struct GatewayPool {
    connections: Vec<PoolConnection>,
    /// Gateway endpoint, kept for lazy dials and error messages.
    endpoint: GatewayEndpoint,
    /// Decoded client key, loaded from `endpoint.ssh_key_path` on first dial.
    key: OnceCell<Arc<russh::keys::PrivateKey>>,
    /// Host-key policy, applied on every dial.
    policy: Arc<HostKeyPolicy>,
}

impl GatewayPool {
    /// Create a pool of `connections` connection slots without performing any
    /// I/O: the SSH key is read and the connections are dialed lazily, the
    /// first time a channel is requested.
    ///
    /// The private key at the endpoint's `ssh_key_path` must be
    /// passphrase-less; its public half must be authorized on the gateway
    /// (the key comment selects the tenant). The host-key pin must be
    /// non-empty: running without one would disable SSH host-key
    /// verification, so it is rejected here rather than at first dial.
    pub fn new(
        endpoint: GatewayEndpoint,
        policy: HostKeyPolicy,
        connections: usize,
    ) -> Result<Self> {
        ensure!(
            connections > 0,
            "the gateway pool needs at least one SSH connection"
        );
        let HostKeyPolicy::Pinned(pin) = &policy;
        ensure!(
            !pin.trim().is_empty(),
            "the gateway transport requires a host-key pin: set cluster.gateway_host_key in the \
             campaign spec (the launcher reads it from the gateway host-key Secret); an empty pin \
             would disable SSH host-key verification"
        );
        let connections = (0..connections)
            .map(|index| PoolConnection {
                handle: RwLock::new(None),
                slots: Arc::new(Semaphore::new(CHANNELS_PER_CONNECTION)),
                index,
            })
            .collect();
        Ok(Self {
            connections,
            endpoint,
            key: OnceCell::new(),
            policy: Arc::new(policy),
        })
    }

    /// Total channel capacity (connections × 4).
    pub fn capacity(&self) -> usize {
        pool_capacity(self.connections.len())
    }

    /// Open a daemon session: pick a connection with a free channel slot
    /// (dialing it if it has never been dialed or the gateway dropped it),
    /// open a channel, exec `nix-daemon --stdio`, run the worker-protocol
    /// handshake, and return the channel.
    ///
    /// Waits if all slots are momentarily busy. The wait is unbounded by
    /// design — callers are expected to do their own admission control (the
    /// campaign engine bounds concurrency before asking for a channel). It
    /// cannot deadlock because every slot is released when its
    /// [`DaemonChannel`] drops. Connections the gateway has closed in the
    /// meantime (it disconnects whenever a connection's last channel closes)
    /// are skipped and re-dialed lazily.
    ///
    /// One channel-open race is absorbed internally: when the exec/open step
    /// lands on a connection the gateway had already closed, or the gateway
    /// rejects the exec because it has not finished processing an earlier
    /// channel close (both routine at saturation — closes are processed
    /// asynchronously), the open is retried once on a freshly acquired slot
    /// before the error is surfaced.
    pub async fn open_channel(&self) -> Result<DaemonChannel> {
        match self.open_channel_once().await {
            Err(err) if err.downcast_ref::<ChannelOpenRace>().is_some() => {
                tracing::debug!(
                    error = %format!("{err:#}"),
                    "channel open raced a gateway-side close or budget rejection; retrying once on a fresh slot"
                );
                self.open_channel_once().await
            }
            other => other,
        }
    }

    /// One attempt at [`Self::open_channel`]: acquire a slot, exec the
    /// daemon, run the handshake.
    async fn open_channel_once(&self) -> Result<DaemonChannel> {
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
            poisoned: None,
            _slot: slot,
        })
    }

    /// Acquire one channel slot.
    ///
    /// Preference order: an open connection with the most free slots; then
    /// closed or never-dialed connections (the gateway drops a connection
    /// whenever its last channel closes, so finding some is routine), each
    /// dialed at most once per pass; finally, when every usable connection is
    /// fully busy, wait for the first slot to free anywhere and re-validate.
    /// Errors only when no connection is open and every dial failed.
    async fn acquire_slot(&self) -> Result<(&PoolConnection, OwnedSemaphorePermit)> {
        let mut last_dial_error: Option<anyhow::Error> = None;
        loop {
            // Snapshot free slots and liveness. `try_read` so a dial in
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
                        .map(|handle| handle.as_ref().is_some_and(|h| !h.is_closed()))
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

            // No open connection has a free slot. Revive closed/undialed
            // connections (most free slots first), one dial attempt each;
            // collect everything usable for the waiting path below.
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
                        // Dialed, but its slots are still held by stale
                        // channels that have not been dropped yet.
                        usable.push(connection);
                    }
                    Err(err) => {
                        tracing::warn!(
                            connection = index,
                            error = %format!("{err:#}"),
                            "dial of closed SSH connection failed; skipping it for this slot"
                        );
                        last_dial_error = Some(err);
                    }
                }
            }

            if usable.is_empty() {
                let detail = last_dial_error
                    .as_ref()
                    .map(|err| format!("{err:#}"))
                    .unwrap_or_else(|| "no dial was attempted".to_string());
                bail!(
                    "all {} SSH connections to {} are closed (the gateway disconnects a \
                     connection when its last channel closes); dialing failed: {detail}",
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
                    last_dial_error = Some(err);
                    // Other connections may still be usable; take another pass.
                }
            }
        }
    }

    /// Make sure `connection` has a live SSH connection, dialing it if it has
    /// never been dialed or the gateway has dropped it. Concurrent callers
    /// serialize on the connection's write lock; whoever loses that race
    /// finds a fresh handle and skips its own dial. The write lock is held
    /// across the dial, but the dial itself is bounded by [`DIAL_TIMEOUT`],
    /// so the lock can never be held across an unbounded await.
    async fn ensure_open(&self, connection: &PoolConnection) -> Result<()> {
        if connection
            .handle
            .read()
            .await
            .as_ref()
            .is_some_and(|handle| !handle.is_closed())
        {
            return Ok(());
        }
        let mut handle = connection.handle.write().await;
        // Re-check under the write lock: another acquirer may have dialed
        // while we waited for it.
        if handle.as_ref().is_some_and(|h| !h.is_closed()) {
            return Ok(());
        }
        tracing::debug!(
            connection = connection.index,
            endpoint = %self.endpoint,
            "dialing SSH connection (first use, or the gateway dropped it on last channel close)"
        );
        *handle = Some(self.dial(connection.index).await.with_context(|| {
            format!(
                "dial of SSH connection {} to {} failed",
                connection.index, self.endpoint
            )
        })?);
        // Slot permits still held by DaemonChannels from the previous
        // incarnation keep the re-dialed connection under-utilized until
        // those stale channels error out and drop.
        Ok(())
    }

    /// Load (once) the client private key from the endpoint's key path.
    async fn load_key(&self) -> Result<Arc<russh::keys::PrivateKey>> {
        self.key
            .get_or_try_init(|| async {
                let path = &self.endpoint.ssh_key_path;
                let key = russh::keys::load_secret_key(path, None).map_err(|err| match err {
                    russh::keys::Error::KeyIsEncrypted => anyhow!(
                        "SSH key {} is passphrase-protected; the gateway transport only \
                         supports passphrase-less keys",
                        path.display()
                    ),
                    other => anyhow::Error::new(other)
                        .context(format!("failed to load SSH private key {}", path.display())),
                })?;
                Ok(Arc::new(key))
            })
            .await
            .cloned()
    }

    /// Dial and authenticate one SSH connection to the gateway. Used both for
    /// the first lazy dial of a connection slot and for re-dialing one the
    /// gateway has closed (it disconnects a connection whenever its last
    /// channel closes).
    async fn dial(&self, index: usize) -> Result<russh::client::Handle<TransportHandler>> {
        let key = self.load_key().await?;
        let connect_and_auth = async {
            let config = Arc::new(russh::client::Config {
                keepalive_interval: Some(KEEPALIVE_INTERVAL),
                keepalive_max: KEEPALIVE_MAX,
                nodelay: true,
                ..Default::default()
            });
            let handler = TransportHandler {
                policy: self.policy.clone(),
                host: self.endpoint.host.clone(),
                port: self.endpoint.port,
            };
            let mut handle = russh::client::connect(
                config,
                (self.endpoint.host.as_str(), self.endpoint.port),
                handler,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to establish SSH connection {index} to {}",
                    self.endpoint
                )
            })?;

            // The gateway only advertises publickey auth. For RSA keys ask
            // which rsa-sha2 variant the server supports; for anything else
            // (ed25519 in practice) the hash parameter is ignored.
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
                .authenticate_publickey(
                    self.endpoint.user.as_str(),
                    PrivateKeyWithHashAlg::new(key.clone(), hash_alg),
                )
                .await
                .with_context(|| {
                    format!(
                        "publickey authentication failed on connection {index} to {}",
                        self.endpoint
                    )
                })?;
            ensure!(
                auth.success(),
                "the gateway rejected the SSH key {} — its public half must be in the gateway's \
                 authorized keys with the key comment naming the campaign's tenant",
                self.endpoint.ssh_key_path.display()
            );
            Ok::<_, anyhow::Error>(handle)
        };
        // Bounded so a blackholed endpoint (or a half-dead gateway that
        // accepts TCP but never finishes the SSH exchange) fails the dial
        // instead of wedging the caller — and the per-connection write lock
        // ensure_open holds across this call — indefinitely.
        let handle = tokio::time::timeout(DIAL_TIMEOUT, connect_and_auth)
            .await
            .map_err(|_| {
                anyhow!(
                    "dialing SSH connection {index} to {} timed out after {}s \
                     (TCP connect + key exchange + publickey auth)",
                    self.endpoint,
                    DIAL_TIMEOUT.as_secs()
                )
            })??;
        tracing::debug!(index, endpoint = %self.endpoint, "gateway SSH connection authenticated");
        Ok(handle)
    }
}

/// Marker context attached to exec/channel-open failures caused by the
/// gateway having already closed the connection (it disconnects a connection
/// whenever its last channel closes) or not yet having processed an earlier
/// channel close when it re-checked its per-connection budget — both routine
/// races at saturation. [`GatewayPool::open_channel`] absorbs exactly one
/// such failure by retrying on a freshly acquired slot.
#[derive(Debug, Clone, Copy)]
struct ChannelOpenRace;

impl std::fmt::Display for ChannelOpenRace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("channel open raced a gateway connection close or channel-budget rejection")
    }
}

/// Open one session channel on `connection`, exec the daemon command, and
/// wait for the gateway to confirm it. `exec` only sends the request; the
/// accept/reject verdict arrives as a channel message (the gateway re-checks
/// its 4-channel budget at exec time).
///
/// Failures caused by the connection having been closed under us, or by the
/// gateway rejecting the exec, carry the [`ChannelOpenRace`] context so
/// [`GatewayPool::open_channel`] can absorb them with one retry.
async fn exec_daemon(
    connection: &PoolConnection,
    endpoint: &GatewayEndpoint,
) -> Result<russh::Channel<russh::client::Msg>> {
    let mut channel = {
        let guard = connection.handle.read().await;
        let Some(handle) = guard.as_ref() else {
            bail!(
                "SSH connection {} to {endpoint} has not been dialed",
                connection.index
            );
        };
        if handle.is_closed() {
            return Err(anyhow!(
                "SSH connection {} to {endpoint} is closed (gateway disconnect or keepalive \
                 timeout)",
                connection.index
            )
            .context(ChannelOpenRace));
        }
        handle
            .channel_open_session()
            .await
            .context("failed to open an SSH session channel")
            .context(ChannelOpenRace)?
    };
    channel
        .exec(true, DAEMON_COMMAND)
        .await
        .context("failed to send the nix-daemon exec request")
        .context(ChannelOpenRace)?;
    // Bailing on any arm below (or later, before the handshake completes)
    // does not leak the server-side slot: our dropped channel sends an SSH
    // close, and the gateway's own 30s handshake reaper tears down sessions
    // that never complete the worker-protocol handshake.
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Success) => return Ok(channel),
            Some(russh::ChannelMsg::Failure) => {
                return Err(anyhow!(
                    "gateway refused `{DAEMON_COMMAND}` on a new channel \
                     (per-connection channel budget exhausted?)"
                )
                .context(ChannelOpenRace));
            }
            // Flow-control noise can arrive ahead of the exec verdict.
            Some(russh::ChannelMsg::WindowAdjusted { .. }) => continue,
            Some(other) => bail!(
                "unexpected SSH channel message while waiting for the exec verdict: {other:?}"
            ),
            None => {
                return Err(anyhow!(
                    "SSH channel closed before the gateway confirmed the exec request"
                )
                .context(ChannelOpenRace));
            }
        }
    }
}

/// The bidirectional byte stream of one exec'd gateway channel.
type GatewayStream = russh::ChannelStream<russh::client::Msg>;

/// Whether an op error leaves the channel's wire position unknown, making
/// the channel unusable for further ops: timeouts (the op was cut off
/// mid-read/-write) and transport failures always do; a refusal does only
/// for upload ops, which may have started writing a framed payload before
/// the daemon refused.
fn poisons_channel(err: &TransportError, upload: bool) -> bool {
    match err {
        TransportError::Refused(_) => upload,
        TransportError::Timeout { .. } | TransportError::Other(_) => true,
    }
}

/// One `nix-daemon --stdio` session over an SSH channel, handshake done.
///
/// Methods take `&mut self`: the gateway runs the daemon protocol
/// sequentially per channel, so a channel can only ever run one operation at
/// a time. Run independent operations on separate channels from
/// [`GatewayPool::open_channel`].
///
/// After an error that desyncs the wire (a timeout, a transport failure, or
/// a refused upload — see `poisons_channel`) the channel cannot be
/// resynchronized; it remembers the failure and every subsequent op fails
/// fast with an error saying to open a fresh channel. A clean daemon refusal
/// on a non-upload op leaves the protocol position known and the channel
/// usable.
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
    /// Set to a short description of the first wire-desyncing failure; once
    /// set, every further op fails fast (see [`poisons_channel`]).
    poisoned: Option<String>,
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

    /// Fail fast when an earlier op left the wire position unknown.
    fn ensure_usable(&self, op: &str) -> std::result::Result<(), TransportError> {
        match &self.poisoned {
            None => Ok(()),
            Some(reason) => Err(TransportError::Other(anyhow!(
                "daemon channel on gateway connection {} is unusable after an earlier error \
                 ({reason}); open a new channel instead of retrying {op} on this one",
                self.connection_index
            ))),
        }
    }

    /// Record an op outcome, poisoning the channel when the error leaves the
    /// wire position unknown. The first poisoning failure wins.
    fn note_outcome<T>(
        &mut self,
        op: &str,
        upload: bool,
        result: &std::result::Result<T, TransportError>,
    ) {
        if let Err(err) = result
            && poisons_channel(err, upload)
            && self.poisoned.is_none()
        {
            self.poisoned = Some(format!("{err} during {op}"));
        }
    }

    /// `wopQueryValidPaths`: which of `paths` does the target already have?
    /// (`substitute = false`, mirroring `nix copy`.)
    pub async fn query_valid_paths(
        &mut self,
        paths: &[String],
        timeout: Duration,
    ) -> std::result::Result<BTreeSet<String>, TransportError> {
        let op = format!("QueryValidPaths ({} paths)", paths.len());
        self.ensure_usable(&op)?;
        let result = run_op(
            client_query_valid_paths(&mut self.reader, &mut self.writer, paths, false),
            timeout,
            &op,
            self.connection_index,
            false,
        )
        .await;
        self.note_outcome(&op, false, &result);
        result
    }

    /// `wopQueryPathInfo`: one path's [`ValidPathInfo`], `None` if absent.
    pub async fn query_path_info(
        &mut self,
        path: &str,
        timeout: Duration,
    ) -> std::result::Result<Option<ValidPathInfo>, TransportError> {
        let op = format!("QueryPathInfo {path}");
        self.ensure_usable(&op)?;
        let result = run_op(
            client_query_path_info(&mut self.reader, &mut self.writer, path),
            timeout,
            &op,
            self.connection_index,
            false,
        )
        .await;
        self.note_outcome(&op, false, &result);
        result
    }

    /// `wopAddMultipleToStore`: upload a batch of store paths in one framed
    /// stream (`repair = false`, `dont_check_sigs = true`).
    pub async fn add_multiple_to_store(
        &mut self,
        entries: Vec<StoreEntry>,
        timeout: Duration,
    ) -> std::result::Result<(), TransportError> {
        let op = format!("AddMultipleToStore ({} entries)", entries.len());
        self.ensure_usable(&op)?;
        let result = run_op(
            client_add_multiple_to_store(&mut self.reader, &mut self.writer, false, true, entries),
            timeout,
            &op,
            self.connection_index,
            true,
        )
        .await;
        self.note_outcome(&op, true, &result);
        result
    }

    /// `wopAddToStoreNar`: upload one store path with its NAR serialization
    /// (`repair = false`, `dont_check_sigs = true`).
    pub async fn add_to_store_nar(
        &mut self,
        entry: StoreEntry,
        timeout: Duration,
    ) -> std::result::Result<(), TransportError> {
        let op = format!("AddToStoreNar {}", entry.store_path);
        self.ensure_usable(&op)?;
        let result = run_op(
            client_add_to_store_nar(&mut self.reader, &mut self.writer, entry, false, true),
            timeout,
            &op,
            self.connection_index,
            true,
        )
        .await;
        self.note_outcome(&op, true, &result);
        result
    }

    /// `wopBuildPathsWithResults`: build the given derived paths and collect
    /// the daemon's per-path results (submission order).
    pub async fn build_paths_with_results(
        &mut self,
        derived: &[String],
        timeout: Duration,
    ) -> std::result::Result<Vec<KeyedBuildResult>, TransportError> {
        let op = format!("BuildPathsWithResults ({} paths)", derived.len());
        self.ensure_usable(&op)?;
        let negotiated_version = self.negotiated_version;
        let result = run_op(
            client_build_paths_with_results(
                &mut self.reader,
                &mut self.writer,
                derived,
                negotiated_version,
            ),
            timeout,
            &op,
            self.connection_index,
            false,
        )
        .await;
        self.note_outcome(&op, false, &result);
        result
    }

    /// [`Self::build_paths_with_results`] with a stderr log-line observer:
    /// every relayed daemon log line is passed to `observer` while the build
    /// runs, so the engine can capture the gateway's `rio: build <uuid>`
    /// announcement and the relayed `derivation '<drv>' failed:` lines as
    /// evidence. Deadline handling and error mapping are identical to the
    /// unobserved method.
    pub async fn build_paths_with_results_observed(
        &mut self,
        derived: &[String],
        timeout: Duration,
        observer: &mut (dyn FnMut(&str) + Send),
    ) -> std::result::Result<Vec<KeyedBuildResult>, TransportError> {
        let op = format!("BuildPathsWithResults ({} paths)", derived.len());
        self.ensure_usable(&op)?;
        let negotiated_version = self.negotiated_version;
        let result = run_op(
            client_build_paths_with_results_observed(
                &mut self.reader,
                &mut self.writer,
                derived,
                negotiated_version,
                observer,
            ),
            timeout,
            &op,
            self.connection_index,
            false,
        )
        .await;
        self.note_outcome(&op, false, &result);
        result
    }

    /// Drop the channel abruptly without a daemon-level goodbye — the
    /// engine's cancellation mechanism (batch deadline, shutdown). The
    /// gateway cancels the session's builds. Releases the pool slot.
    ///
    /// This is the same as dropping the channel: the worker protocol has no
    /// goodbye message, and on drop russh sends a best-effort SSH channel
    /// close, which the gateway treats as the client going away.
    pub fn abandon(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rio_nix::protocol::stderr::StderrError;
    use rio_nix::protocol::wire::WireError;
    use russh::client::Handler as _;
    use russh::keys::{Algorithm, PrivateKey};

    use super::*;

    fn ed25519_key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("generate test key")
    }

    /// Pin string for a path-based [`HostKeyPolicy::Pinned`] (temp paths are
    /// valid UTF-8 in these tests).
    fn pin_path(path: &Path) -> String {
        path.to_str().expect("temp path is valid UTF-8").to_string()
    }

    fn endpoint_with_key(key_path: &str) -> GatewayEndpoint {
        GatewayEndpoint {
            host: "rio-gateway.rio-system.svc".into(),
            port: 22,
            user: "rio".into(),
            ssh_key_path: PathBuf::from(key_path),
        }
    }

    #[test]
    fn gateway_endpoint_parse_extracts_host_port_user_and_key() {
        let parsed = GatewayEndpoint::parse(
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/etc/rio/parity-ssh/parity-leaf",
        )
        .expect("spec-shaped store URL parses");
        assert_eq!(parsed.host, "rio-gateway.rio-system.svc");
        assert_eq!(parsed.port, 22);
        assert_eq!(parsed.user, "rio");
        assert_eq!(
            parsed.ssh_key_path,
            PathBuf::from("/etc/rio/parity-ssh/parity-leaf")
        );

        // Missing user and port fall back to the documented defaults.
        let defaulted = GatewayEndpoint::parse("ssh-ng://gw.example?ssh-key=/k").unwrap();
        assert_eq!(defaulted.host, "gw.example");
        assert_eq!(defaulted.port, 22);
        assert_eq!(defaulted.user, "rio");
        assert_eq!(defaulted.ssh_key_path, PathBuf::from("/k"));

        // Explicit port and a bracketed IPv6 host.
        let v6 = GatewayEndpoint::parse("ssh-ng://rio@[::1]:2222?ssh-key=/k").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.port, 2222);

        // A missing ssh-key parameter is an error naming the parameter.
        let err = GatewayEndpoint::parse("ssh-ng://rio@gw:22?compress=true").unwrap_err();
        assert!(err.to_string().contains("ssh-key="), "{err:#}");

        // Non-ssh-ng schemes are rejected naming the required scheme.
        let err = GatewayEndpoint::parse("https://gw.example?ssh-key=/k").unwrap_err();
        assert!(err.to_string().contains("ssh-ng"), "{err:#}");

        // An invalid port is an error rather than a silent default.
        assert!(GatewayEndpoint::parse("ssh-ng://gw:notaport?ssh-key=/k").is_err());
    }

    #[test]
    fn default_connections_is_ceil_div_4_min_1() {
        assert_eq!(default_connections(0), 1);
        assert_eq!(default_connections(1), 1);
        assert_eq!(default_connections(4), 1);
        assert_eq!(default_connections(5), 2);
        assert_eq!(default_connections(8), 2);
        assert_eq!(default_connections(9), 3);
        assert_eq!(default_connections(32), 8);
        // Capacity follows the gateway's per-connection channel budget.
        assert_eq!(pool_capacity(1), 4);
        assert_eq!(pool_capacity(8), 32);
    }

    #[test]
    fn pinned_policy_matches_fingerprint_and_pubkey_line() -> Result<()> {
        let offered_key = ed25519_key();
        let offered = offered_key.public_key();
        let other_key = ed25519_key();

        // Matching SHA256 fingerprint pin.
        let matching = offered.fingerprint(HashAlg::Sha256).to_string();
        assert!(
            matching.starts_with("SHA256:"),
            "fingerprint format: {matching}"
        );
        assert!(evaluate_pinned_key(&matching, offered)?);

        // A mismatching fingerprint is Ok(false) — the SSH handler turns that
        // into a hard, descriptive error; it must never be Ok(true).
        let mismatching = other_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        assert!(!evaluate_pinned_key(&mismatching, offered)?);

        // Pin by inline OpenSSH public-key line; the comment is ignored
        // because only key material is compared.
        let line = offered.to_openssh()?;
        assert!(line.starts_with("ssh-ed25519 "), "key line: {line}");
        assert!(evaluate_pinned_key(&line, offered)?);
        let commented = format!("{} rio-gateway-host-key", line.trim_end());
        assert!(evaluate_pinned_key(&commented, offered)?);
        let other_line = other_key.public_key().to_openssh()?;
        assert!(!evaluate_pinned_key(&other_line, offered)?);

        // Pin by public-key file path.
        let dir = tempfile::tempdir()?;
        let matching_path = dir.path().join("gateway_host_key.pub");
        std::fs::write(&matching_path, offered.to_openssh()?)?;
        assert!(evaluate_pinned_key(&pin_path(&matching_path), offered)?);

        // A file with a different key must not match.
        let other_path = dir.path().join("other_host_key.pub");
        std::fs::write(&other_path, other_key.public_key().to_openssh()?)?;
        assert!(!evaluate_pinned_key(&pin_path(&other_path), offered)?);

        // A missing pin file fails closed with an error.
        assert!(evaluate_pinned_key(&pin_path(&dir.path().join("missing.pub")), offered).is_err());
        Ok(())
    }

    #[test]
    fn pool_construction_requires_a_nonempty_host_key_pin() {
        let endpoint = endpoint_with_key("/does/not/exist/parity-leaf");

        // An empty (or whitespace-only) pin is a hard construction error
        // pointing at the spec field, not a silently unverified transport.
        for pin in ["", "   "] {
            let err =
                match GatewayPool::new(endpoint.clone(), HostKeyPolicy::Pinned(pin.to_string()), 2)
                {
                    Ok(_) => panic!("an empty host-key pin must fail pool construction"),
                    Err(err) => err,
                };
            assert!(
                err.to_string().contains("cluster.gateway_host_key"),
                "{err:#}"
            );
        }

        // Zero connections cannot serve any channel.
        assert!(
            GatewayPool::new(
                endpoint.clone(),
                HostKeyPolicy::Pinned("SHA256:placeholder".into()),
                0,
            )
            .is_err()
        );

        // With a pin the pool is built without any I/O: the key path does not
        // exist and the host is not resolvable, yet construction succeeds and
        // reports the channel budget — dialing is deferred to first use.
        let pool = GatewayPool::new(
            endpoint,
            HostKeyPolicy::Pinned("SHA256:placeholder".into()),
            3,
        )
        .expect("lazy pool construction performs no I/O");
        assert_eq!(pool.capacity(), 12);
    }

    #[tokio::test]
    async fn pinned_mismatch_is_a_hard_error_naming_both_fingerprints() {
        let offered_key = ed25519_key();
        let offered = offered_key.public_key();
        let pinned_key = ed25519_key();
        let pin = pinned_key.public_key().to_openssh().unwrap();

        // A mismatching offered key is rejected with an error naming the
        // offered and the expected fingerprints (never a silent accept).
        let mut handler = TransportHandler {
            policy: Arc::new(HostKeyPolicy::Pinned(pin)),
            host: "rio-gateway.rio-system.svc".into(),
            port: 22,
        };
        let err = handler
            .check_server_key(offered)
            .await
            .expect_err("a mismatched host key must be a hard error");
        let msg = format!("{err:#}");
        let offered_fp = offered.fingerprint(HashAlg::Sha256).to_string();
        let expected_fp = pinned_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        assert!(msg.contains(&offered_fp), "offered fingerprint in: {msg}");
        assert!(msg.contains(&expected_fp), "expected fingerprint in: {msg}");

        // The matching key is accepted.
        let mut handler = TransportHandler {
            policy: Arc::new(HostKeyPolicy::Pinned(offered.to_openssh().unwrap())),
            host: "rio-gateway.rio-system.svc".into(),
            port: 22,
        };
        assert!(handler.check_server_key(offered).await.unwrap());
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
        // to dial closed connections instead.
        assert_eq!(pick_connection(&[false, false], &[4, 4]), None);
        assert_eq!(pick_connection(&[], &[]), None);
    }

    #[test]
    fn transport_error_mapping() {
        // Daemon refusal → Refused carrying the daemon's message.
        let daemon = ClientOpError::Daemon(StderrError::simple("rio-gateway", "path not allowed"));
        match map_client_op_error(daemon, "AddToStoreNar /nix/store/x", true) {
            TransportError::Refused(msg) => {
                assert!(msg.contains("path not allowed"), "message: {msg}");
            }
            other => panic!("daemon error must map to Refused, got {other:?}"),
        }

        // Wire error on a non-upload op → Other, context names the op.
        let wire = ClientOpError::Wire(WireError::Io(std::io::Error::other("broken pipe")));
        match map_client_op_error(wire, "QueryPathInfo /nix/store/x", false) {
            TransportError::Other(err) => {
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
            TransportError::Refused(msg) => {
                assert!(msg.contains("racing session teardown"), "message: {msg}");
                assert!(
                    msg.contains("connection reset"),
                    "message keeps the cause: {msg}"
                );
            }
            other => panic!("wire error during upload must map to Refused, got {other:?}"),
        }
    }

    #[test]
    fn timeout_error_names_op_and_connection() {
        let err = TransportError::Timeout {
            op: "QueryValidPaths (2000 paths)".into(),
            connection: 3,
            deadline: Duration::from_secs(120),
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("QueryValidPaths (2000 paths)"),
            "{rendered}"
        );
        assert!(rendered.contains("connection 3"), "{rendered}");
        assert!(rendered.contains("120"), "{rendered}");
    }

    #[test]
    fn op_errors_poison_the_channel_per_wire_position_rules() {
        let refused = TransportError::Refused("path not allowed".into());
        let timeout = TransportError::Timeout {
            op: "AddMultipleToStore (10 entries)".into(),
            connection: 0,
            deadline: Duration::from_secs(5),
        };
        let other = TransportError::Other(anyhow!("broken pipe"));

        // A clean refusal leaves the protocol position known on query/build
        // ops (no poison) but not on upload ops, which may have started a
        // framed payload.
        assert!(!poisons_channel(&refused, false));
        assert!(poisons_channel(&refused, true));
        // Timeouts and transport failures always desync the wire.
        assert!(poisons_channel(&timeout, false));
        assert!(poisons_channel(&timeout, true));
        assert!(poisons_channel(&other, false));
        assert!(poisons_channel(&other, true));
    }
}
