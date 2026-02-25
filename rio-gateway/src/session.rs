//! Per-SSH-channel protocol session state machine.
//!
//! Each SSH channel runs an independent protocol session with its own
//! handshake, option negotiation, derivation cache, and build tracking.

use std::collections::{HashMap, HashSet};

use rio_nix::derivation::Derivation;
use rio_nix::protocol::handshake;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::handler::{self, ClientOptions};

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
    let mut options: Option<ClientOptions> = None;
    let mut temp_roots: HashSet<StorePath> = HashSet::new();
    let mut drv_cache: HashMap<StorePath, Derivation> = HashMap::new();
    // IFD detection: tracks whether this session has seen wopBuildPathsWithResults.
    // If wopBuildDerivation arrives without a prior wopBuildPathsWithResults,
    // it's likely an IFD or build-hook request.
    let mut has_seen_build_paths_with_results: bool = false;
    // Active build IDs for scheduler failover reconnection.
    // Each entry is (build_id, last_sequence).
    let mut active_build_ids: Vec<(String, u64)> = Vec::new();

    // Step 1: Handshake
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
            metrics::counter!("rio_gateway_handshakes_total", "result" => "failed").increment(1);
            warn!(error = %e, "handshake failed");
            return Err(anyhow::anyhow!("handshake failed: {e}"));
        }
    }

    // Step 2: Opcode loop
    loop {
        let opcode = match wire::read_u64(reader).await {
            Ok(op) => op,
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("client disconnected (EOF)");

                // On disconnect, cancel all active builds
                for (build_id, _) in &active_build_ids {
                    debug!(build_id = %build_id, "cancelling build on disconnect");
                    let req = types::CancelBuildRequest {
                        build_id: build_id.clone(),
                        reason: "client_disconnect".to_string(),
                    };
                    if let Err(e) = scheduler_client.cancel_build(req).await {
                        warn!(
                            build_id = %build_id,
                            error = %e,
                            "failed to cancel build on disconnect"
                        );
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

        handler::handle_opcode(
            opcode,
            reader,
            writer,
            store_client,
            scheduler_client,
            &mut options,
            &mut temp_roots,
            &mut drv_cache,
            &mut has_seen_build_paths_with_results,
            &mut active_build_ids,
        )
        .await?;

        writer.flush().await?;
    }
}
