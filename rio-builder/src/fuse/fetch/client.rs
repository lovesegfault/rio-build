//! gRPC client bundle for FUSE fetches.

use tonic::transport::Channel;

use rio_proto::StoreServiceClient;

/// Wraps `StoreServiceClient` over a (typically p2c-balanced)
/// `tonic::transport::Channel`. Clone is cheap — the channel is
/// `Arc`-internal.
///
/// Kept as a struct (not a bare type alias) so future client additions
/// thread through every `prefetch_path_blocking` / `NixStoreFs` call
/// site as one parameter.
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap the store client over a single `Channel` with the standard
    /// max-message-size headroom (matches `connect_single`'s convention).
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}
