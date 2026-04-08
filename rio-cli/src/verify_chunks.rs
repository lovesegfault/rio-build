//! `rio-cli verify-chunks` — PG↔backend chunk consistency audit.
//!
//! Calls `StoreAdminService.VerifyChunks` (server-streaming). Progress
//! goes to stderr (operator watches it scroll); missing chunk hashes
//! go to stdout (one hex-encoded BLAKE3 per line — pipeable into
//! `xargs aws s3api head-object` or whatever).
//!
//! I-040 diagnostic: surfaces chunks where PG says exists (refcount>0,
//! deleted=false) but the backend's HeadObject 404s. The I-007
//! prefix-normalize fix stranded 3465 objects this way.

use anyhow::anyhow;
use rio_proto::StoreAdminServiceClient;
use rio_proto::types::VerifyChunksRequest;
use tonic::transport::Channel;

use crate::RPC_TIMEOUT;

pub(crate) async fn run(
    client: &mut StoreAdminServiceClient<Channel>,
    batch_size: u32,
) -> anyhow::Result<()> {
    let mut stream = rio_common::grpc::with_timeout(
        "VerifyChunks",
        RPC_TIMEOUT,
        client.verify_chunks(VerifyChunksRequest { batch_size }),
    )
    .await?
    .into_inner();

    let mut saw_done = false;
    while let Some(p) = stream
        .message()
        .await
        .map_err(|s| anyhow!("VerifyChunks: stream: {} ({:?})", s.message(), s.code()))?
    {
        // Missing hashes to stdout (per-batch, no buffering — large
        // stores can have a long tail). Hex-encoded BLAKE3, one per
        // line. The operator pipes this into S3 spot-checks or a
        // recovery script.
        for h in &p.missing_hashes {
            println!("{}", hex::encode(h));
        }
        // Progress to stderr. Separate stream so `verify-chunks |
        // tee missing.txt` captures stdout while progress scrolls
        // past on stderr.
        if p.done {
            eprintln!(
                "verify-chunks: done — scanned {}, missing {}",
                p.scanned, p.missing
            );
            saw_done = true;
        } else {
            eprintln!("  scanned={} missing={}", p.scanned, p.missing);
        }
    }
    if !saw_done {
        eprintln!(
            "warning: verify-chunks stream closed without done — \
             store disconnected mid-scan"
        );
    }
    Ok(())
}
