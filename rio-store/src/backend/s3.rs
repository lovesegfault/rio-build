//! S3 NAR storage backend.
//!
//! Stores NARs as `{prefix}/{sha256-hex}.nar` in an S3 bucket using the
//! `aws-sdk-s3` client.

use aws_sdk_s3::Client;
use bytes::Bytes;
use tokio::io::AsyncRead;
use tracing::debug;

use super::NarBackend;

/// S3-based NAR blob storage backend.
///
/// Uses the AWS SDK for S3 operations. The client should be configured
/// externally (region, credentials, endpoint) and passed in.
pub struct S3Backend {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3Backend {
    /// Create a new S3 backend.
    ///
    /// # Arguments
    /// - `client`: Pre-configured AWS S3 client.
    /// - `bucket`: S3 bucket name.
    /// - `prefix`: Key prefix for NAR blobs (e.g., "nars").
    pub fn new(client: Client, bucket: String, prefix: String) -> Self {
        Self {
            client,
            bucket,
            prefix,
        }
    }

    /// Compute the full S3 key for a given storage key.
    fn s3_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }
}

#[async_trait::async_trait]
impl NarBackend for S3Backend {
    async fn put(&self, sha256_hex: &str, data: Bytes) -> anyhow::Result<String> {
        let key = format!("{sha256_hex}.nar");
        let s3_key = self.s3_key(&key);
        debug!(
            bucket = %self.bucket,
            s3_key = %s3_key,
            size = data.len(),
            "S3Backend: uploading NAR blob"
        );
        metrics::counter!("rio_store_s3_requests_total", "operation" => "put_object").increment(1);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(data.into())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 PutObject failed for {s3_key}: {e}"))?;

        Ok(key)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Box<dyn AsyncRead + Send + Unpin>>> {
        let s3_key = self.s3_key(key);
        debug!(
            bucket = %self.bucket,
            s3_key = %s3_key,
            "S3Backend: downloading NAR blob"
        );
        metrics::counter!("rio_store_s3_requests_total", "operation" => "get_object").increment(1);

        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(output) => {
                let body = output.body.into_async_read();
                Ok(Some(Box::new(body) as Box<dyn AsyncRead + Send + Unpin>))
            }
            Err(err) => {
                // Check if this is a NoSuchKey error
                let service_err = err.into_service_error();
                if service_err.is_no_such_key() {
                    Ok(None)
                } else {
                    Err(anyhow::anyhow!(
                        "S3 GetObject failed for {s3_key}: {service_err}"
                    ))
                }
            }
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let s3_key = self.s3_key(key);
        debug!(
            bucket = %self.bucket,
            s3_key = %s3_key,
            "S3Backend: deleting NAR blob"
        );
        metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_object")
            .increment(1);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 DeleteObject failed for {s3_key}: {e}"))?;

        Ok(())
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let s3_key = self.s3_key(key);
        metrics::counter!("rio_store_s3_requests_total", "operation" => "head_object").increment(1);
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                let service_err = err.into_service_error();
                if service_err.is_not_found() {
                    Ok(false)
                } else {
                    Err(anyhow::anyhow!(
                        "S3 HeadObject failed for {s3_key}: {service_err}"
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::operation::head_object::HeadObjectError;
    use aws_sdk_s3::types::error::{NoSuchKey, NotFound};
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    /// Map Result<Option<Box<dyn AsyncRead>>, E> to something Debug-printable
    /// for assertion messages (dyn AsyncRead itself has no Debug impl).
    fn describe_get(r: &anyhow::Result<Option<Box<dyn AsyncRead + Send + Unpin>>>) -> String {
        match r {
            Ok(Some(_)) => "Ok(Some(<stream>))".into(),
            Ok(None) => "Ok(None)".into(),
            Err(e) => format!("Err({e})"),
        }
    }

    fn make_backend(client: Client) -> S3Backend {
        S3Backend::new(client, "test-bucket".into(), "prefix".into())
    }

    /// GetObject → NoSuchKey must yield Ok(None), not Err.
    /// This is the "path not in store" happy-negative path.
    #[tokio::test]
    async fn test_get_nosuchkey_returns_none() {
        let rule = mock!(Client::get_object)
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_backend(client);

        let result = backend.get("some-key.nar").await;
        assert!(
            matches!(result, Ok(None)),
            "NoSuchKey should map to Ok(None), got {}",
            describe_get(&result)
        );
    }

    /// GetObject → transient server error must yield Err, NOT Ok(None).
    /// Conflating these makes every store outage look like a cache miss —
    /// callers would silently re-fetch instead of surfacing EIO.
    #[tokio::test]
    async fn test_get_server_error_returns_err() {
        let rule = mock!(Client::get_object).then_error(|| {
            GetObjectError::generic(
                ErrorMetadata::builder()
                    .code("InternalError")
                    .message("We encountered an internal error")
                    .build(),
            )
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_backend(client);

        let result = backend.get("some-key.nar").await;
        assert!(
            result.is_err(),
            "transient error should propagate as Err, got {}",
            describe_get(&result)
        );
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => unreachable!("asserted is_err above"),
        };
        assert!(
            err.contains("GetObject failed"),
            "error should mention GetObject, got: {err}"
        );
    }

    /// HeadObject → NotFound must yield Ok(false).
    #[tokio::test]
    async fn test_exists_notfound_returns_false() {
        let rule = mock!(Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_backend(client);

        let result = backend.exists("some-key.nar").await;
        assert!(
            matches!(result, Ok(false)),
            "NotFound should map to Ok(false), got {result:?}"
        );
    }

    /// HeadObject → transient server error must yield Err, NOT Ok(false).
    /// A bug here would make the put_path idempotency check (grpc.rs) silently
    /// re-upload every NAR during a store outage.
    #[tokio::test]
    async fn test_exists_server_error_returns_err() {
        let rule = mock!(Client::head_object).then_error(|| {
            HeadObjectError::generic(
                ErrorMetadata::builder()
                    .code("ServiceUnavailable")
                    .message("Please reduce your request rate")
                    .build(),
            )
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_backend(client);

        let result = backend.exists("some-key.nar").await;
        assert!(
            result.is_err(),
            "transient error should propagate as Err, got {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("HeadObject failed"),
            "error should mention HeadObject, got: {err}"
        );
    }

    /// s3_key prefix joining: empty prefix → no slash, non-empty → slash-joined.
    #[test]
    fn test_s3_key_prefix_handling() {
        // Can't construct a real Client in a sync #[test], so build S3Backend
        // with a dummy config — s3_key() doesn't touch the client.
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        let client = Client::from_conf(cfg);

        let with_prefix = S3Backend::new(client.clone(), "b".into(), "nars".into());
        assert_eq!(with_prefix.s3_key("abc.nar"), "nars/abc.nar");

        let no_prefix = S3Backend::new(client, "b".into(), "".into());
        assert_eq!(no_prefix.s3_key("abc.nar"), "abc.nar");
    }
}
