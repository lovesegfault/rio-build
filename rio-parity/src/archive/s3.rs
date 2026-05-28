//! Write-once S3 layout for replay archives — publish, fetch, list.
//!
//! Archives at rest live under a digest-keyed, write-once prefix:
//! `<root>/archives/<archive_id_short>/`. The at-rest representation is
//! always the DwarFS image (`archive.dwarfs`) plus two small standalone
//! control objects, so listing and probing never require fetching the
//! image: `manifest.json` (byte-identical to the manifest member inside
//! the image — the identity bytes) and `complete.json`, the completeness
//! marker, uploaded strictly last with a conditional PUT
//! (`If-None-Match: *`). A prefix without `complete.json` is incomplete:
//! the engine never uses it and tooling never lists it, and an uploader
//! that finds the marker already present refuses to overwrite. This
//! mirrors the eval-set upload discipline in [`crate::s3`] (see
//! `docs/dev/2026-05-28-build-replay-design.md`, "S3 layout, write-once
//! upload, completion marker").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::identity;
use super::reader::ReplayArchive;
use super::schema::MemberDigest;

/// Object names at an archive prefix.
pub const ARCHIVE_IMAGE_OBJECT: &str = "archive.dwarfs";
pub const ARCHIVE_MANIFEST_OBJECT: &str = "manifest.json";
pub const ARCHIVE_COMPLETE_OBJECT: &str = "complete.json";
/// Path segment under the deployment root that holds all archives.
pub const ARCHIVES_PREFIX_SEGMENT: &str = "archives";

/// `complete.json`: the upload completion marker, always uploaded last.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteMarker {
    /// Full 64-hex archive id (SHA-256 of manifest.json's bytes).
    pub archive_id: String,
    /// First 16 hex characters; must equal the prefix segment.
    pub archive_id_short: String,
    /// Every object at the prefix except complete.json itself.
    pub objects: BTreeMap<String, MemberDigest>,
    /// Upload completion time.
    pub uploaded_at: jiff::Timestamp,
    /// Free-form tool/version string.
    pub uploader: String,
}

/// A fetched archive on local disk, ready to open in place.
#[derive(Debug, Clone)]
pub struct FetchedArchive {
    /// The downloaded `archive.dwarfs` image.
    pub image_path: PathBuf,
    /// The downloaded standalone `manifest.json` (the identity bytes).
    pub manifest_path: PathBuf,
    /// The parsed completion marker the objects were verified against.
    pub marker: CompleteMarker,
}

/// S3 destination (bucket + deployment root prefix, e.g. "parity") for
/// replay archives.
#[derive(Debug, Clone)]
pub struct ArchiveStore {
    pub bucket: String,
    pub root: String,
}

impl ArchiveStore {
    /// Bucket + deployment root prefix (e.g. `parity`). Surrounding slashes
    /// on the root are trimmed; an empty root puts the `archives/` tree at
    /// the bucket root.
    pub fn new(bucket: &str, root: &str) -> Self {
        Self {
            bucket: bucket.to_string(),
            root: root.trim_matches('/').to_string(),
        }
    }

    /// Key prefix of one archive: `<root>/archives/<archive_id_short>/`
    /// (no leading slash when the root is empty).
    pub fn prefix(&self, archive_id_short: &str) -> String {
        format!("{}{archive_id_short}/", self.archives_prefix())
    }

    /// Key of one object at one archive's prefix.
    pub fn object_key(&self, archive_id_short: &str, object: &str) -> String {
        format!("{}{object}", self.prefix(archive_id_short))
    }

    /// Key prefix holding every archive: `<root>/archives/`.
    fn archives_prefix(&self) -> String {
        if self.root.is_empty() {
            format!("{ARCHIVES_PREFIX_SEGMENT}/")
        } else {
            format!("{}/{ARCHIVES_PREFIX_SEGMENT}/", self.root)
        }
    }

