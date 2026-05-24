//! gRPC client bundle for FUSE fetches.

use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::store::log_service_client::LogServiceClient;

/// Wraps the rio-store service clients over a (typically p2c-balanced)
/// `tonic::transport::Channel`. Clone is cheap — the channel is
/// `Arc`-internal.
///
/// Kept as a struct (not a bare type alias) so future client additions
/// thread through every `prefetch_path_blocking` / `NixStoreFs` call
/// site as one parameter.
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
    /// `LogService` over the same channel. Cloned into each build's
    /// [`crate::log_upload::LogUploader`] for the `AppendLog` stream.
    pub log: LogServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap the store clients over a single `Channel` with the standard
    /// max-message-size headroom (matches `connect_single`'s convention).
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            log: LogServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}
