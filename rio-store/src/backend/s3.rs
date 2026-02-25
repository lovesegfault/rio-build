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
