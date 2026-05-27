//! Write-once S3 layout writer for eval-set artifacts.
//!
//! Eval sets live under
//! `<prefix>/evals/<hydra-eval-id>/<short-key-digest>/` (e.g.
//! `parity/evals/1824219/8b919129046e0f60/manifest.jsonl`), one
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

    /// Object key for one artifact of one eval set. `key_short_digest`
    /// is the SHORT key digest ([`EvalSetKey::short_digest`]) — the
    /// full digest never appears in object keys.
    ///
    /// [`EvalSetKey::short_digest`]: crate::evalset::key::EvalSetKey::short_digest
    pub fn key(&self, hydra_eval_id: u64, key_short_digest: &str, file: &str) -> String {
        if self.prefix.is_empty() {
            format!("evals/{hydra_eval_id}/{key_short_digest}/{file}")
        } else {
            format!(
                "{}/evals/{hydra_eval_id}/{key_short_digest}/{file}",
                self.prefix
            )
        }
    }

    /// Does `evalset.json` already exist at this prefix?
    pub async fn evalset_exists(
        &self,
        client: &aws_sdk_s3::Client,
        hydra_eval_id: u64,
        key_short_digest: &str,
    ) -> anyhow::Result<bool> {
        let key = self.key(hydra_eval_id, key_short_digest, EVALSET_FILE);
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
    /// refusing if the prefix is already complete. `evalset.json` MUST
    /// exist locally (it is the completeness marker, written even for
    /// dry-run sets); the other artifacts are optional and skipped with
    /// a warning when absent (a dry-run set has no dep-closure or
    /// archive). Returns the uploaded keys in order.
    ///
    /// A failure mid-upload leaves partial objects WITHOUT
    /// `evalset.json` at the prefix — the prefix never looks complete,
    /// and rerunning the same eval set overwrites those partial objects
    /// before writing `evalset.json` last (with a conditional PUT, so
    /// two racing uploads cannot both claim completeness).
    pub async fn upload_eval_set(
        &self,
        client: &aws_sdk_s3::Client,
        dir: &artifacts::EvalSetDir,
        hydra_eval_id: u64,
        key_short_digest: &str,
    ) -> anyhow::Result<Vec<String>> {
        anyhow::ensure!(
            dir.path(EVALSET_FILE).exists(),
            "local eval set {} has no {EVALSET_FILE}; refusing to upload an incomplete set",
            dir.root.display()
        );
        if self
            .evalset_exists(client, hydra_eval_id, key_short_digest)
            .await?
        {
            anyhow::bail!(
                "eval set s3://{}/{} already exists and eval sets are write-once; \
                 use --force to build under a new key digest",
                self.bucket,
                self.key(hydra_eval_id, key_short_digest, EVALSET_FILE)
            );
        }
        let mut uploaded = Vec::new();
        for file in UPLOAD_ORDER {
            let local = dir.path(file);
            if !local.exists() {
                tracing::warn!(file, "artifact missing locally; skipping upload");
                continue;
            }
            let key = self.key(hydra_eval_id, key_short_digest, file);
            let body = ByteStream::from_path(&local)
                .await
                .with_context(|| format!("read {}", local.display()))?;
            let mut put = client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .content_type(content_type(file))
                .body(body);
            if file == EVALSET_FILE {
                // evalset.json is the completeness marker: the
                // conditional PUT makes claiming the prefix atomic, so
                // two concurrent uploads of the same key digest cannot
                // both believe they won.
                put = put.if_none_match("*");
            }
            put.send()
                .await
                .with_context(|| format!("PUT s3://{}/{key}", self.bucket))?;
            tracing::info!(key = %key, "uploaded");
            uploaded.push(key);
        }
        Ok(uploaded)
    }
}

/// Content-Type for one eval-set artifact, by its fixed filename.
fn content_type(file: &str) -> &'static str {
    if file.ends_with(".jsonl") {
        "application/x-ndjson"
    } else if file.ends_with(".json") {
        "application/json"
    } else {
        // drvs.tar.zst — a zstd-compressed tar stream.
        "application/zstd"
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
        // then one put per artifact, evalset.json strictly last. Each
        // put rule also pins the artifact's Content-Type and that ONLY
        // the final evalset.json put is conditional (If-None-Match: *).
        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let puts: Vec<_> = [
            ("manifest.jsonl", "application/x-ndjson", false),
            ("eval-errors.jsonl", "application/x-ndjson", false),
            ("fidelity.json", "application/json", false),
            ("dep-closure.jsonl", "application/x-ndjson", false),
            ("drvs.tar.zst", "application/zstd", false),
            ("evalset.json", "application/json", true),
        ]
        .iter()
        .map(|&(f, ctype, conditional)| {
            let key = format!("parity/evals/1824219/abcd1234abcd1234/{f}");
            mock!(aws_sdk_s3::Client::put_object)
                .match_requests(move |req| {
                    let conditional_ok = if conditional {
                        req.if_none_match() == Some("*")
                    } else {
                        req.if_none_match().is_none()
                    };
                    req.key() == Some(key.as_str())
                        && req.content_type() == Some(ctype)
                        && conditional_ok
                })
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
    async fn refuses_to_upload_a_local_set_missing_evalset_json() {
        // evalset.json is the completeness marker; a local set without
        // it must hard-fail before any S3 request is issued (the other
        // artifacts being optional does NOT extend to the marker).
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::evalset::artifacts::EvalSetDir::create(tmp.path()).unwrap();
        std::fs::write(dir.path("manifest.jsonl"), "{}\n").unwrap();

        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&head_404]);

        let layout = EvalSetS3::new("rio-chunks", "parity");
        let err = layout
            .upload_eval_set(&client, &dir, 1824219, "abcd1234abcd1234")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("evalset.json"),
            "expected the missing-marker refusal, got: {err:#}"
        );
        assert_eq!(head_404.num_calls(), 0, "no S3 request should be made");
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
