//! Default fetch path: import completed build outputs into the local
//! `/nix/store` by speaking the nix worker protocol to the local daemon
//! socket (ADR-024 "Attach, detach, results").
//!
//! Per output the importer walks the runtime closure through the store's
//! `QueryPathInfo`, prunes what the daemon already has via
//! `wopQueryValidPaths`, and imports the rest in topological order with
//! `wopAddToStoreNar` — each NAR streamed straight from `GetPath` into
//! the daemon's framed sink while being SHA-256-hashed, never buffered
//! whole. Signature checking stays ON (`dontCheckSigs = 0`): the daemon
//! applies its own `require-sigs` policy against the cluster signatures
//! riding the `PathInfo`.
//!
//! When no daemon is reachable, or the cluster store serves unsigned
//! paths (no signing key configured), the importer falls back to the
//! client-CAS materialization in [`crate::fetch`] with a single stderr
//! note naming the cause.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, anyhow, bail};
use sha2::Digest as _;
use tokio::io::{AsyncRead, BufReader, ReadBuf};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio_stream::Stream;
use tracing::{debug, info, instrument};

use rio_nix::protocol::client::{
    client_add_to_store_nar, client_handshake, client_query_valid_paths,
};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use rio_proto::types::{GetPathRequest, GetPathResponse, PathInfo, QueryPathInfoRequest};

use crate::coordinator::clients::Clients;

/// The local daemon socket: `NIX_DAEMON_SOCKET_PATH` when set (matches
/// nix's own override), otherwise the standard multi-user location.
pub fn daemon_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("NIX_DAEMON_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    PathBuf::from("/nix/var/nix/daemon-socket/socket")
}

/// Where one fetched output ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchedOutput {
    /// Imported into the local `/nix/store`; the path is the store path
    /// itself.
    Store(PathBuf),
    /// No usable daemon — materialized into the client CAS.
    Cas(PathBuf),
}

impl FetchedOutput {
    /// The local filesystem location an out-link should point at.
    pub fn local_path(&self) -> &Path {
        match self {
            FetchedOutput::Store(p) | FetchedOutput::Cas(p) => p,
        }
    }
}

/// One worker-protocol connection to the local nix daemon.
pub struct LocalDaemon {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl LocalDaemon {
    /// Connect and handshake. Errors cover "no daemon there" as well as
    /// protocol-level failures; the caller decides whether that means
    /// fallback (output fetch) or hard error.
    pub async fn connect(socket: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to nix daemon at {}", socket.display()))?;
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;
        // The negotiated version is not interesting beyond the handshake's
        // own floor check (≥ 1.35) — AddToStoreNar/QueryValidPaths need
        // nothing newer.
        let _negotiated = client_handshake(&mut reader, &mut writer)
            .await
            .map_err(|e| anyhow!("nix daemon handshake at {} failed: {e}", socket.display()))?;
        Ok(Self { reader, writer })
    }

    /// Which of `paths` the daemon already considers valid.
    async fn valid_paths(&mut self, paths: &[String]) -> anyhow::Result<HashSet<String>> {
        let valid = client_query_valid_paths(&mut self.reader, &mut self.writer, paths, false)
            .await
            .context("QueryValidPaths against the local nix daemon")?;
        Ok(valid.into_iter().collect())
    }