    /// Does `complete.json` already exist at this archive's prefix?
    pub async fn is_complete(
        &self,
        client: &aws_sdk_s3::Client,
        archive_id_short: &str,
    ) -> anyhow::Result<bool> {
        let key = self.object_key(archive_id_short, ARCHIVE_COMPLETE_OBJECT);
        match client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(err)) if err.err().is_not_found() => Ok(false),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("HEAD s3://{}/{key}", self.bucket)))
            }
        }
    }

    /// The refusal raised when an archive prefix has already claimed
    /// completeness. Shared by the HEAD pre-check and the 412 mapping on the
    /// final conditional PUT so both ways of losing the prefix read the same.
    fn write_once_refusal(&self, archive_id_short: &str) -> String {
        format!(
            "archive prefix s3://{}/{} already has {ARCHIVE_COMPLETE_OBJECT} and archive \
             prefixes are write-once; an identical archive id means this content is already \
             published (probe is_complete before publishing) — only a re-record, which gets a \
             new archive id, needs a new prefix",
            self.bucket,
            self.prefix(archive_id_short)
        )
    }

    /// Publish a packed archive: upload `archive.dwarfs`, the standalone
    /// `manifest.json` (the exact bytes passed in), and finally
    /// `complete.json` with `If-None-Match: *` so two racing uploads cannot
    /// both claim completeness.
    ///
    /// `manifest_bytes` must be byte-identical to the `manifest.json`
    /// member inside the image (the archive's identity bytes): the image is
    /// opened and cross-checked before anything is uploaded, so the
    /// standalone manifest can never disagree with the image it stands for.
    /// v0 archives have no `archive_id` and cannot live in the digest-keyed
    /// layout; they are refused. A prefix that already carries
    /// `complete.json` is write-once and refused: the HEAD pre-check is only
    /// the cheap early refusal (it avoids uploading a multi-gigabyte image
    /// just to lose the claim), while the conditional PUT of `complete.json`
    /// is the authoritative write-once claim.
    ///
    /// A failure mid-upload leaves partial objects WITHOUT `complete.json`
    /// at the prefix — the prefix never looks complete, and a retry of the
    /// same archive overwrites those partial objects before writing
    /// `complete.json` last.
    pub async fn publish(
        &self,
        client: &aws_sdk_s3::Client,
        image_path: &Path,
        manifest_bytes: &[u8],
        uploader: &str,
    ) -> anyhow::Result<CompleteMarker> {
        let archive_id = identity::archive_id_from_manifest_bytes(manifest_bytes);
        let archive_id_short = identity::short_id(&archive_id);

        // Cross-check the image before any S3 traffic: it must be a v1
        // archive whose manifest member hashes to the same archive id as the
        // standalone manifest bytes (same SHA-256 ⇒ same bytes). Opening a
        // DwarFS image is blocking work, and the image digest for the
        // completion marker is computed in the same pass.
        let expected_id = archive_id.clone();
        let image = image_path.to_path_buf();
        let image_digest = tokio::task::spawn_blocking(move || -> anyhow::Result<MemberDigest> {
            anyhow::ensure!(
                image.is_file(),
                "{} is not a file; the published form of an archive is the packed DwarFS image \
                 (pack a staged directory with mkdwarfs first)",
                image.display()
            );
            let archive = ReplayArchive::open(&image)
                .with_context(|| format!("open {} for publishing", image.display()))?;
            match archive.archive_id() {
                None => anyhow::bail!(
                    "{}: v0 archives cannot be published (they have no archive_id); re-record or \
                     convert to v1 to publish to S3",
                    image.display()
                ),
                Some(id) if id != expected_id => anyhow::bail!(
                    "{}: standalone manifest does not match the image: the manifest bytes hash \
                     to {expected_id} but the image's manifest member hashes to {id}",
                    image.display()
                ),
                Some(_) => {}
            }
            file_member_digest(&image)
        })
        .await
        .context("image cross-check task panicked or was cancelled")??;

        // Write-once: a prefix that already claimed completeness is never
        // overwritten; re-recording produces a new id and a new prefix.
        if self.is_complete(client, &archive_id_short).await? {
            anyhow::bail!(self.write_once_refusal(&archive_id_short));
        }

        // Data first: the image, then the standalone manifest.
        let image_key = self.object_key(&archive_id_short, ARCHIVE_IMAGE_OBJECT);
        let body = ByteStream::from_path(image_path)
            .await
            .with_context(|| format!("read {}", image_path.display()))?;
        client
            .put_object()
            .bucket(&self.bucket)
            .key(&image_key)
            .content_type("application/octet-stream")
            .body(body)
            .send()
            .await
            .with_context(|| format!("PUT s3://{}/{image_key}", self.bucket))?;
        tracing::info!(key = %image_key, "uploaded");

        let manifest_key = self.object_key(&archive_id_short, ARCHIVE_MANIFEST_OBJECT);
        client
            .put_object()
            .bucket(&self.bucket)
            .key(&manifest_key)
            .content_type("application/json")
            .body(ByteStream::from(manifest_bytes.to_vec()))
            .send()
            .await
            .with_context(|| format!("PUT s3://{}/{manifest_key}", self.bucket))?;
        tracing::info!(key = %manifest_key, "uploaded");

        // The completion marker goes strictly last; the conditional PUT makes
        // claiming the prefix atomic.
        let marker = CompleteMarker {
            objects: BTreeMap::from([
                (ARCHIVE_IMAGE_OBJECT.to_string(), image_digest),
                (
                    ARCHIVE_MANIFEST_OBJECT.to_string(),
                    MemberDigest {
                        // The standalone manifest is the identity bytes, so
                        // its digest IS the archive id.
                        sha256: archive_id.clone(),
                        size: manifest_bytes.len() as u64,
                    },
                ),
            ]),
            archive_id,
            archive_id_short: archive_id_short.clone(),
            uploaded_at: jiff::Timestamp::now(),
            uploader: uploader.to_string(),
        };
        let mut marker_bytes =
            serde_json::to_vec_pretty(&marker).context("serialize complete.json")?;
        marker_bytes.push(b'\n');
        let complete_key = self.object_key(&archive_id_short, ARCHIVE_COMPLETE_OBJECT);
        match client
            .put_object()
            .bucket(&self.bucket)
            .key(&complete_key)
            .content_type("application/json")
            .if_none_match("*")
            .body(ByteStream::from(marker_bytes))
            .send()
            .await
        {
            Ok(_) => {}
            // The conditional PUT lost: another publisher claimed the prefix
            // between the HEAD pre-check and this final upload. Surface the
            // same actionable refusal as the pre-check — the raw 412 carries
            // nothing the message does not already say.
            Err(e) if put_is_precondition_failed(&e) => {
                anyhow::bail!(self.write_once_refusal(&archive_id_short));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("PUT s3://{}/{complete_key}", self.bucket)));
            }
        }
        tracing::info!(
            key = %complete_key,
            archive_id = %marker.archive_id,
            "archive published"
        );
        Ok(marker)
    }

    /// Fetch a published archive into `dest_dir`: read `complete.json`,
    /// download every object it lists (verifying each object's SHA-256 and
    /// size against the marker), and verify that the downloaded standalone
    /// `manifest.json` hashes to the marker's `archive_id`. The image is
    /// downloaded as-is and opened in place — there is no unpack step.
    ///
    /// A failed download or digest check can leave partial files behind in
    /// `dest_dir`; a retry overwrites them in place. Treat the destination
    /// directory of a failed fetch as scratch — nothing in it has been
    /// verified.
    pub async fn fetch(
        &self,
        client: &aws_sdk_s3::Client,
        archive_id_short: &str,
        dest_dir: &Path,
    ) -> anyhow::Result<FetchedArchive> {
        let complete_key = self.object_key(archive_id_short, ARCHIVE_COMPLETE_OBJECT);
        let resp = client
            .get_object()
            .bucket(&self.bucket)
            .key(&complete_key)
            .send()
            .await
            .with_context(|| {
                format!(
                    "GET s3://{}/{complete_key} (is the archive complete?)",
                    self.bucket
                )
            })?;
        let marker_bytes = resp
            .body
            .collect()
            .await
            .with_context(|| format!("read s3://{}/{complete_key}", self.bucket))?
            .into_bytes();
        let marker: CompleteMarker = serde_json::from_slice(&marker_bytes)
            .with_context(|| format!("parse s3://{}/{complete_key}", self.bucket))?;
        anyhow::ensure!(
            marker.archive_id_short == archive_id_short,
            "short id mismatch at s3://{}/{complete_key}: the prefix says {archive_id_short} but \
             {ARCHIVE_COMPLETE_OBJECT} says {}",
            self.bucket,
            marker.archive_id_short
        );

        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("create {}", dest_dir.display()))?;
        for (object, expected) in &marker.objects {
            // The marker is remote input: only plain basenames may be joined
            // onto the destination directory.
            anyhow::ensure!(
                !object.is_empty()
                    && !object.contains('/')
                    && !object.contains('\\')
                    && object != "..",
                "{ARCHIVE_COMPLETE_OBJECT} at s3://{}/{complete_key} lists a non-basename object \
                 name {object:?}; refusing to write outside {}",
                self.bucket,
                dest_dir.display()
            );
            let key = self.object_key(archive_id_short, object);
            self.download_verified(client, &key, &dest_dir.join(object), object, expected)
                .await?;
        }
        for required in [ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT] {
            anyhow::ensure!(
                marker.objects.contains_key(required),
                "{ARCHIVE_COMPLETE_OBJECT} lists no {required} at s3://{}/{}; the upload is \
                 malformed",
                self.bucket,
                self.prefix(archive_id_short)
            );
        }

        // The standalone manifest is the identity bytes: hashing the
        // downloaded copy must reproduce the archive id the marker claims.
        let manifest_path = dest_dir.join(ARCHIVE_MANIFEST_OBJECT);
        let manifest_bytes = tokio::fs::read(&manifest_path)
            .await
            .with_context(|| format!("read {}", manifest_path.display()))?;
        let manifest_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        anyhow::ensure!(
            manifest_id == marker.archive_id,
            "downloaded {ARCHIVE_MANIFEST_OBJECT} hashes to {manifest_id} but \
             {ARCHIVE_COMPLETE_OBJECT} says the archive id is {}",
            marker.archive_id
        );

        Ok(FetchedArchive {
            image_path: dest_dir.join(ARCHIVE_IMAGE_OBJECT),
            manifest_path,
            marker,
        })
    }

    /// [`Self::fetch`], then open the downloaded image in place and verify
    /// that the manifest member inside the image yields the same
    /// `archive_id` the completion marker claims.
    pub async fn fetch_and_open(
        &self,
        client: &aws_sdk_s3::Client,
        archive_id_short: &str,
        dest_dir: &Path,
    ) -> anyhow::Result<(ReplayArchive, FetchedArchive)> {
        let fetched = self.fetch(client, archive_id_short, dest_dir).await?;
        let image = fetched.image_path.clone();
        let archive = tokio::task::spawn_blocking(move || ReplayArchive::open(&image))
            .await
            .context("archive open task panicked or was cancelled")??;
        anyhow::ensure!(
            archive.archive_id() == Some(fetched.marker.archive_id.as_str()),
            "downloaded image manifest does not match {ARCHIVE_COMPLETE_OBJECT}: the image at {} \
             has archive id {:?} but the marker says {}",
            fetched.image_path.display(),
            archive.archive_id(),
            fetched.marker.archive_id
        );
        Ok((archive, fetched))
    }

    /// List every complete archive under `<root>/archives/`. Archives are
    /// discovered through their `complete.json` objects only, so incomplete
    /// or in-flight uploads are invisible. Each marker's `archive_id_short`
    /// is cross-checked against the prefix segment it was found under.
    /// Returns `(archive_id_short, marker)` pairs sorted by short id.
    pub async fn list(
        &self,
        client: &aws_sdk_s3::Client,
    ) -> anyhow::Result<Vec<(String, CompleteMarker)>> {
        let prefix = self.archives_prefix();
        let complete_suffix = format!("/{ARCHIVE_COMPLETE_OBJECT}");
        let mut complete_keys: Vec<(String, String)> = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut request = client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);
            if let Some(token) = &continuation {
                request = request.continuation_token(token);
            }
            let page = request
                .send()
                .await
                .with_context(|| format!("LIST s3://{}/{prefix}", self.bucket))?;
            for object in page.contents() {
                let Some(key) = object.key() else { continue };
                let Some(parent) = key.strip_suffix(&complete_suffix) else {
                    continue;
                };
                // The path segment right before complete.json is the short id.
                let short = parent.rsplit('/').next().unwrap_or(parent).to_string();
                complete_keys.push((short, key.to_string()));
            }
            if page.is_truncated() == Some(true) {
                continuation = page.next_continuation_token().map(str::to_string);
                anyhow::ensure!(
                    continuation.is_some(),
                    "LIST s3://{}/{prefix}: truncated response without a continuation token",
                    self.bucket
                );
            } else {
                break;
            }
        }

        let mut archives = Vec::with_capacity(complete_keys.len());
        for (short, key) in complete_keys {
            let resp = client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .with_context(|| format!("GET s3://{}/{key}", self.bucket))?;
            let bytes = resp
                .body
                .collect()
                .await
                .with_context(|| format!("read s3://{}/{key}", self.bucket))?
                .into_bytes();
            let marker: CompleteMarker = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse s3://{}/{key}", self.bucket))?;
            // Publishing always writes the marker under its own short id, so
            // a mismatch means the prefix was hand-copied or the marker was
            // edited; surface that instead of returning an entry whose fetch
            // would fail anyway.
            anyhow::ensure!(
                marker.archive_id_short == short,
                "{ARCHIVE_COMPLETE_OBJECT} at s3://{}/{key} says archive_id_short {} but it \
                 lives under the {short}/ prefix; the archive was copied to the wrong prefix \
                 or the marker was edited",
                self.bucket,
                marker.archive_id_short
            );
            archives.push((short, marker));
        }
        archives.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(archives)
    }

    /// GET one object and stream it to `dest`, verifying its SHA-256 and
    /// size against the completion marker's entry as the bytes arrive.
    async fn download_verified(
        &self,
        client: &aws_sdk_s3::Client,
        key: &str,
        dest: &Path,
        object: &str,
        expected: &MemberDigest,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let resp = client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("GET s3://{}/{key}", self.bucket))?;
        let mut body = resp.body;
        let file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("create {}", dest.display()))?;
        let mut writer = tokio::io::BufWriter::new(file);
        let mut hasher = sha2::Sha256::new();
        let mut size: u64 = 0;
        while let Some(chunk) = body
            .try_next()
            .await
            .with_context(|| format!("read s3://{}/{key}", self.bucket))?
        {
            hasher.update(&chunk);
            size += chunk.len() as u64;
            writer
                .write_all(&chunk)
                .await
                .with_context(|| format!("write {}", dest.display()))?;
        }
        writer
            .flush()
            .await
            .with_context(|| format!("flush {}", dest.display()))?;

        let sha256 = hex::encode(hasher.finalize());
        let expected_sha256 = &expected.sha256;
        let expected_size = expected.size;
        anyhow::ensure!(
            sha256 == *expected_sha256 && size == expected_size,
            "download digest mismatch for {object} at s3://{}/{key}: downloaded {size} bytes \
             with sha256 {sha256}, {ARCHIVE_COMPLETE_OBJECT} says {expected_size} bytes with \
             sha256 {expected_sha256}",
            self.bucket
        );
        Ok(())
    }
}

