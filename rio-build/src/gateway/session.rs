//! Per-SSH-channel protocol session state machine.
//!
//! Each SSH channel gets its own protocol session that tracks the
//! connection lifecycle: Handshake -> Options -> Ready.

use std::collections::HashSet;

use rio_nix::protocol::handshake;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use super::handler::{self, ClientOptions};
use crate::store::Store;

/// Protocol session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Handshake done; must receive wopSetOptions next.
    AwaitingOptions,
    /// Ready to accept any implemented opcode.
    Ready,
}

/// Runs the Nix worker protocol on a bidirectional stream.
///
/// The stream must implement both `AsyncRead` and `AsyncWrite` (e.g.,
/// a `ChannelStream` from russh, or a `tokio::io::Join` of separate halves).
pub async fn run_protocol<S>(stream: &mut S, store: &dyn Store) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut options: Option<ClientOptions> = None;
    let mut temp_roots: HashSet<String> = HashSet::new();

    // Step 1: Handshake
    let version_string = format!("rio-build {}", env!("CARGO_PKG_VERSION"));
    match handshake::server_handshake(stream, &version_string).await {
        Ok(result) => {
            let (major, minor) = handshake::decode_version(result.client_version);
            info!(
                client_version = format!("{major}.{minor}"),
                "handshake complete"
            );
        }
        Err(handshake::HandshakeError::VersionTooOld {
            client_major,
            client_minor,
        }) => {
            warn!(
                client_version = format!("{client_major}.{client_minor}"),
                "rejecting client: protocol version too old"
            );
            let mut stderr = StderrWriter::new(&mut *stream);
            stderr
                .error(&StderrError::simple(format!(
                    "rio-build requires Nix protocol version 1.37+, client sent {client_major}.{client_minor}"
                )))
                .await?;
            return Ok(());
        }
        Err(e) => {
            warn!(error = %e, "handshake failed");
            return Ok(());
        }
    }

    // Step 2: Opcode loop
    let mut state = State::AwaitingOptions;

    // Split the stream for the opcode loop: we need to read opcodes while
    // writing responses. The `tokio::io::split` adapter allows this safely.
    let (mut reader, mut writer) = tokio::io::split(stream);

    loop {
        let opcode = match wire::read_u64(&mut reader).await {
            Ok(op) => op,
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("client disconnected (EOF)");
                return Ok(());
            }
            Err(e) => {
                error!(error = %e, "error reading opcode");
                return Err(e.into());
            }
        };

        debug!(opcode = opcode, state = ?state, "received opcode");

        match state {
            State::AwaitingOptions => {
                if opcode != 19 {
                    warn!(
                        opcode = opcode,
                        "protocol error: wopSetOptions must be the first opcode after handshake"
                    );
                    let mut stderr = StderrWriter::new(&mut writer);
                    stderr
                        .error(&StderrError::simple(
                            "protocol error: wopSetOptions must be the first opcode after handshake",
                        ))
                        .await?;
                    continue;
                }

                handler::handle_opcode(
                    opcode,
                    &mut reader,
                    &mut writer,
                    store,
                    &mut options,
                    &mut temp_roots,
                )
                .await?;
                state = State::Ready;
            }
            State::Ready => {
                handler::handle_opcode(
                    opcode,
                    &mut reader,
                    &mut writer,
                    store,
                    &mut options,
                    &mut temp_roots,
                )
                .await?;
            }
        }

        writer.flush().await?;
    }
}
