//! Recorder-facing S3 layer: archive publication plus the by-recipe pointer.
//!
//! Published replay archives live under
//! `<prefix>/archives/<archive_id_short>/` as three objects —
//! `archive.dwarfs`, the standalone `manifest.json`, and `complete.json`
//! uploaded strictly last with a conditional PUT so the prefix is
//! write-once. That upload/probe machinery is owned by
//! [`crate::archive::s3::ArchiveStore`]; [`ArchiveS3`] only adapts it to
//! the recorder's local output directory and layers on the by-recipe
//! pointer at `<prefix>/archives/by-recipe/<recipe_digest>.json`, which
//! lets a re-run of an already-recorded reproduction recipe find the
//! existing archive instead of recording a duplicate. The pointer is
//! written only after `complete.json` and is never read by the campaign
//! engine.
//!
//! The legacy eval-set uploader [`EvalSetS3`] (write-once
//! `<prefix>/evals/<hydra-eval-id>/<short-key-digest>/` layout, completeness
//! marked by `evalset.json`) remains below until the eval pipeline records
//! archives directly.

use std::path::Path;

use anyhow::Context as _;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use serde::{Deserialize, Serialize};

use crate::archive::identity;
pub use crate::archive::s3::{
    ARCHIVE_COMPLETE_OBJECT, ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT,
};
use crate::archive::s3::{ARCHIVES_PREFIX_SEGMENT, ArchiveStore};
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

/// Path segment under `<prefix>/archives/` that holds the recorder-owned
/// by-recipe idempotency pointers.
pub const BY_RECIPE_SEGMENT: &str = "by-recipe";

/// Key of the by-recipe pointer for one recipe digest:
/// `<prefix>/archives/by-recipe/<recipe_digest>.json` (prefix slashes
/// trimmed; an empty prefix roots the tree at the bucket root). The pointer
/// lives under the same `archives/` tree as the archive prefixes it points
/// into.
pub fn by_recipe_key(prefix: &str, recipe_digest: &str) -> String {
    let root = prefix.trim_matches('/');
    if root.is_empty() {
        format!("{ARCHIVES_PREFIX_SEGMENT}/{BY_RECIPE_SEGMENT}/{recipe_digest}.json")
    } else {
        format!("{root}/{ARCHIVES_PREFIX_SEGMENT}/{BY_RECIPE_SEGMENT}/{recipe_digest}.json")
    }
}

/// Recorder-owned by-recipe idempotency pointer: which archive a given
/// reproduction recipe (the `EvalSetKey` digest) was last recorded as.
///
/// Written only after a successful publish — the archive's `complete.json`
/// must exist before any pointer names it — and read before re-recording so
/// an already-recorded recipe is skipped instead of duplicated. Unlike
/// archive prefixes, pointers are deliberately last-writer-wins (a forced
/// re-record salts the recipe key and therefore writes a different pointer
/// object). The campaign engine never reads them. Reads tolerate shape
/// drift: unknown fields are ignored and missing fields default to empty so
/// a pointer written by a newer recorder still resolves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ByRecipePointer {
    /// Full 64-hex archive id of the recorded archive.
    pub archive_id: String,
    /// First 16 hex characters of the archive id (the S3 prefix segment).
    pub archive_id_short: String,
    /// When the pointer was written (RFC 3339).
    pub recorded_at: String,
}

/// Recorder-facing S3 destination (bucket + deployment root prefix, e.g.
/// `parity`) for published replay archives.
///
/// The upload/probe mechanics — write-once prefixes, upload order, the
/// completion marker — are [`ArchiveStore`]'s; this type adapts them to the
/// recorder's local output directory (a packed `archive.dwarfs` with the
/// standalone `manifest.json` beside it) and adds the by-recipe idempotency
/// pointer, which only the recorder reads or writes.
#[derive(Debug, Clone)]
pub struct ArchiveS3 {
    pub bucket: String,
    pub prefix: String,
}

impl ArchiveS3 {
    /// Bucket + deployment root prefix; surrounding slashes on the prefix
    /// are trimmed (an empty prefix roots the `archives/` tree at the
    /// bucket root).
    pub fn new(bucket: &str, prefix: &str) -> Self {
        Self {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        }
    }

    /// The engine-facing store this layer delegates uploads and existence
    /// probes to.
    fn store(&self) -> ArchiveStore {
        ArchiveStore::new(&self.bucket, &self.prefix)
    }

    /// Key of one object at one archive's prefix:
    /// `<prefix>/archives/<archive_id_short>/<object>`.
    pub fn object_key(&self, archive_id_short: &str, object: &str) -> String {
        self.store().object_key(archive_id_short, object)
    }

    /// Is this archive already published — does its `complete.json` exist?
    pub async fn archive_exists(
        &self,
        client: &aws_sdk_s3::Client,
        archive_id_short: &str,
    ) -> anyhow::Result<bool> {
        self.store().is_complete(client, archive_id_short).await
    }

