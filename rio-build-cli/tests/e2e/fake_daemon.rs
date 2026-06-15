//! In-process nix-daemon stand-in for the local-store import tests.
//!
//! Speaks just enough of the worker protocol server side for
//! `OutputFetcher`: the handshake, `wopQueryValidPaths` (answered from a
//! configurable valid set) and `wopAddToStoreNar` (recorded, or rejected
//! with a scripted STDERR_ERROR). Everything is in-memory — nothing is
//! written to any real store.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rio_nix::protocol::handshake::server_handshake_split;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::pathinfo::{ValidPathInfo, read_valid_path_info};
use rio_nix::protocol::stderr::{STDERR_LAST, StderrError, StderrWriter};
use rio_nix::protocol::wire;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;

/// One recorded `wopAddToStoreNar`.
pub struct ImportedPath {
    pub store_path: String,
    pub info: ValidPathInfo,
    pub nar: Vec<u8>,
}

#[derive(Default)]
pub struct FakeDaemonState {
    /// Paths reported as already valid by `wopQueryValidPaths`.
    pub valid: HashSet<String>,
    /// Recorded imports, in arrival order.
    pub imported: Vec<ImportedPath>,
    /// When set, every `wopAddToStoreNar` is rejected with this message
    /// (sent as STDERR_ERROR before the framed NAR is consumed — the
    /// fail-fast path a real daemon takes for signature-policy
    /// rejections).
    pub reject_with: Option<String>,
}

pub struct FakeDaemon {
    pub socket: PathBuf,
    pub state: Arc<Mutex<FakeDaemonState>>,
    _dir: tempfile::TempDir,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl FakeDaemon {
    pub async fn spawn() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket)?;
        let state = Arc::new(Mutex::new(FakeDaemonState::default()));

        let accept_state = Arc::clone(&state);
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _addr)) = listener.accept().await else {
                    return;
                };
                let conn_state = Arc::clone(&accept_state);
                tokio::spawn(async move {
                    // A dropped client connection is normal teardown, not
                    // a test failure; real protocol bugs surface through
                    // the client-side assertions.
                    let _ = handle_conn(stream, conn_state).await;
                });
            }
        });

        Ok(Self {
            socket,
            state,
            _dir: dir,
            accept_task,
        })
    }
}

async fn handle_conn(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<FakeDaemonState>>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let _negotiated =
        server_handshake_split(&mut reader, &mut writer, "rio-fake-daemon 0.0").await?;

    loop {
        let op = match wire::read_u64(&mut reader).await {
            Ok(op) => op,
            // Client hung up between opcodes — clean end of session.
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        match WorkerOp::from_u64(op) {
            Some(WorkerOp::QueryValidPaths) => {
                let asked = wire::read_strings(&mut reader).await?;
                let _substitute = wire::read_bool(&mut reader).await?;
                let valid: Vec<String> = {
                    let st = state.lock().expect("fake daemon state lock");
                    asked
                        .iter()
                        .filter(|p| st.valid.contains(*p))
                        .cloned()
                        .collect()
                };
                wire::write_u64(&mut writer, STDERR_LAST).await?;
                wire::write_strings(&mut writer, &valid).await?;
                writer.flush().await?;
            }
            Some(WorkerOp::AddToStoreNar) => {
                let store_path = wire::read_string(&mut reader).await?;
                let info = read_valid_path_info(&mut reader).await?;
                let _repair = wire::read_bool(&mut reader).await?;
                let _dont_check_sigs = wire::read_bool(&mut reader).await?;

                let reject = state
                    .lock()
                    .expect("fake daemon state lock")
                    .reject_with
                    .clone();
                if let Some(message) = reject {
                    // Reject before consuming the framed stream — the
                    // client aborts and abandons the connection.
                    StderrWriter::new(&mut writer)
                        .error(&StderrError::simple("nix-daemon", message.as_str()))
                        .await?;
                    writer.flush().await?;
                    return Ok(());
                }

                let nar = wire::read_framed_stream(&mut reader).await?;
                state
                    .lock()
                    .expect("fake daemon state lock")
                    .imported
                    .push(ImportedPath {
                        store_path,
                        info,
                        nar,
                    });
                wire::write_u64(&mut writer, STDERR_LAST).await?;
                writer.flush().await?;
            }
            other => anyhow::bail!("fake daemon: unexpected opcode {op} ({other:?})"),
        }
    }
}
