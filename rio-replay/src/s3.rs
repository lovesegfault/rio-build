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

use std::path::Path;

use anyhow::Context as _;
use aws_sdk_s3::primitives::ByteStream;
use serde::{Deserialize, Serialize};

use crate::archive::identity;
pub use crate::archive::s3::{
    ARCHIVE_COMPLETE_OBJECT, ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT,
};
use crate::archive::s3::{ARCHIVES_PREFIX_SEGMENT, ArchiveStore};

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

impl ByRecipePointer {
    /// Whether this pointer actually names an archive prefix that can be
    /// probed. Drift-tolerant reads turn unknown or garbage pointer
    /// objects into empty fields rather than errors, and probing an empty
    /// `archive_id_short` would HEAD a malformed `archives//complete.json`
    /// key — an unusable pointer means "re-record", which overwrites it
    /// with a fresh one after the publish.
    pub fn names_archive(&self) -> bool {
        !self.archive_id_short.is_empty()
    }
}

/// Recorder-facing S3 destination (bucket + deployment root prefix, e.g.
/// `replay`) for published replay archives.
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

    /// Key prefix of one archive: `<prefix>/archives/<archive_id_short>/`.
    /// The recorder logs it before uploading so an interrupted attempt's
    /// destination survives in the Job log.
    pub fn archive_prefix(&self, archive_id_short: &str) -> String {
        self.store().prefix(archive_id_short)
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
    use aws_sdk_s3::operation::head_object::HeadObjectOutput;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    #[test]
    fn archive_keys_follow_the_layout() {
        let layout = ArchiveS3::new("rio-chunks", "replay");
        assert_eq!(
            layout.object_key("0123456789abcdef", ARCHIVE_COMPLETE_OBJECT),
            "replay/archives/0123456789abcdef/complete.json"
        );
        let digest = "ab".repeat(32);
        assert_eq!(
            by_recipe_key("replay", &digest),
            format!("replay/archives/by-recipe/{digest}.json")
        );
        // Prefix slashes are normalized.
        let layout = ArchiveS3::new("rio-chunks", "/replay/");
        assert_eq!(
            layout.object_key("0123456789abcdef", ARCHIVE_IMAGE_OBJECT),
            "replay/archives/0123456789abcdef/archive.dwarfs"
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

        let layout = ArchiveS3::new("rio-chunks", "replay");
        let err = layout
            .upload_archive(
                &client,
                tmp.path(),
                &"a".repeat(64),
                "aaaaaaaaaaaaaaaa",
                "rio-replay-eval/test",
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
        let key = format!("replay/archives/by-recipe/{digest}.json");
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

        let layout = ArchiveS3::new("rio-chunks", "replay");
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

        let layout = ArchiveS3::new("rio-chunks", "replay");
        let pointer = layout
            .read_by_recipe_pointer(&client, &digest)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pointer.archive_id_short, "0123456789abcdef");
        assert_eq!(pointer.archive_id, "", "missing fields default to empty");
    }

    #[test]
    fn only_pointers_naming_an_archive_are_usable() {
        // The recorder's already-recorded skip probes the archive prefix the
        // pointer names; a drift-tolerated garbage pointer deserializes to
        // empty fields and must read as unusable instead of producing a
        // malformed `archives//complete.json` probe.
        let usable = ByRecipePointer {
            archive_id: "ab".repeat(32),
            archive_id_short: "abababababababab".to_string(),
            recorded_at: "2026-05-28T00:00:00Z".to_string(),
        };
        assert!(usable.names_archive());
        assert!(!ByRecipePointer::default().names_archive());
    }
}
