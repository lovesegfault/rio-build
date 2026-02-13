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

/// Runs the Nix worker protocol on split read/write streams.
///
/// This is the main protocol loop for a single SSH channel. It handles
/// handshake, option negotiation, and opcode dispatch.
pub async fn run_protocol<R, W>(
    reader: &mut R,
    writer: &mut W,
    store: &dyn Store,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut options: Option<ClientOptions> = None;
    let mut temp_roots: HashSet<String> = HashSet::new();

    // Step 1: Handshake (reads from reader, writes to writer)
    let version_string = format!("rio-build {}", env!("CARGO_PKG_VERSION"));

    // Handshake needs a combined read+write stream. We use a join wrapper.
    let mut combined = tokio::io::join(reader, writer);
    match handshake::server_handshake(&mut combined, &version_string).await {
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
            let (_, w) = combined.into_inner();
            let mut stderr = StderrWriter::new(w);
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
    let (reader, writer) = combined.into_inner();

    // Step 2: Opcode loop
    let mut state = State::AwaitingOptions;

    loop {
        // Read the next opcode
        let opcode = match wire::read_u64(reader).await {
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
                    // wopSetOptions
                    warn!(
                        opcode = opcode,
                        "protocol error: wopSetOptions must be the first opcode after handshake"
                    );
                    let mut stderr = StderrWriter::new(&mut *writer);
                    stderr
                        .error(&StderrError::simple(
                            "protocol error: wopSetOptions must be the first opcode after handshake",
                        ))
                        .await?;
                    continue;
                }

                handler::handle_opcode(
                    opcode,
                    reader,
                    writer,
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
                    reader,
                    writer,
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