    /// Publish a packed archive from the recorder's local output directory
    /// `staged`, which holds `archive.dwarfs` and the standalone
    /// `manifest.json` side by side.
    ///
    /// `archive_id`/`archive_id_short` are the ids the caller computed when
    /// staging; they are cross-checked against the manifest found in
    /// `staged` before any S3 traffic, so the recorder can never publish
    /// under a different prefix than the one it is about to record in its
    /// own pointer and provenance. The upload itself — image first, then
    /// the manifest, then `complete.json` strictly last via a conditional
    /// PUT, refusing prefixes that already claimed completeness — is
    /// [`ArchiveStore::publish`]. Returns the uploaded keys in upload
    /// order.
    pub async fn upload_archive(
        &self,
        client: &aws_sdk_s3::Client,
        staged: &Path,
        archive_id: &str,
        archive_id_short: &str,
        uploader: &str,
    ) -> anyhow::Result<Vec<String>> {
        let manifest_path = staged.join(ARCHIVE_MANIFEST_OBJECT);
        let manifest_bytes = tokio::fs::read(&manifest_path).await.with_context(|| {
            format!(
                "read {} (the standalone manifest copied next to the packed image)",
                manifest_path.display()
            )
        })?;
        let derived_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        anyhow::ensure!(
            derived_id == archive_id,
            "staged manifest at {} does not match the archive id this recording produced: the \
             manifest hashes to {derived_id} but the recorder staged {archive_id}",
            manifest_path.display()
        );
        anyhow::ensure!(
            identity::short_id(&derived_id) == archive_id_short,
            "archive_id_short {archive_id_short} does not match archive id {archive_id}"
        );
        self.store()
            .publish(
                client,
                &staged.join(ARCHIVE_IMAGE_OBJECT),
                &manifest_bytes,
                uploader,
            )
            .await?;
        Ok(vec![
            self.object_key(archive_id_short, ARCHIVE_IMAGE_OBJECT),
            self.object_key(archive_id_short, ARCHIVE_MANIFEST_OBJECT),
            self.object_key(archive_id_short, ARCHIVE_COMPLETE_OBJECT),
        ])
    }

    /// Write the by-recipe pointer for `recipe_digest`. Call only after
    /// [`Self::upload_archive`] has succeeded: the pointer must never name
    /// an archive whose `complete.json` is not in place. A plain PUT —
    /// pointers are intentionally last-writer-wins.
    pub async fn write_by_recipe_pointer(
        &self,
        client: &aws_sdk_s3::Client,
        recipe_digest: &str,
        pointer: &ByRecipePointer,
    ) -> anyhow::Result<()> {
        let key = by_recipe_key(&self.prefix, recipe_digest);
        let mut body =
            serde_json::to_vec_pretty(pointer).context("serialize the by-recipe pointer")?;
        body.push(b'\n');
        client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .content_type("application/json")
            .body(ByteStream::from(body))
            .send()
            .await
            .with_context(|| format!("PUT s3://{}/{key}", self.bucket))?;
        tracing::info!(key = %key, archive_id = %pointer.archive_id, "wrote by-recipe pointer");
        Ok(())
    }

