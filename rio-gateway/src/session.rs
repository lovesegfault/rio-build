//! Per-SSH-channel protocol session state machine.
//!
//! Each SSH channel runs an independent protocol session with its own
//! handshake, option negotiation, derivation cache, and build tracking.

use rio_nix::protocol::handshake;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::handler::{self, SessionContext};

// r[impl gw.conn.per-channel-state]
// r[impl gw.conn.sequential]
// r[impl gw.conn.lifecycle]
// r[impl gw.handshake.phases]
// r[impl gw.compat.version-range]
/// Runs the Nix worker protocol on separate read/write streams,
/// delegating store operations to `StoreServiceClient` and build
/// operations to `SchedulerServiceClient`.
#[tracing::instrument(name = "session", skip_all)]
pub async fn run_protocol<R, W>(
    reader: &mut R,
    writer: &mut W,
    store_client: &mut StoreServiceClient<Channel>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin,
{
    let mut ctx = SessionContext::new(store_client.clone(), scheduler_client.clone());

    let version_string = format!("rio-gateway {}", env!("CARGO_PKG_VERSION"));
    match handshake::server_handshake_split(reader, writer, &version_string).await {
        Ok(result) => {
            let (major, minor) = handshake::decode_version(result.negotiated_version());
            metrics::counter!("rio_gateway_handshakes_total", "result" => "success").increment(1);
            info!(
                client_version_major = major,
                client_version_minor = minor,
                "handshake complete"
            );
        }
        // r[impl gw.handshake.version-negotiation]
        // handshake.rs returns VersionTooOld for client < 1.37; the session
        // layer is responsible for converting that into STDERR_ERROR on the
        // wire before closing. The annotation in handshake.rs covers the
        // version check; this one covers the STDERR_ERROR send.
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
                    "rio-gateway",
                    format!("rio-gateway requires Nix protocol version 1.37+, client sent {client_major}.{client_minor}"),
                ))
                .await?;
            return Ok(());
        }
        Err(e) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "failure").increment(1);
            warn!(error = %e, "handshake failed");
            return Err(anyhow::anyhow!("handshake failed: {e}"));
        }
    }

    loop {
        let opcode = match wire::read_u64(reader).await {
            Ok(op) => op,
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("client disconnected (EOF)");

                for build_id in ctx.active_build_ids.keys() {
                    debug!(build_id = %build_id, "cancelling build on disconnect");
                    let req = types::CancelBuildRequest {
                        build_id: build_id.clone(),
                        reason: "client_disconnect".to_string(),
                    };
                    // Best-effort cancel: wrap in timeout so an unreachable
                    // scheduler doesn't block the disconnect cleanup loop.
                    match tokio::time::timeout(
                        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                        ctx.scheduler_client.cancel_build(req),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {
                            debug!(build_id = %build_id, "cancelled build on disconnect");
                        }
                        Ok(Err(e)) => {
                            warn!(build_id = %build_id, error = %e, "failed to cancel build on disconnect");
                        }
                        Err(_) => {
                            warn!(build_id = %build_id, "cancel_build timed out on disconnect");
                        }
                    }
                }

                return Ok(());
            }
            Err(e) => {
                metrics::counter!("rio_gateway_errors_total", "type" => "wire_read").increment(1);
                error!(error = %e, "error reading opcode");
                return Err(e.into());
            }
        };

        debug!(opcode = opcode, "received opcode");

        handler::handle_opcode(opcode, reader, writer, &mut ctx).await?;

        writer.flush().await?;
    }
}
