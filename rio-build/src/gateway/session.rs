//! Per-SSH-channel protocol session state machine.

use std::collections::HashSet;

use rio_nix::protocol::handshake;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use super::handler::{self, ClientOptions};
use crate::store::Store;

/// Runs the Nix worker protocol on separate read/write streams.
#[tracing::instrument(name = "session", skip_all)]
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

    // Step 1: Handshake — needs both reader and writer interleaved
    let version_string = format!("rio-build {}", env!("CARGO_PKG_VERSION"));
    match handshake::server_handshake_split(reader, writer, &version_string).await {
        Ok(result) => {
            let (major, minor) = handshake::decode_version(result.negotiated_version);
            metrics::counter!("rio_gateway_handshakes_total", "result" => "success").increment(1);
            info!(
                client_version_major = major,
                client_version_minor = minor,
                "handshake complete"
            );
        }
        Err(handshake::HandshakeError::VersionTooOld {
            client_major,
            client_minor,
        }) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "rejected").increment(1);
            warn!(
                client_version_major = client_major,
                client_version_minor = client_minor,
                "rejecting client: protocol version too old"
            );
            let mut stderr = StderrWriter::new(&mut *writer);
            stderr
                .error(&StderrError::simple(
                    "rio-build",
                    format!("rio-build requires Nix protocol version 1.37+, client sent {client_major}.{client_minor}"),
                ))
                .await?;
            return Ok(());
        }
        Err(e) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "failed").increment(1);
            warn!(error = %e, "handshake failed");
            return Ok(());
        }
    }

    // Step 2: Opcode loop
    // Note: wopSetOptions is conventionally the first opcode but not enforced.
    // Real nix-daemon accepts any opcode after handshake.
    loop {
        let opcode = match wire::read_u64(reader).await {
            Ok(op) => op,
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("client disconnected (EOF)");
                return Ok(());
            }
            Err(e) => {
                metrics::counter!("rio_gateway_errors_total", "type" => "wire_read").increment(1);
                error!(error = %e, "error reading opcode");
                return Err(e.into());
            }
        };

        debug!(opcode = opcode, "received opcode");

        handler::handle_opcode(opcode, reader, writer, store, &mut options, &mut temp_roots)
            .await?;

        writer.flush().await?;
    }
}