    /// Stream one path's NAR from the cluster store into the daemon.
    ///
    /// On error the worker-protocol connection is in an indeterminate
    /// state (frames may be half-written) — the caller MUST drop this
    /// `LocalDaemon` and not issue further opcodes on it.
    // r[impl bc.fetch.narhash-verify+2]
    async fn import_path(
        &mut self,
        clients: &mut Clients,
        store_path: &str,
        info: &PathInfo,
    ) -> anyhow::Result<()> {
        let mut stream = clients
            .store
            .get_path(clients.req(GetPathRequest {
                store_path: store_path.to_string(),
            })?)
            .await
            .with_context(|| format!("GetPath {store_path}"))?
            .into_inner();

        // First frame: the authoritative PathInfo (deriver, narHash,
        // references, narSize, signatures) — what the daemon gets as the
        // ValidPathInfo body.
        let frame = stream
            .message()
            .await
            .with_context(|| format!("GetPath {store_path} stream"))?
            .ok_or_else(|| anyhow!("GetPath {store_path}: empty stream"))?;
        let Some(rio_proto::types::get_path_response::Msg::Info(get_info)) = frame.msg else {
            bail!("GetPath {store_path}: stream did not start with PathInfo");
        };
        let valid_info = valid_path_info_from(&get_info);
        // The closure walk's QueryPathInfo answer and the GetPath header
        // come from the same narinfo row; a hash disagreement means the
        // store changed underneath us — refuse rather than import bytes
        // that no longer match the metadata we planned against.
        if !info.nar_hash.is_empty() && info.nar_hash != get_info.nar_hash {
            bail!(
                "GetPath {store_path}: narHash differs from the closure walk's QueryPathInfo \
                 answer — refusing to import"
            );
        }

        let mut nar = VerifiedNarReader::new(
            stream,
            get_info.nar_hash.clone(),
            get_info.nar_size,
            store_path.to_string(),
        );
        client_add_to_store_nar(
            &mut self.reader,
            &mut self.writer,
            store_path,
            &valid_info,
            &mut nar,
        )
        .await
        .map_err(|e| anyhow!("importing {store_path} into the local nix store: {e}"))?;
        debug!(store_path, "imported into local /nix/store");
        Ok(())
    }
}

/// Map a `PathInfo` (gRPC) to the worker-protocol `ValidPathInfo` body.
fn valid_path_info_from(info: &PathInfo) -> ValidPathInfo {
    let none_if_empty = |s: &str| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    ValidPathInfo {
        deriver: none_if_empty(&info.deriver),
        nar_hash: info.nar_hash.clone(),
        references: info.references.clone(),
        registration_time: info.registration_time,
        nar_size: info.nar_size,
        // Never claim local trust for cluster-built bytes; the daemon
        // must judge them by their signatures.
        ultimate: false,
        signatures: info.signatures.clone(),
        content_address: none_if_empty(&info.content_address),
    }
}

/// `AsyncRead` over the `nar_chunk` frames of a `GetPath` stream that
/// hashes every byte and refuses to report EOF unless the SHA-256 and
/// size match the server's claim. The error surfaces *before* the framed
/// terminator is written, so the daemon discards the partial import.
pub(crate) struct VerifiedNarReader {
    stream: tonic::Streaming<GetPathResponse>,
    pending: Vec<u8>,
    pos: usize,
    hasher: sha2::Sha256,
    received: u64,
    claimed_hash: Vec<u8>,
    claimed_size: u64,
    store_path: String,
    verified: bool,
}

impl VerifiedNarReader {
    pub(crate) fn new(
        stream: tonic::Streaming<GetPathResponse>,
        claimed_hash: Vec<u8>,
        claimed_size: u64,
        store_path: String,
    ) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            pos: 0,
            hasher: sha2::Sha256::new(),
            received: 0,
            claimed_hash,
            claimed_size,
            store_path,
            verified: false,
        }
    }
}

