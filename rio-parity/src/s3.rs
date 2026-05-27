//! Write-once S3 layout writer for eval-set artifacts.
//!
//! Eval sets live under `<prefix>/evals/<hydra-eval-id>/<key-digest>/`
//! (e.g. `parity/evals/1824219/8b919129046e0f60/manifest.jsonl`), one
//! object per local artifact with the same filenames the local
//! directory layout uses. Eval sets are write-once: the upload refuses
//! to touch a prefix where `evalset.json` already exists; `--force`
//! salts the key digest (see [`crate::evalset::key::EvalSetKey`]) so a
//! forced rebuild lands in a new prefix instead of overwriting the old
//! one.

use anyhow::Context as _;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;

use crate::evalset::artifacts::{
    self, DEP_CLOSURE_FILE, DRVS_ARCHIVE_FILE, EVAL_ERRORS_FILE, EVALSET_FILE, FIDELITY_FILE,
    MANIFEST_FILE,
};

/// Upload order: data artifacts first, `evalset.json` last — its
/// presence is the completeness marker for the prefix.
pub const UPLOAD_ORDER: [&str; 6] = [
    MANIFEST_FILE,
    EVAL_ERRORS_FILE,
    FIDELITY_FILE,
    DEP_CLOSURE_FILE,
    DRVS_ARCHIVE_FILE,
    EVALSET_FILE,
];

/// S3 destination (bucket + key prefix) for eval-set uploads.
#[derive(Debug, Clone)]
pub struct EvalSetS3 {
    pub bucket: String,
    pub prefix: String,
}

impl EvalSetS3 {
    pub fn new(bucket: &str, prefix: &str) -> Self {
        Self {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        }
    }

    /// Object key for one artifact of one eval set.
    pub fn key(&self, hydra_eval_id: u64, key_digest: &str, file: &str) -> String {
        if self.prefix.is_empty() {
            format!("evals/{hydra_eval_id}/{key_digest}/{file}")
        } else {
            format!("{}/evals/{hydra_eval_id}/{key_digest}/{file}", self.prefix)
        }
    }

    /// Does `evalset.json` already exist at this prefix?
    pub async fn evalset_exists(
        &self,
        client: &aws_sdk_s3::Client,
        hydra_eval_id: u64,
        key_digest: &str,
    ) -> anyhow::Result<bool> {
        let key = self.key(hydra_eval_id, key_digest, EVALSET_FILE);
        match client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if head_is_not_found(&e) => Ok(false),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("HEAD s3://{}/{key}", self.bucket)))
            }
        }
    }

    /// Upload every artifact present in `dir`, in [`UPLOAD_ORDER`],
    /// refusing if the prefix is already complete. Artifacts missing
    /// locally are skipped with a warning. Returns the uploaded keys in
    /// order.
    pub async fn upload_eval_set(
        &self,
        client: &aws_sdk_s3::Client,
        dir: &artifacts::EvalSetDir,
        hydra_eval_id: u64,
        key_digest: &str,
    ) -> anyhow::Result<Vec<String>> {
        if self
            .evalset_exists(client, hydra_eval_id, key_digest)
            .await?
        {
            anyhow::bail!(
                "eval set s3://{}/{} already exists and eval sets are write-once; \
                 use --force to build under a new key digest",
                self.bucket,
                self.key(hydra_eval_id, key_digest, EVALSET_FILE)
            );
        }
        let mut uploaded = Vec::new();
        for file in UPLOAD_ORDER {
            let local = dir.path(file);
            if !local.exists() {
                tracing::warn!(file, "artifact missing locally; skipping upload");
                continue;
            }
            let key = self.key(hydra_eval_id, key_digest, file);
            let body = ByteStream::from_path(&local)
                .await
                .with_context(|| format!("read {}", local.display()))?;
            client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(body)
                .send()
                .await
                .with_context(|| format!("PUT s3://{}/{key}", self.bucket))?;
            tracing::info!(key = %key, "uploaded");
            uploaded.push(key);
        }
        Ok(uploaded)
    }
}

/// Generic over the response type so we don't have to name
/// aws-smithy-runtime-api types (not a direct dependency).
fn head_is_not_found<R>(
    err: &SdkError<aws_sdk_s3::operation::head_object::HeadObjectError, R>,
) -> bool {
    match err {
        SdkError::ServiceError(se) => se.err().is_not_found(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::head_object::{HeadObjectError, HeadObjectOutput};
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::types::error::NotFound;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    #[test]
    fn keys_follow_the_design_layout() {
        let layout = EvalSetS3::new("rio-chunks", "parity");
        assert_eq!(
            layout.key(1824219, "abcd1234abcd1234", "manifest.jsonl"),
            "parity/evals/1824219/abcd1234abcd1234/manifest.jsonl"
        );
        // Prefix slashes are normalized.
        let layout = EvalSetS3::new("rio-chunks", "parity/");
        assert_eq!(
            layout.key(1, "d", "evalset.json"),
            "parity/evals/1/d/evalset.json"
        );
        // An empty prefix roots the eval-set tree at the bucket root
        // without a leading slash.
        let layout = EvalSetS3::new("rio-chunks", "");
        assert_eq!(layout.key(1, "d", "evalset.json"), "evals/1/d/evalset.json");
    }

    fn local_set(tmp: &tempfile::TempDir) -> crate::evalset::artifacts::EvalSetDir {
        let dir = crate::evalset::artifacts::EvalSetDir::create(tmp.path()).unwrap();
        for f in [
            "manifest.jsonl",
            "eval-errors.jsonl",
            "fidelity.json",
            "dep-closure.jsonl",
            "drvs.tar.zst",
            "evalset.json",
        ] {
            std::fs::write(dir.path(f), format!("content of {f}")).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn uploads_in_order_with_evalset_json_last() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = local_set(&tmp);

        // Sequential rules: the write-once existence probe (NotFound),
        // then one put per artifact, evalset.json strictly last.
        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let puts: Vec<_> = [
            "manifest.jsonl",
            "eval-errors.jsonl",
            "fidelity.json",
            "dep-closure.jsonl",
            "drvs.tar.zst",
            "evalset.json",
        ]
        .iter()
        .map(|f| {
            let key = format!("parity/evals/1824219/abcd1234abcd1234/{f}");
            mock!(aws_sdk_s3::Client::put_object)
                .match_requests(move |req| req.key() == Some(key.as_str()))
                .then_output(|| PutObjectOutput::builder().build())
        })
        .collect();
        let mut rules: Vec<&aws_smithy_mocks::Rule> = vec![&head_404];
        rules.extend(puts.iter());
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, rules);

        let layout = EvalSetS3::new("rio-chunks", "parity");
        let uploaded = layout
            .upload_eval_set(&client, &dir, 1824219, "abcd1234abcd1234")
            .await
            .unwrap();
        assert_eq!(
            uploaded.last().unwrap(),
            "parity/evals/1824219/abcd1234abcd1234/evalset.json"
        );
        assert_eq!(uploaded.len(), 6);
        for rule in &puts {
            assert_eq!(rule.num_calls(), 1, "every artifact uploaded exactly once");
        }
    }

    #[tokio::test]
    async fn refuses_to_overwrite_an_existing_eval_set() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = local_set(&tmp);
        let head_200 = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&head_200]);

        let layout = EvalSetS3::new("rio-chunks", "parity");
        let err = layout
            .upload_eval_set(&client, &dir, 1824219, "abcd1234abcd1234")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("write-once"),
            "expected write-once refusal, got: {err:#}"
        );
    }
}
