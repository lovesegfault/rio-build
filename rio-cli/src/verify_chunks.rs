//! `rio-cli verify-chunks` — PG↔backend chunk consistency audit.
//!
//! Calls `StoreAdminService.VerifyChunks` (server-streaming). Progress
//! goes to stderr (operator watches it scroll); missing chunk hashes
//! go to stdout (one hex-encoded BLAKE3 per line — pipeable into
//! `xargs aws s3api head-object` or whatever).
//!
//! I-040 diagnostic: surfaces chunks where PG says exists
//! (`uploaded_at` set, not deleted) but the backend's HeadObject 404s.
//! The I-007 prefix-normalize fix stranded 3465 objects this way.

use rio_proto::types::VerifyChunksRequest;

use crate::{RPC_TIMEOUT, StoreAdminClient};

#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// Chunks per backend exists_batch. 0 = server default (1000).
    #[arg(long, default_value_t = 0)]
    batch_size: u32,
}

// r[impl cli.cmd.verify-chunks]
pub(crate) async fn run(client: &mut StoreAdminClient, a: Args) -> anyhow::Result<()> {
    let mut stream = rio_common::grpc::with_timeout(
        "VerifyChunks",
        RPC_TIMEOUT,
        client.verify_chunks(VerifyChunksRequest {
            batch_size: a.batch_size,
        }),
    )
    .await?
    .into_inner();

    // The drain law (bug_141): an audit stream that closes without the
    // `done` sentinel was TRUNCATED — that must be a nonzero exit, not
    // a warning. A partial missing-hash list that exits 0 reads as "the
    // rest of the store is clean", and the operator acts on absence.
    crate::stream_util::drain_until_done(
        "VerifyChunks",
        &mut stream,
        |p| {
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
            } else {
                eprintln!("  scanned={} missing={}", p.scanned, p.missing);
            }
            Ok(())
        },
        |p| p.done,
    )
    .await
}