impl AsyncRead for VerifiedNarReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            // Drain buffered chunk bytes first.
            if this.pos < this.pending.len() {
                let n = (this.pending.len() - this.pos).min(buf.remaining());
                buf.put_slice(&this.pending[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            if this.verified {
                return Poll::Ready(Ok(())); // EOF
            }
            match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(status))) => {
                    return Poll::Ready(Err(std::io::Error::other(format!(
                        "GetPath {}: {}",
                        this.store_path,
                        status.message()
                    ))));
                }
                Poll::Ready(Some(Ok(resp))) => match resp.msg {
                    Some(rio_proto::types::get_path_response::Msg::NarChunk(chunk)) => {
                        this.hasher.update(&chunk);
                        this.received += chunk.len() as u64;
                        this.pending = chunk;
                        this.pos = 0;
                    }
                    // A duplicate Info frame or an empty message carries
                    // no NAR bytes; skip it.
                    Some(rio_proto::types::get_path_response::Msg::Info(_)) | None => {}
                },
                Poll::Ready(None) => {
                    let got = this.hasher.finalize_reset();
                    if got.as_slice() != this.claimed_hash.as_slice()
                        || this.received != this.claimed_size
                    {
                        return Poll::Ready(Err(std::io::Error::other(format!(
                            "narHash mismatch fetching {}: server claims {} ({} bytes), stream \
                             hashes to {} ({} bytes) — refusing to import",
                            this.store_path,
                            hex::encode(&this.claimed_hash),
                            this.claimed_size,
                            hex::encode(got),
                            this.received,
                        ))));
                    }
                    this.verified = true;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

/// Walk the runtime closure of `root` through the cluster store and
/// return it in topological order (dependencies before dependents).
// r[impl bc.fetch.closure-topo]
async fn walk_closure(
    clients: &mut Clients,
    root: &str,
) -> anyhow::Result<Vec<(String, PathInfo)>> {
    let mut infos: HashMap<String, PathInfo> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::from([root.to_string()]);
    while let Some(path) = queue.pop_front() {
        if infos.contains_key(&path) {
            continue;
        }
        let info = clients
            .store
            .query_path_info(clients.req(QueryPathInfoRequest {
                store_path: path.clone(),
            })?)
            .await
            .map_err(|status| {
                if status.code() == tonic::Code::NotFound {
                    anyhow!(
                        "{path} (referenced by {root}) is not visible in the cluster store for \
                         this tenant. Substituted dependencies are only visible to tenants that \
                         trust the upstream signing key (the tenant sig-visibility gate) — fix \
                         the tenant's trusted keys, or re-run with --no-fetch to leave outputs \
                         in the cluster store."
                    )
                } else {
                    anyhow!("QueryPathInfo {path}: {}", status.message())
                }
            })?
            .into_inner();
        for reference in &info.references {
            if reference != &path && !infos.contains_key(reference) {
                queue.push_back(reference.clone());
            }
        }
        infos.insert(path, info);
    }

    // Topological order via iterative DFS post-order over references.
    // Self-references are skipped above; reference graphs are acyclic
    // beyond that, but visited-tracking keeps a malformed graph from
    // looping forever.
    let mut order: Vec<(String, PathInfo)> = Vec::with_capacity(infos.len());
    let mut done: HashSet<String> = HashSet::new();
    let mut stack: Vec<(String, bool)> = vec![(root.to_string(), false)];
    let mut on_stack: HashSet<String> = HashSet::new();
    while let Some((path, expanded)) = stack.pop() {
        if expanded {
            on_stack.remove(&path);
            if done.insert(path.clone()) {
                let info = infos
                    .get(&path)
                    .expect("every stacked path was BFS-visited")
                    .clone();
                order.push((path, info));
            }
            continue;
        }
        if done.contains(&path) || !on_stack.insert(path.clone()) {
            continue;
        }
        stack.push((path.clone(), true));
        if let Some(info) = infos.get(&path) {
            for reference in &info.references {
                if reference != &path && !done.contains(reference) {
                    stack.push((reference.clone(), false));
                }
            }
        }
    }
    Ok(order)
}

/// Map a daemon import failure to actionable guidance when it is the
/// signature-policy rejection ("lacks a signature by a trusted key" in
/// nix 2.34; older wording "lacks a valid signature" kept as a secondary
/// match). Other errors pass through unchanged.
// r[impl bc.fetch.sig-reject-ux]
fn map_import_error(err: anyhow::Error, store_path: &str, info: &PathInfo) -> anyhow::Error {
    let msg = format!("{err:#}");
    if !(msg.contains("lacks a signature by a trusted key")
        || msg.contains("lacks a valid signature"))
    {
        return err;
    }
    let key_names: Vec<String> = info
        .signatures
        .iter()
        .filter_map(|s| s.split(':').next().map(str::to_string))
        .collect();
    let key_hint = match key_names.first() {
        Some(name) => format!(
            "The cluster signed it with key '{name}'; ask your rio operator for that key's \
             public half (the cluster's `rio/signing-key-pub` secret) and add it to nix.conf:\n\
             \x20 trusted-public-keys = <existing keys> {name}:<base64 public key>"
        ),
        // Should not happen — unsigned closures fall back to the CAS
        // before any import is attempted — but keep the message useful
        // if a partially signed closure slips through.
        None => "The path carries no cluster signature at all; the cluster store has no signing \
                 key configured (see the rio store's signing_key_path setting)."
            .to_string(),
    };
    anyhow!(
        "the local nix daemon refused to import {store_path}: it is not signed by any key in \
         this machine's `trusted-public-keys`.\n{key_hint}\n\
         Alternatively re-run with --no-fetch to leave outputs in the cluster store.\n\
         (daemon said: {msg})"
    )
}

/// The first to-import path that carries no signature at all, if any —
/// the marker that the cluster store has no signing key configured and a
/// `require-sigs` daemon would reject every import.
fn first_unsigned(missing: &[&(String, PathInfo)]) -> Option<String> {
    missing
        .iter()
        .find(|(_, info)| info.signatures.is_empty())
        .map(|(path, _)| path.clone())
}

enum DaemonState {
    /// Not yet probed (or dropped after an import error — the next fetch
    /// reconnects rather than reusing a possibly poisoned connection).
    Untried,
    Connected(Box<LocalDaemon>),
    /// Probed and unreachable — every output of this run uses the CAS.
    Unavailable,
}

/// Fetches completed outputs to the local machine: into `/nix/store` via
/// the daemon when one is reachable, into the client CAS otherwise.
pub struct OutputFetcher {
    socket: PathBuf,
    cas_root: PathBuf,
    daemon: DaemonState,
    note: Box<dyn Fn(String) + Send + Sync>,
    fallback_noted: bool,
}

impl OutputFetcher {
    /// `socket`: the daemon socket to probe (callers normally pass
    /// [`daemon_socket_path()`]). `note` receives the single
    /// human-readable fallback note (rendered on stderr).
    pub fn new(
        socket: PathBuf,
        cas_root: PathBuf,
        note: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self {
            socket,
            cas_root,
            daemon: DaemonState::Untried,
            note: Box::new(note),
            fallback_noted: false,
        }
    }

    fn note_fallback(&mut self, msg: String) {
        // r[impl bc.fetch.daemonless-fallback]
        if !self.fallback_noted {
            (self.note)(msg);
            self.fallback_noted = true;
        }
    }

    async fn daemon(&mut self) -> Option<&mut LocalDaemon> {
        if matches!(self.daemon, DaemonState::Untried) {
            self.daemon = match LocalDaemon::connect(&self.socket).await {
                Ok(d) => DaemonState::Connected(Box::new(d)),
                Err(e) => {
                    self.note_fallback(format!(
                        "no local nix daemon reachable ({e:#}); materializing outputs into the \
                         client CAS at {} instead of /nix/store",
                        self.cas_root.join("fetched").display()
                    ));
                    DaemonState::Unavailable
                }
            };
        }
        match &mut self.daemon {
            DaemonState::Connected(d) => Some(d),
            _ => None,
        }
    }

    /// Fetch one completed output. Default path: import its closure into
    /// the local `/nix/store`; fallback: materialize into the client CAS.
    // r[impl bc.fetch.store-import-default]
    #[instrument(skip(self, clients), fields(component = "build-client"))]
    pub async fn fetch(
        &mut self,
        clients: &mut Clients,
        store_path: &str,
    ) -> anyhow::Result<FetchedOutput> {
        if self.daemon().await.is_none() {
            let dest = crate::fetch::materialize(clients, &self.cas_root, store_path).await?;
            return Ok(FetchedOutput::Cas(dest));
        }

        let closure = walk_closure(clients, store_path).await?;
        let all_paths: Vec<String> = closure.iter().map(|(p, _)| p.clone()).collect();
        let daemon = self.daemon().await.expect("daemon checked reachable above");
        let valid = daemon.valid_paths(&all_paths).await?;
        let missing: Vec<&(String, PathInfo)> =
            closure.iter().filter(|(p, _)| !valid.contains(p)).collect();

        if let Some(unsigned) = first_unsigned(&missing) {
            // The cluster has no signing key configured for these paths;
            // a require-sigs daemon would reject them, so don't even try.
            self.note_fallback(format!(
                "cluster store serves {unsigned} without a signature (store signing key not \
                 configured); materializing outputs into the client CAS at {} instead of \
                 /nix/store",
                self.cas_root.join("fetched").display()
            ));
            self.daemon = DaemonState::Unavailable;
            let dest = crate::fetch::materialize(clients, &self.cas_root, store_path).await?;
            return Ok(FetchedOutput::Cas(dest));
        }

        let total = missing.len();
        for (path, info) in missing {
            let daemon = self
                .daemon()
                .await
                .expect("daemon connection established above");
            if let Err(e) = daemon.import_path(clients, path, info).await {
                // The connection may be mid-frame — never reuse it. The
                // next fetch (if any) reconnects from scratch.
                self.daemon = DaemonState::Untried;
                return Err(map_import_error(e, path, info));
            }
        }
        info!(
            store_path,
            imported = total,
            closure = all_paths.len(),
            "imported output closure into the local /nix/store"
        );
        Ok(FetchedOutput::Store(PathBuf::from(store_path)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_with_sigs(sigs: &[&str]) -> PathInfo {
        PathInfo {
            signatures: sigs.iter().map(|s| s.to_string()).collect(),
            ..PathInfo::default()
        }
    }

    /// Unsigned paths anywhere in the missing set trigger the CAS
    /// fallback decision; fully signed sets do not.
    #[test]
    fn first_unsigned_detects_missing_signatures() {
        let signed = ("/nix/store/aaa-x".to_string(), info_with_sigs(&["k1:abc"]));
        let unsigned = ("/nix/store/bbb-y".to_string(), info_with_sigs(&[]));
        assert_eq!(
            first_unsigned(&[&signed, &unsigned]),
            Some("/nix/store/bbb-y".to_string())
        );
        assert_eq!(first_unsigned(&[&signed]), None);
        assert_eq!(first_unsigned(&[]), None);
    }

    /// The signature rejection is rewritten into trusted-public-keys
    /// guidance naming the signing key and the --no-fetch escape hatch;
    /// unrelated errors pass through untouched.
    #[test]
    fn map_import_error_rewrites_sig_rejections_only() {
        let info = info_with_sigs(&["rio-prod-1:c2lnbmF0dXJl"]);
        let err =
            anyhow!("daemon error: cannot add path because it lacks a signature by a trusted key");
        let mapped = map_import_error(err, "/nix/store/aaa-x", &info);
        let msg = format!("{mapped:#}");
        assert!(msg.contains("trusted-public-keys"), "{msg}");
        assert!(msg.contains("rio-prod-1"), "{msg}");
        assert!(msg.contains("--no-fetch"), "{msg}");

        let other = anyhow!("connection reset by peer");
        let passed = map_import_error(other, "/nix/store/aaa-x", &info);
        assert_eq!(format!("{passed:#}"), "connection reset by peer");
    }

    /// `NIX_DAEMON_SOCKET_PATH` overrides the standard socket location
    /// (matches nix's own override; what the e2e fake daemon relies on).
    #[test]
    fn daemon_socket_path_honors_env_override() {
        // SAFETY: test-local env mutation; nextest gives each test its
        // own process, and no other test in this crate reads
        // NIX_DAEMON_SOCKET_PATH.
        unsafe { std::env::set_var("NIX_DAEMON_SOCKET_PATH", "/tmp/test-daemon.sock") };
        assert_eq!(daemon_socket_path(), PathBuf::from("/tmp/test-daemon.sock"));
        unsafe { std::env::remove_var("NIX_DAEMON_SOCKET_PATH") };
        assert_eq!(
            daemon_socket_path(),
            PathBuf::from("/nix/var/nix/daemon-socket/socket")
        );
    }
}