/// Did this PUT lose an `If-None-Match: *` conditional write — an HTTP 412
/// `PreconditionFailed` rejection because the object already exists? Generic
/// over the response type so we don't have to name aws-smithy-runtime-api
/// types (not a direct dependency).
fn put_is_precondition_failed<R>(
    err: &SdkError<aws_sdk_s3::operation::put_object::PutObjectError, R>,
) -> bool {
    use aws_sdk_s3::error::ProvideErrorMetadata as _;

    match err {
        SdkError::ServiceError(se) => se.err().code() == Some("PreconditionFailed"),
        _ => false,
    }
}

/// Streaming SHA-256 + size of a local file (the image of a fat archive can
/// be large, so it is hashed in chunks rather than read into memory).
fn file_member_digest(path: &Path) -> anyhow::Result<MemberDigest> {
    use std::io::Read as _;

    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = sha2::Sha256::new();
    let mut size: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok(MemberDigest {
        sha256: hex::encode(hasher.finalize()),
        size,
    })
}

#[cfg(test)]
mod tests {
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_sdk_s3::operation::get_object::GetObjectOutput;
    use aws_sdk_s3::operation::head_object::{HeadObjectError, HeadObjectOutput};
    use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
    use aws_sdk_s3::operation::put_object::{PutObjectError, PutObjectOutput};
    use aws_sdk_s3::types::Object;
    use aws_sdk_s3::types::error::NotFound;
    use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};

    use super::*;
    use crate::archive::MANIFEST_MEMBER;
    use crate::archive::writer::pack_with_mkdwarfs;
    use crate::archive::writer::test_support::tiny_archive;

    /// Stage the canonical tiny v1 archive in `dir`, pack it into a DwarFS
    /// image, and return the image path plus the staged manifest.json bytes
    /// (the identity bytes a recorder would publish alongside the image).
    fn packed_tiny_archive(dir: &Path) -> (PathBuf, Vec<u8>) {
        let root = dir.join("archive");
        tiny_archive(&root);
        let image = dir.join("archive.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();
        let manifest_bytes = std::fs::read(root.join(MANIFEST_MEMBER)).unwrap();
        (image, manifest_bytes)
    }

    /// A GET rule that returns `bytes` for exactly the object at `key`.
    fn get_rule(key: String, bytes: Vec<u8>) -> Rule {
        mock!(aws_sdk_s3::Client::get_object)
            .match_requests(move |req| req.key() == Some(key.as_str()))
            .then_output(move || {
                GetObjectOutput::builder()
                    .body(ByteStream::from(bytes.clone()))
                    .build()
            })
    }

    /// A fixed timestamp for markers built by tests.
    fn test_stamp() -> jiff::Timestamp {
        "2026-05-28T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn keys_follow_the_archive_layout() {
        let store = ArchiveStore::new("rio-chunks", "parity");
        assert_eq!(
            store.prefix("0123456789abcdef"),
            "parity/archives/0123456789abcdef/"
        );
        assert_eq!(
            store.object_key("0123456789abcdef", ARCHIVE_COMPLETE_OBJECT),
            "parity/archives/0123456789abcdef/complete.json"
        );

        // An empty root puts the archives tree at the bucket root without a
        // leading slash.
        let store = ArchiveStore::new("rio-chunks", "");
        assert_eq!(
            store.object_key("0123456789abcdef", ARCHIVE_IMAGE_OBJECT),
            "archives/0123456789abcdef/archive.dwarfs"
        );

        // Surrounding slashes on the root are normalized away.
        let store = ArchiveStore::new("rio-chunks", "parity/");
        assert_eq!(
            store.prefix("0123456789abcdef"),
            "parity/archives/0123456789abcdef/"
        );
    }

    #[tokio::test]
    async fn publish_uploads_in_order_and_claims_completeness_conditionally() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // Sequential rules: the write-once existence probe (NotFound), then
        // one PUT per object — image, manifest, complete.json strictly last.
        // Each PUT rule pins the object's key, its Content-Type, and that
        // ONLY the final complete.json PUT is conditional (If-None-Match: *).
        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let puts: Vec<_> = [
            (ARCHIVE_IMAGE_OBJECT, "application/octet-stream", false),
            (ARCHIVE_MANIFEST_OBJECT, "application/json", false),
            (ARCHIVE_COMPLETE_OBJECT, "application/json", true),
        ]
        .iter()
        .map(|&(object, ctype, conditional)| {
            let key = format!("parity/archives/{short}/{object}");
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
        let mut rules: Vec<&Rule> = vec![&head_404];
        rules.extend(puts.iter());
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, rules);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let marker = store
            .publish(&client, &image, &manifest_bytes, "rio-parity/test")
            .await
            .unwrap();

        assert_eq!(marker.archive_id, archive_id);
        assert_eq!(marker.archive_id_short, short);
        assert_eq!(
            marker
                .objects
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT],
            "complete.json lists exactly the image and the standalone manifest"
        );
        assert_eq!(
            marker.objects[ARCHIVE_MANIFEST_OBJECT].sha256, archive_id,
            "the standalone manifest's digest is the archive id"
        );
        assert_eq!(
            marker.objects[ARCHIVE_MANIFEST_OBJECT].size,
            manifest_bytes.len() as u64
        );
        assert_eq!(
            marker.objects[ARCHIVE_IMAGE_OBJECT].size,
            std::fs::metadata(&image).unwrap().len()
        );
        assert_eq!(
            marker.objects[ARCHIVE_IMAGE_OBJECT].sha256,
            identity::sha256_hex(&std::fs::read(&image).unwrap()),
            "the marker's image digest matches the uploaded image bytes"
        );
        assert_eq!(marker.uploader, "rio-parity/test");
        for rule in &puts {
            assert_eq!(rule.num_calls(), 1, "every object uploaded exactly once");
        }
    }

    #[tokio::test]
    async fn publish_refuses_an_already_complete_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());

        let head_200 = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().build());
        let put = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&head_200, &put]);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-parity/test")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("write-once"),
            "expected the write-once refusal, got: {err:#}"
        );
        assert_eq!(put.num_calls(), 0, "no object may be uploaded");
    }

    #[tokio::test]
    async fn publish_maps_a_lost_conditional_put_to_the_write_once_refusal() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());

        // The HEAD pre-check sees no marker and the data uploads succeed,
        // but the final conditional PUT comes back 412: another publisher
        // claimed the prefix in between. The surfaced error must be the same
        // actionable write-once refusal as the pre-check, not the raw SDK
        // error.
        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_data = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(|req| req.if_none_match().is_none())
            .then_output(|| PutObjectOutput::builder().build());
        let put_complete_412 = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(|req| req.if_none_match() == Some("*"))
            .then_error(|| {
                PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
            });
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&head_404, &put_data, &put_complete_412]
        );

        let store = ArchiveStore::new("rio-chunks", "parity");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-parity/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("write-once"),
            "expected the write-once refusal, got: {message}"
        );
        assert!(
            !message.contains("PreconditionFailed"),
            "the raw SDK error must not surface: {message}"
        );
        assert_eq!(put_complete_412.num_calls(), 1);
    }

    #[tokio::test]
    async fn publish_refuses_a_mismatched_manifest() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, mut manifest_bytes) = packed_tiny_archive(dir.path());
        // One extra byte changes the identity, so the standalone manifest no
        // longer matches the manifest member inside the image.
        manifest_bytes.push(b' ');

        let head = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().build());
        let put = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&head, &put]);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-parity/test")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("standalone manifest does not match the image"),
            "got: {err:#}"
        );
        assert_eq!(head.num_calls(), 0, "refused before any S3 request");
        assert_eq!(put.num_calls(), 0, "refused before any S3 request");
    }

    #[tokio::test]
    async fn publish_refuses_a_v0_archive() {
        // v0 archives have no content-addressed identity (no archive_id), so
        // they cannot live in the digest-keyed write-once layout.
        let fixtures = crate::test_manifest_dir().join("tests/fixtures/archive");
        let v0_image = fixtures.join("v0-basic.dwarfs");
        let manifest_bytes = std::fs::read(fixtures.join("v0-basic/manifest.json")).unwrap();

        let head = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().build());
        let put = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&head, &put]);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let err = store
            .publish(&client, &v0_image, &manifest_bytes, "rio-parity/test")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("v0 archives cannot be published"),
            "got: {err:#}"
        );
        assert_eq!(head.num_calls(), 0, "refused before any S3 request");
        assert_eq!(put.num_calls(), 0, "refused before any S3 request");
    }

    #[tokio::test]
    async fn fetch_verifies_digests_and_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // The marker a publish of this archive would have written.
        let marker = CompleteMarker {
            archive_id: archive_id.clone(),
            archive_id_short: short.clone(),
            objects: BTreeMap::from([
                (
                    ARCHIVE_IMAGE_OBJECT.to_string(),
                    MemberDigest {
                        sha256: identity::sha256_hex(&image_bytes),
                        size: image_bytes.len() as u64,
                    },
                ),
                (
                    ARCHIVE_MANIFEST_OBJECT.to_string(),
                    MemberDigest {
                        sha256: archive_id.clone(),
                        size: manifest_bytes.len() as u64,
                    },
                ),
            ]),
            uploaded_at: test_stamp(),
            uploader: "rio-parity/test".to_string(),
        };
        let marker_bytes = serde_json::to_vec(&marker).unwrap();

        let complete_key = format!("parity/archives/{short}/complete.json");
        let image_key = format!("parity/archives/{short}/archive.dwarfs");
        let manifest_key = format!("parity/archives/{short}/manifest.json");

        let get_complete = get_rule(complete_key.clone(), marker_bytes.clone());
        let get_image = get_rule(image_key.clone(), image_bytes.clone());
        let get_manifest = get_rule(manifest_key.clone(), manifest_bytes.clone());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&get_complete, &get_image, &get_manifest]
        );

        let store = ArchiveStore::new("rio-chunks", "parity");
        let dest = dir.path().join("fetched");
        let fetched = store.fetch(&client, &short, &dest).await.unwrap();
        assert_eq!(fetched.image_path, dest.join(ARCHIVE_IMAGE_OBJECT));
        assert_eq!(fetched.manifest_path, dest.join(ARCHIVE_MANIFEST_OBJECT));
        assert_eq!(fetched.marker.archive_id, archive_id);
        assert_eq!(std::fs::read(&fetched.image_path).unwrap(), image_bytes);
        assert_eq!(
            std::fs::read(&fetched.manifest_path).unwrap(),
            manifest_bytes
        );

        // fetch_and_open opens the downloaded image in place and cross-checks
        // its identity against the marker.
        let dest_open = dir.path().join("fetched-open");
        let client_open = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&get_complete, &get_image, &get_manifest]
        );
        let (archive, fetched_open) = store
            .fetch_and_open(&client_open, &short, &dest_open)
            .await
            .unwrap();
        assert_eq!(archive.archive_id(), Some(archive_id.as_str()));
        assert_eq!(archive.requests().len(), 2);
        assert_eq!(fetched_open.marker.archive_id, archive_id);

        // A truncated image download must be rejected with the object named.
        let truncated = image_bytes[..image_bytes.len() / 2].to_vec();
        let get_complete_bad = get_rule(complete_key, marker_bytes.clone());
        let get_image_bad = get_rule(image_key, truncated);
        let get_manifest_bad = get_rule(manifest_key, manifest_bytes.clone());
        let client_bad = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&get_complete_bad, &get_image_bad, &get_manifest_bad]
        );
        let dest_bad = dir.path().join("fetched-truncated");
        let err = store
            .fetch(&client_bad, &short, &dest_bad)
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("download digest mismatch"),
            "got: {message}"
        );
        assert!(message.contains(ARCHIVE_IMAGE_OBJECT), "got: {message}");
    }

    #[tokio::test]
    async fn fetch_refuses_a_marker_listing_a_non_basename_object() {
        // The completion marker is remote input: an object name with a path
        // separator must be refused before any further GET is issued.
        let short = "aaaaaaaaaaaaaaaa";
        let marker = CompleteMarker {
            archive_id: "a".repeat(64),
            archive_id_short: short.to_string(),
            objects: BTreeMap::from([(
                "a/b".to_string(),
                MemberDigest {
                    sha256: "0".repeat(64),
                    size: 1,
                },
            )]),
            uploaded_at: test_stamp(),
            uploader: "rio-parity/test".to_string(),
        };
        let get_complete = get_rule(
            format!("parity/archives/{short}/complete.json"),
            serde_json::to_vec(&marker).unwrap(),
        );
        // Catch-all for any other GET; the guard must keep it at zero calls.
        let get_other = mock!(aws_sdk_s3::Client::get_object)
            .then_output(|| GetObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&get_complete, &get_other]);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let dir = tempfile::TempDir::new().unwrap();
        let err = store
            .fetch(&client, short, &dir.path().join("fetched"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("non-basename"), "got: {err:#}");
        assert_eq!(get_complete.num_calls(), 1);
        assert_eq!(
            get_other.num_calls(),
            0,
            "no further GET may follow the refusal"
        );
    }

    #[tokio::test]
    async fn fetch_refuses_a_short_id_mismatch() {
        // complete.json names a different short id than the prefix it was
        // fetched from: refuse before downloading anything.
        let short = "aaaaaaaaaaaaaaaa";
        let marker = CompleteMarker {
            archive_id: "b".repeat(64),
            archive_id_short: "bbbbbbbbbbbbbbbb".to_string(),
            objects: BTreeMap::new(),
            uploaded_at: test_stamp(),
            uploader: "rio-parity/test".to_string(),
        };
        let get_complete = get_rule(
            format!("parity/archives/{short}/complete.json"),
            serde_json::to_vec(&marker).unwrap(),
        );
        let get_other = mock!(aws_sdk_s3::Client::get_object)
            .then_output(|| GetObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&get_complete, &get_other]);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let dir = tempfile::TempDir::new().unwrap();
        let err = store
            .fetch(&client, short, &dir.path().join("fetched"))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("short id mismatch"),
            "got: {err:#}"
        );
        assert_eq!(
            get_other.num_calls(),
            0,
            "nothing may be downloaded after the refusal"
        );
    }

    #[tokio::test]
    async fn list_returns_only_complete_prefixes() {
        let short_a = "aaaaaaaaaaaaaaaa";
        let short_b = "bbbbbbbbbbbbbbbb";
        let marker_for = |short: &str, fill: char| CompleteMarker {
            archive_id: fill.to_string().repeat(64),
            archive_id_short: short.to_string(),
            objects: BTreeMap::new(),
            uploaded_at: test_stamp(),
            uploader: "rio-parity/test".to_string(),
        };
        let marker_a = serde_json::to_vec(&marker_for(short_a, 'a')).unwrap();
        let marker_b = serde_json::to_vec(&marker_for(short_b, 'b')).unwrap();

        // One page: two complete prefixes (deliberately listed out of order)
        // plus their data objects, and one incomplete prefix that has an
        // image but no complete.json yet.
        let list_page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|req| req.prefix() == Some("parity/archives/"))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .contents(
                        Object::builder()
                            .key(format!("parity/archives/{short_b}/complete.json"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key(format!("parity/archives/{short_b}/archive.dwarfs"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key(format!("parity/archives/{short_a}/complete.json"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key("parity/archives/cccccccccccccccc/archive.dwarfs")
                            .build(),
                    )
                    .build()
            });
        let get_a = get_rule(format!("parity/archives/{short_a}/complete.json"), marker_a);
        let get_b = get_rule(format!("parity/archives/{short_b}/complete.json"), marker_b);
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&list_page, &get_a, &get_b]
        );

        let store = ArchiveStore::new("rio-chunks", "parity");
        let archives = store.list(&client).await.unwrap();
        assert_eq!(archives.len(), 2, "the incomplete prefix is invisible");
        assert_eq!(archives[0].0, short_a);
        assert_eq!(archives[0].1.archive_id, "a".repeat(64));
        assert_eq!(archives[1].0, short_b);
        assert_eq!(archives[1].1.archive_id, "b".repeat(64));
        assert_eq!(get_a.num_calls(), 1);
        assert_eq!(get_b.num_calls(), 1);
    }

    #[tokio::test]
    async fn list_cross_checks_the_marker_against_its_prefix() {
        // A marker that names a different short id than the prefix it lives
        // under is surfaced as an error rather than returned as an entry
        // whose fetch would fail anyway.
        let short = "aaaaaaaaaaaaaaaa";
        let marker = CompleteMarker {
            archive_id: "b".repeat(64),
            archive_id_short: "bbbbbbbbbbbbbbbb".to_string(),
            objects: BTreeMap::new(),
            uploaded_at: test_stamp(),
            uploader: "rio-parity/test".to_string(),
        };
        let list_page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|req| req.prefix() == Some("parity/archives/"))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .contents(
                        Object::builder()
                            .key(format!("parity/archives/{short}/complete.json"))
                            .build(),
                    )
                    .build()
            });
        let get_marker = get_rule(
            format!("parity/archives/{short}/complete.json"),
            serde_json::to_vec(&marker).unwrap(),
        );
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list_page, &get_marker]);

        let store = ArchiveStore::new("rio-chunks", "parity");
        let err = store.list(&client).await.unwrap_err();
        assert!(format!("{err:#}").contains("lives under"), "got: {err:#}");
    }
}
