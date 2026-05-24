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
use std::path::Path;
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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Channels the gateway accepts per SSH connection; further opens (or execs)
/// are rejected until one closes.
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
    handle: russh::client::Handle<ReplayHandler>,
    slots: Arc<Semaphore>,
    index: usize,
}

/// A pool of authenticated SSH connections to rio-gateway.
///
/// Every connection carries a [`CHANNELS_PER_CONNECTION`]-permit semaphore
/// mirroring the gateway's per-connection channel cap. [`Self::open_channel`]
/// prefers the connection with the most free slots and waits (without
/// deadlocking) when all `connections × 4` slots are in use; the slot is
/// released when the returned [`DaemonChannel`] is dropped or
/// [abandoned](DaemonChannel::abandon).
///
/// The pool owns the SSH connections: keep it alive for as long as any
/// [`DaemonChannel`] handed out from it is in use.
pub struct GatewayPool {
    connections: Vec<PoolConnection>,
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

        let config = Arc::new(russh::client::Config {
            keepalive_interval: Some(KEEPALIVE_INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            nodelay: true,
            ..Default::default()
        });
        let policy = Arc::new(policy);

        let connections = futures_util::future::try_join_all((0..connections).map(|index| {
            let config = config.clone();
            let key = key.clone();
            let policy = policy.clone();
            async move {
                let handler = ReplayHandler {
                    policy,
                    host: endpoint.host.clone(),
                    port: endpoint.port,
                };
                let mut handle = russh::client::connect(
                    config,
                    (endpoint.host.as_str(), endpoint.port),
                    handler,
                )
                .await
                .with_context(|| {
                    format!("failed to establish SSH connection {index} to {endpoint}")
                })?;

                // The gateway only advertises publickey auth. For RSA keys ask
                // which rsa-sha2 variant the server supports; for anything
                // else (ed25519 in practice) the hash parameter is ignored.
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
                        "rio",
                        PrivateKeyWithHashAlg::new(key.clone(), hash_alg),
                    )
                    .await
                    .with_context(|| {
                        format!("publickey authentication failed on connection {index} to {endpoint}")
                    })?;
                ensure!(
                    auth.success(),
                    "the gateway rejected the SSH key {} (is its public half in the gateway's \
                     authorized keys, with the comment naming the tenant?)",
                    key_path.display()
                );
                tracing::debug!(index, endpoint = %endpoint, "gateway SSH connection authenticated");
                Ok(PoolConnection {
                    handle,
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
        Ok(Self { connections })
    }

    /// Total channel capacity (connections × 4).
    pub fn capacity(&self) -> usize {
        pool_capacity(self.connections.len())
    }

    /// Open a daemon session: pick a connection with a free channel slot,
    /// open a channel, exec `nix-daemon --stdio`, run the worker-protocol
    /// handshake, and return the channel. Waits if all slots are momentarily
    /// busy; the wait cannot deadlock because every slot is released when its
    /// [`DaemonChannel`] drops.
    pub async fn open_channel(&self) -> Result<DaemonChannel> {
        let (connection, slot) = self.acquire_slot().await?;

        let setup = async {
            let channel = exec_daemon(&connection.handle).await?;
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
                    "daemon session setup (exec + handshake) timed out after {}s",
                    SETUP_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| {
                format!(
                    "failed to open a daemon session on SSH connection {}",
                    connection.index
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
            _slot: slot,
        })
    }

    /// Acquire one channel slot, preferring the connection with the most free
    /// slots; when everything is busy, wait for the first slot to free on any
    /// connection.
    async fn acquire_slot(&self) -> Result<(&PoolConnection, OwnedSemaphorePermit)> {
        // Fast path: most-free connection with an immediately available slot.
        let mut by_free: Vec<&PoolConnection> = self.connections.iter().collect();
        by_free.sort_by_key(|connection| std::cmp::Reverse(connection.slots.available_permits()));
        for connection in by_free {
            if let Ok(permit) = connection.slots.clone().try_acquire_owned() {
                return Ok((connection, permit));
            }
        }

        // All slots busy: wait on every connection's semaphore and take
        // whichever frees first (each queue is FIFO, so waiters cannot
        // starve; losing acquire futures are dropped without consuming a
        // permit).
        let waiters: Vec<_> = self
            .connections
            .iter()
            .map(|connection| Box::pin(connection.slots.clone().acquire_owned()))
            .collect();
        let (permit, index, _rest) = futures_util::future::select_all(waiters).await;
        let permit = permit.context("gateway pool slot semaphore closed")?;
        let connection = self
            .connections
            .get(index)
            .context("gateway pool slot index out of range")?;
        Ok((connection, permit))
    }
}

/// Open one session channel on `handle`, exec the daemon command, and wait
/// for the gateway to confirm it. `exec` only sends the request; the
/// accept/reject verdict arrives as a channel message (the gateway re-checks
/// its 4-channel budget at exec time).
async fn exec_daemon(
    handle: &russh::client::Handle<ReplayHandler>,
) -> Result<russh::Channel<russh::client::Msg>> {
    let mut channel = handle.channel_open_session().await.context(
        "failed to open an SSH session channel (the gateway allows at most 4 per connection)",
    )?;
    channel
        .exec(true, DAEMON_COMMAND)
        .await
        .context("failed to send the nix-daemon exec request")?;
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
    /// Slot on the owning connection; released on drop.
    _slot: OwnedSemaphorePermit,
}

impl DaemonChannel {
    /// Worker-protocol version negotiated during the handshake.
    pub fn negotiated_version(&self) -> u64 {
        self.negotiated_version
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