    /// Read the by-recipe pointer for `recipe_digest`; `Ok(None)` when no
    /// pointer exists (the recipe has never been recorded). A pointer can
    /// outlive or predate its archive, so callers still probe
    /// [`Self::archive_exists`] before trusting one.
    pub async fn read_by_recipe_pointer(
        &self,
        client: &aws_sdk_s3::Client,
        recipe_digest: &str,
    ) -> anyhow::Result<Option<ByRecipePointer>> {
        let key = by_recipe_key(&self.prefix, recipe_digest);
        let resp = match client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                return Ok(None);
            }
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context(format!("GET s3://{}/{key}", self.bucket))
                );
            }
        };
        let bytes = resp
            .body
            .collect()
            .await
            .with_context(|| format!("read s3://{}/{key}", self.bucket))?
            .into_bytes();
        let pointer: ByRecipePointer = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse s3://{}/{key}", self.bucket))?;
        Ok(Some(pointer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
    use aws_sdk_s3::operation::head_object::{HeadObjectError, HeadObjectOutput};
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::types::error::{NoSuchKey, NotFound};
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

    #[test]
    fn archive_keys_follow_the_layout() {
        let layout = ArchiveS3::new("rio-chunks", "parity");
        assert_eq!(
            layout.object_key("0123456789abcdef", ARCHIVE_COMPLETE_OBJECT),
            "parity/archives/0123456789abcdef/complete.json"
        );
        let digest = "ab".repeat(32);
        assert_eq!(
            by_recipe_key("parity", &digest),
            format!("parity/archives/by-recipe/{digest}.json")
        );
        // Prefix slashes are normalized.
        let layout = ArchiveS3::new("rio-chunks", "/parity/");
        assert_eq!(
            layout.object_key("0123456789abcdef", ARCHIVE_IMAGE_OBJECT),
            "parity/archives/0123456789abcdef/archive.dwarfs"
        );
        // An empty prefix roots the archives tree at the bucket root without
        // a leading slash, for the pointer keys exactly like for the archive
        // object keys.
        let layout = ArchiveS3::new("rio-chunks", "");
        assert_eq!(
            layout.object_key("0123456789abcdef", ARCHIVE_COMPLETE_OBJECT),
            "archives/0123456789abcdef/complete.json"
        );
        assert_eq!(
            by_recipe_key("", &digest),
            format!("archives/by-recipe/{digest}.json")
        );
    }

    #[tokio::test]
    async fn upload_archive_refuses_a_mismatched_archive_id() {
        // The caller passes the ids it computed at staging time; when the
        // manifest sitting next to the image hashes to something else the
        // wrapper must refuse before any S3 traffic, otherwise the publish
        // would land under a different prefix than the one the recorder
        // records in its by-recipe pointer and provenance.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(ARCHIVE_MANIFEST_OBJECT), b"{}\n").unwrap();

        let head = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&head]);

        let layout = ArchiveS3::new("rio-chunks", "parity");
        let err = layout
            .upload_archive(
                &client,
                tmp.path(),
                &"a".repeat(64),
                "aaaaaaaaaaaaaaaa",
                "rio-parity-eval/test",
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match"),
            "expected the archive-id cross-check refusal, got: {err:#}"
        );
        assert_eq!(head.num_calls(), 0, "no S3 request may be issued");
    }

    #[tokio::test]
    async fn by_recipe_pointer_round_trips() {
        let digest = "cd".repeat(32);
        let key = format!("parity/archives/by-recipe/{digest}.json");
        let pointer = ByRecipePointer {
            archive_id: "ab".repeat(32),
            archive_id_short: "abababababababab".to_string(),
            recorded_at: "2026-05-28T00:00:00Z".to_string(),
        };

        // The write is a plain unconditional PUT (pointers are
        // last-writer-wins, unlike the write-once archive prefixes they
        // point at) carrying exactly the pointer fields as JSON.
        let put_key = key.clone();
        let put = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(move |req| {
                let body: serde_json::Value =
                    serde_json::from_slice(req.body().bytes().expect("in-memory body")).unwrap();
                req.key() == Some(put_key.as_str())
                    && req.content_type() == Some("application/json")
                    && req.if_none_match().is_none()
                    && body["archive_id"] == "ab".repeat(32)
                    && body["archive_id_short"] == "abababababababab"
                    && body["recorded_at"] == "2026-05-28T00:00:00Z"
            })
            .then_output(|| PutObjectOutput::builder().build());
        let get_key = key.clone();
        let get_body = serde_json::to_vec(&pointer).unwrap();
        let get = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(move |req| req.key() == Some(get_key.as_str()))
            .then_output(move || {
                GetObjectOutput::builder()
                    .body(ByteStream::from(get_body.clone()))
                    .build()
            });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&put, &get]);

        let layout = ArchiveS3::new("rio-chunks", "parity");
        layout
            .write_by_recipe_pointer(&client, &digest, &pointer)
            .await
            .unwrap();
        let read = layout
            .read_by_recipe_pointer(&client, &digest)
            .await
            .unwrap();
        assert_eq!(read, Some(pointer));
        assert_eq!(put.num_calls(), 1);
        assert_eq!(get.num_calls(), 1);

        // An absent pointer (NoSuchKey) reads back as None, not an error.
        let missing = mock!(aws_sdk_s3::Client::get_object)
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&missing]);
        assert_eq!(
            layout
                .read_by_recipe_pointer(&client, &digest)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn by_recipe_pointer_read_tolerates_shape_drift() {
        // A pointer written by a different recorder version may carry extra
        // fields or omit ones this version knows about; the read must not
        // fail on that — the caller falls back to probing/re-recording when
        // a pointer is unusable.
        let digest = "ef".repeat(32);
        let body = serde_json::json!({
            "archive_id_short": "0123456789abcdef",
            "written_by": "a newer recorder",
        });
        let get = mock!(aws_sdk_s3::Client::get_object).then_output(move || {
            GetObjectOutput::builder()
                .body(ByteStream::from(serde_json::to_vec(&body).unwrap()))
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&get]);

        let layout = ArchiveS3::new("rio-chunks", "parity");
        let pointer = layout
            .read_by_recipe_pointer(&client, &digest)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pointer.archive_id_short, "0123456789abcdef");
        assert_eq!(pointer.archive_id, "", "missing fields default to empty");
    }
}
