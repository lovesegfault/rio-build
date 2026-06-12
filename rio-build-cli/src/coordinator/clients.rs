//! Cluster connections for the coordinator: one channel to the
//! scheduler (zstd-compressed both ways — the SubmitBuild skeleton
//! compresses ~4×, ADR-024), one to the store's external castore door
//! (negotiation + upload + fetch; the store does NOT advertise gRPC
//! zstd, so those calls go uncompressed — at-rest compression is the
//! store's job).

use rio_proto::{
    ChunkServiceClient, DirectoryServiceClient, DrvBlobServiceClient, SchedulerServiceClient,
    StoreServiceClient,
};
use tonic::transport::Channel;

/// Typed clients + the tenant identity attached to every RPC.
#[derive(Clone)]
pub struct Clients {
    pub scheduler: SchedulerServiceClient<Channel>,
    pub store: StoreServiceClient<Channel>,
    pub chunks: ChunkServiceClient<Channel>,
    pub directories: DirectoryServiceClient<Channel>,
    pub drv_blobs: DrvBlobServiceClient<Channel>,
    /// Tenant JWT for `x-rio-tenant-token`. `None` = anonymous
    /// (dev/single-tenant clusters only).
    token: Option<String>,
}

impl Clients {
    /// Connect both channels eagerly. `token` is the raw JWT string.
    pub async fn connect(
        scheduler_addr: &str,
        store_addr: &str,
        token: Option<String>,
    ) -> anyhow::Result<Self> {
        use rio_proto::client::{ProtoClient, connect_single};
        let scheduler: SchedulerServiceClient<Channel> = connect_single(scheduler_addr).await?;
        let scheduler = scheduler
            .send_compressed(tonic::codec::CompressionEncoding::Zstd)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        // One store channel, four typed views.
        let store_ch = rio_proto::client::connect_raw::<StoreServiceClient<Channel>>(
            &rio_common::config::UpstreamAddrs {
                addr: store_addr.to_string(),
                balance_host: None,
                balance_port: 0,
            },
        )
        .await?
        .0;
        Ok(Self::from_parts(
            scheduler,
            StoreServiceClient::wrap(store_ch.clone()),
            ChunkServiceClient::wrap(store_ch.clone()),
            DirectoryServiceClient::wrap(store_ch.clone()),
            DrvBlobServiceClient::wrap(store_ch),
            token,
        ))
    }

    /// Assemble from already-connected clients (the in-process test
    /// path: tests spawn real store services + a stub scheduler and
    /// hand the clients in directly).
    pub fn from_parts(
        scheduler: SchedulerServiceClient<Channel>,
        store: StoreServiceClient<Channel>,
        chunks: ChunkServiceClient<Channel>,
        directories: DirectoryServiceClient<Channel>,
        drv_blobs: DrvBlobServiceClient<Channel>,
        token: Option<String>,
    ) -> Self {
        Self {
            scheduler,
            store,
            chunks,
            directories,
            drv_blobs,
            token,
        }
    }

    /// Wrap a message in a request carrying the tenant token (the
    /// single JWT-injection point — every coordinator RPC goes through
    /// here so an unauthenticated call site can't slip in).
    pub fn req<T>(&self, msg: T) -> anyhow::Result<tonic::Request<T>> {
        let mut r = tonic::Request::new(msg);
        if let Some(t) = &self.token {
            r.metadata_mut().insert(
                rio_proto::TENANT_TOKEN_HEADER,
                t.parse()
                    .map_err(|e| anyhow::anyhow!("tenant token is not valid header ASCII: {e}"))?,
            );
        }
        Ok(r)
    }
}

/// Decode a `HasBitmap` bit: LSB-first within each byte, bit `i` set ⇔
/// `digests[i]` present and tenant-visible.
pub fn bitmap_bit(bitmap: &[u8], i: usize) -> bool {
    bitmap.get(i / 8).is_some_and(|b| b & (1u8 << (i % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_is_lsb_first() {
        // 0b01: bit 0 set, bit 1 clear (the external_door fixture).
        assert!(bitmap_bit(&[0b01], 0));
        assert!(!bitmap_bit(&[0b01], 1));
        // Bit 9 lives in byte 1, position 1.
        assert!(bitmap_bit(&[0, 0b10], 9));
        // Out of range = absent.
        assert!(!bitmap_bit(&[0xFF], 8));
    }
}
