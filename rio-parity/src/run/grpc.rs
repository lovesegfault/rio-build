//! Thin trait facades over the rio gRPC surfaces the engine reads, so every
//! stage is unit-testable with in-memory fakes. The tonic-backed impls are
//! deliberately dumb adapters (no logic beyond chunking) — they are exercised
//! against a live cluster during the first smoke campaign, not in unit tests.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

/// Validity/nar-hash lookups against rio-store. Local-only semantics:
/// `BatchQueryPathInfo` reports what is already valid in rio-store and never
/// triggers substitution.
#[async_trait]
pub trait StoreApi: Send + Sync {
    /// For every requested path: Some((nar_hash_hex, nar_size)) when the path
    /// is valid in rio-store, None otherwise. Order-insensitive.
    async fn query_valid(&self, paths: &[String])
    -> Result<HashMap<String, Option<(String, u64)>>>;
}

/// Chunk size for BatchQueryPathInfo calls (store-side `= ANY(...)` query;
/// 500 keeps requests well under message-size limits).
pub const BATCH_QUERY_CHUNK: usize = 500;

/// [`StoreApi`] backed by the rio-store StoreService gRPC endpoint.
pub struct GrpcStoreApi {
    addr: String,
    timeout: Duration,
}

impl GrpcStoreApi {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            timeout: Duration::from_secs(60),
        }
    }
}

#[async_trait]
impl StoreApi for GrpcStoreApi {
    async fn query_valid(
        &self,
        paths: &[String],
    ) -> Result<HashMap<String, Option<(String, u64)>>> {
        let channel = rio_proto::client::connect_channel(&self.addr)
            .await
            .with_context(|| format!("connect rio-store at {}", self.addr))?;
        let mut client = rio_proto::StoreServiceClient::new(channel)
            .max_decoding_message_size(rio_common::grpc::max_message_size())
            .max_encoding_message_size(rio_common::grpc::max_message_size());
        let mut out = HashMap::with_capacity(paths.len());
        for chunk in paths.chunks(BATCH_QUERY_CHUNK) {
            let entries = rio_proto::client::batch_query_path_info(
                &mut client,
                chunk.to_vec(),
                self.timeout,
                &[],
            )
            .await
            .map_err(|s| anyhow::anyhow!("BatchQueryPathInfo: {s}"))?;
            for (path, info) in entries {
                out.insert(path, info.map(|i| (hex::encode(i.nar_hash), i.nar_size)));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// In-memory StoreApi: paths present in the map are valid.
    #[derive(Default)]
    pub struct FakeStoreApi {
        pub valid: HashMap<String, (String, u64)>,
        pub calls: Mutex<usize>,
    }

    #[async_trait]
    impl StoreApi for FakeStoreApi {
        async fn query_valid(
            &self,
            paths: &[String],
        ) -> Result<HashMap<String, Option<(String, u64)>>> {
            *self.calls.lock().unwrap() += 1;
            Ok(paths
                .iter()
                .map(|p| (p.clone(), self.valid.get(p).cloned()))
                .collect())
        }
    }
}
