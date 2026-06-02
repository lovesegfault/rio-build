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
//! the engine never uses it and never lists it as available, and an
//! uploader that finds an object already present refuses to overwrite —
//! unless the at-rest content verifiably equals the bytes it is
//! uploading, which is how a publisher recognizes its own committed PUT
//! after a lost response. This mirrors the eval-set upload discipline in
//! [`crate::s3`] (see `docs/dev/2026-05-28-build-replay-design.md`, "S3
//! layout, write-once upload, completion marker").
//!
//! Lost conditionals are not proof of a foreign publisher. The shared S3
//! client is deliberately at-least-once (see
//! [`rio_common::s3::default_client`]: a raised retry budget with
//! replayable bodies, because S3-compatible backends drop connections
//! mid-request), so a PUT that commits server-side but loses its
//! response is replayed by the SDK and collides with its own object —
//! `412 PreconditionFailed` from the publisher's own write. Every lost
//! conditional is therefore disambiguated by content before any refusal:
//! the data objects by size + SHA-256 (every PUT attaches an
//! `x-amz-checksum-sha256`, so a later HEAD can return the digest), the
//! small control objects by fetching and comparing. Only verified-foreign
//! or unattributable content refuses. AWS S3 and MinIO store and return
//! header-supplied SHA-256 checksums; on a backend that does not, the
//! image conflict is unattributable and the refusal stands — exactly the
//! pre-disambiguation behavior, never worse.
//!
//! Deletion can race publication, and S3 has no cross-key transactions
//! for the two sides to linearize on: `replay delete` sweeps whatever a
//! LIST returned, and a mid-publish prefix carries no marker to exclude
//! it. The race is narrowed, not closed. On this side, publish
//! re-observes every object its marker lists immediately before the
//! conditional marker PUT and refuses ("swept mid-publish — re-run")
//! when one is missing or replaced; the delete sweep, for its part,
//! re-lists until the prefix is empty. The residual window — a sweep
//! passing entirely between those revalidation HEADs and the marker PUT,
//! then finishing before the marker lands — is milliseconds wide, and
//! what it leaves is a marker-only prefix: still listed by the operator
//! tooling (summaries degrade to `?`), still removable by
//! `replay delete`, and rejected loudly by any fetch (every object is
//! digest-verified at download). Every interrupted state stays nameable
//! and converges; none is silently consumed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use base64::Engine as _;
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

/// A published archive, as proven by its parsed `complete.json`.
///
/// `complete.json` is the one completeness predicate of the layout: it is
/// uploaded strictly last, so a prefix without it is an in-flight or
/// interrupted upload (or an interrupted delete's leftovers) — never a
/// published archive. This type is constructible only from a prefix's
/// `complete.json` document, so every consumer that enumerates or resolves
/// archives — the engine and the operator tooling alike — necessarily
/// applies that one predicate; keying archive existence on any other
/// object (e.g. `manifest.json`) is unrepresentable.
#[derive(Debug, Clone)]
pub struct PublishedArchive {
    marker: CompleteMarker,
}

impl PublishedArchive {
    /// Parse a prefix's `complete.json` bytes into the proof of
    /// publication. The marker must be internally consistent: a full
    /// 64-hex lowercase `archive_id` whose first 16 characters equal
    /// `archive_id_short`.
    pub fn from_complete_json(bytes: &[u8]) -> anyhow::Result<Self> {
        let marker: CompleteMarker = serde_json::from_slice(bytes)
            .with_context(|| format!("parse {ARCHIVE_COMPLETE_OBJECT}"))?;
        anyhow::ensure!(
            marker.archive_id.len() == 64
                && marker
                    .archive_id
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{ARCHIVE_COMPLETE_OBJECT} carries a malformed archive_id {:?} (expected 64 \
             lowercase hex characters)",
            marker.archive_id
        );
        anyhow::ensure!(
            marker.archive_id_short == identity::short_id(&marker.archive_id),
            "{ARCHIVE_COMPLETE_OBJECT} is internally inconsistent: archive_id_short {} is not \
             the leading 16 characters of archive_id {} — the marker was hand-edited or \
             corrupted",
            marker.archive_id_short,
            marker.archive_id
        );
        Ok(Self { marker })
    }

    /// [`Self::from_complete_json`], additionally pinning the marker to
    /// the `<archive_id_short>/` prefix segment it was fetched from.
    /// Publishing always writes the marker under its own short id, so a
    /// mismatch means the archive was copied to the wrong prefix or the
    /// marker was edited.
    pub fn from_complete_json_at(bytes: &[u8], archive_id_short: &str) -> anyhow::Result<Self> {
        let published = Self::from_complete_json(bytes)?;
        anyhow::ensure!(
            published.archive_id_short() == archive_id_short,
            "{ARCHIVE_COMPLETE_OBJECT} says archive_id_short {} but it lives under the \
             {archive_id_short}/ prefix; the archive was copied to the wrong prefix or the \
             marker was edited",
            published.archive_id_short(),
        );
        Ok(published)
    }

    /// Full 64-hex archive id (SHA-256 of the manifest.json bytes).
    pub fn archive_id(&self) -> &str {
        &self.marker.archive_id
    }

    /// First 16 hex characters of the archive id — the S3 prefix segment.
    pub fn archive_id_short(&self) -> &str {
        &self.marker.archive_id_short
    }

    /// The parsed completion marker.
    pub fn marker(&self) -> &CompleteMarker {
        &self.marker
    }

    /// Consume the proof, keeping the marker document.
    pub fn into_marker(self) -> CompleteMarker {
        self.marker
    }
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

/// S3 destination (bucket + deployment root prefix, e.g. "replay") for
/// replay archives.
#[derive(Debug, Clone)]
pub struct ArchiveStore {
    pub bucket: String,
    pub root: String,
}

impl ArchiveStore {
    /// Bucket + deployment root prefix (e.g. `replay`). Surrounding slashes
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

    /// Classify a lost `If-None-Match: *` PUT on a DATA object whose
    /// at-rest content is NOT this publish's bytes (`detail` says how the
    /// attribution failed — content mismatch or no checksum to compare).
    /// With `complete.json` present the prefix is a published archive and
    /// another publisher simply won: the standard write-once refusal.
    /// Without it the prefix holds partial objects from an interrupted
    /// (or still-running) publish, which write-once refuses to overwrite
    /// — a DISTINCT error naming the recovery path, since "already
    /// published" would be a lie and the retry can never succeed in
    /// place.
    async fn data_object_conflict(
        &self,
        client: &aws_sdk_s3::Client,
        archive_id_short: &str,
        object: &str,
        detail: &str,
    ) -> anyhow::Error {
        match self.is_complete(client, archive_id_short).await {
            Ok(true) => anyhow::anyhow!(
                "{} — and the existing {object} is not this publish's upload ({detail})",
                self.write_once_refusal(archive_id_short)
            ),
            Ok(false) => anyhow::anyhow!(
                "archive prefix s3://{}/{} has partial objects from an interrupted publish \
                 ({object} exists but {ARCHIVE_COMPLETE_OBJECT} does not, and the existing \
                 {object} is not attributable to this publish: {detail}), and archive objects \
                 are write-once — if no other publisher is currently uploading this prefix, run \
                 `cargo xtask replay delete {archive_id_short}` (its sweep tolerates the missing \
                 marker), then retry",
                self.bucket,
                self.prefix(archive_id_short)
            ),
            Err(probe) => probe.context(format!(
                "PUT s3://{}/{} lost its write-once conditional (the object already exists), and \
                 probing {ARCHIVE_COMPLETE_OBJECT} to classify the conflict failed too",
                self.bucket,
                self.object_key(archive_id_short, object)
            )),
        }
    }

    /// The refusal raised when an object this publish already verified (or
    /// uploaded) vanished from the prefix before the upload could finish:
    /// a concurrent `replay delete` swept the prefix mid-publish. Nothing
    /// of this publish remains claimed, so the recovery is simply to
    /// re-run it.
    fn swept_mid_publish(&self, archive_id_short: &str, object: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "archive prefix s3://{}/{} was swept mid-publish ({object} is gone — a concurrent \
             `replay delete` raced this upload); nothing of this publish remains claimed — \
             re-run the publish",
            self.bucket,
            self.prefix(archive_id_short)
        )
    }

    /// HEAD `key` and compare what is at rest against the digest this
    /// publish has in hand for it. The comparison is by size plus the
    /// stored SHA-256 checksum (`x-amz-checksum-sha256`, returned because
    /// every publish PUT attaches one); a backend that returns no checksum
    /// leaves a size-matching object unverifiable
    /// ([`RemoteObjectMatch::Unverifiable`]) — present, but not
    /// attributable to anyone.
    async fn remote_object_match(
        &self,
        client: &aws_sdk_s3::Client,
        key: &str,
        expected: &MemberDigest,
    ) -> anyhow::Result<RemoteObjectMatch> {
        let head = match client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
        {
            Ok(head) => head,
            Err(SdkError::ServiceError(err)) if err.err().is_not_found() => {
                return Ok(RemoteObjectMatch::Absent);
            }
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("HEAD s3://{}/{key}", self.bucket))
                );
            }
        };
        let Some(size) = head.content_length() else {
            return Ok(RemoteObjectMatch::Unverifiable(
                "the backend returned no Content-Length to compare".to_string(),
            ));
        };
        if size != expected.size as i64 {
            return Ok(RemoteObjectMatch::Foreign(format!(
                "{size} bytes at rest vs {} bytes staged",
                expected.size
            )));
        }
        match head.checksum_sha256() {
            Some(stored) => match base64::engine::general_purpose::STANDARD.decode(stored) {
                Ok(raw) => {
                    let at_rest = hex::encode(raw);
                    if at_rest == expected.sha256 {
                        Ok(RemoteObjectMatch::Identical)
                    } else {
                        Ok(RemoteObjectMatch::Foreign(format!(
                            "SHA-256 {at_rest} at rest vs {} staged",
                            expected.sha256
                        )))
                    }
                }
                // A multipart upload's composite checksum ("<base64>-N")
                // or other undecodable form cannot be ours (publish PUTs
                // are single-part), but without a comparable digest the
                // honest classification is unattributable, not foreign.
                Err(_) => Ok(RemoteObjectMatch::Unverifiable(format!(
                    "the backend returned an uncomparable SHA-256 checksum {stored:?}"
                ))),
            },
            None => Ok(RemoteObjectMatch::Unverifiable(format!(
                "the existing object matches by size ({size} bytes) but the backend returned no \
                 SHA-256 checksum to attribute it"
            ))),
        }
    }

    /// GET one small control object's bytes; `Ok(None)` when the key does
    /// not exist (a concurrent delete removed it).
    async fn get_object_bytes(
        &self,
        client: &aws_sdk_s3::Client,
        key: &str,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let resp = match client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                return Ok(None);
            }
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("GET s3://{}/{key}", self.bucket))
                );
            }
        };
        let bytes = resp
            .body
            .collect()
            .await
            .with_context(|| format!("read s3://{}/{key}", self.bucket))?
            .to_vec();
        Ok(Some(bytes))
    }

    /// Publish a packed archive: upload `archive.dwarfs`, the standalone
    /// `manifest.json` (the exact bytes passed in), and finally
    /// `complete.json` strictly last — after re-HEADing every object the
    /// marker lists, so completeness is never claimed over state a
    /// concurrent `replay delete` swept mid-upload. EVERY PUT carries
    /// `If-None-Match: *`: the marker so two racing uploads cannot both
    /// claim completeness, and the data objects so a racing publisher of
    /// the same archive id (same manifest bytes, but mkdwarfs packing is
    /// not deterministic, so different image bytes) can never overwrite an
    /// object that already landed — a winner's published image stays
    /// exactly the bytes its marker hashes.
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
    /// at the prefix — the prefix never looks complete. Every PUT also
    /// attaches the object's SHA-256 as an `x-amz-checksum-sha256` header
    /// (the backend verifies the body against it and stores it), so a
    /// lost conditional can be disambiguated by content instead of being
    /// read as proof of a foreign publisher: the SDK retries a PUT whose
    /// response was lost, and the replay collides with the publisher's
    /// own committed object. On a lost conditional the existing object is
    /// compared against the bytes this publish is uploading — the image
    /// by HEAD size + stored checksum, the standalone manifest by
    /// fetching and re-hashing (its digest IS the archive id), the marker
    /// by fetching and comparing identity fields — and a verified match
    /// continues (or returns the existing marker) instead of refusing.
    /// This also lets a re-run of an interrupted publish of byte-identical
    /// inputs resume in place. Only verified-foreign or unattributable
    /// content is refused, with a recovery path (delete the partial
    /// prefix, then retry) instead of silently overwriting objects
    /// another publisher may be mid-claiming.
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

        // Data first: the image, then the standalone manifest — each PUT
        // write-once, so nothing already at the prefix (a racing or
        // interrupted publisher's objects) is ever overwritten.
        let image_key = self.object_key(&archive_id_short, ARCHIVE_IMAGE_OBJECT);
        let body = ByteStream::from_path(image_path)
            .await
            .with_context(|| format!("read {}", image_path.display()))?;
        match client
            .put_object()
            .bucket(&self.bucket)
            .key(&image_key)
            .content_type("application/octet-stream")
            .if_none_match("*")
            .checksum_sha256(sha256_base64(&image_digest.sha256)?)
            .body(body)
            .send()
            .await
        {
            Ok(_) => {}
            Err(e) if put_lost_conditional(&e) => {
                // The image is multi-gigabyte, so the self-write check is
                // a HEAD against the stored checksum, never a re-download.
                match self
                    .remote_object_match(client, &image_key, &image_digest)
                    .await?
                {
                    RemoteObjectMatch::Identical => {
                        tracing::info!(
                            key = %image_key,
                            "lost the write-once conditional to an object holding exactly the \
                             bytes being uploaded (a replayed PUT whose response was lost \
                             collides with itself); the claim is won"
                        );
                    }
                    RemoteObjectMatch::Absent => {
                        return Err(self.swept_mid_publish(&archive_id_short, ARCHIVE_IMAGE_OBJECT));
                    }
                    RemoteObjectMatch::Foreign(detail)
                    | RemoteObjectMatch::Unverifiable(detail) => {
                        return Err(self
                            .data_object_conflict(
                                client,
                                &archive_id_short,
                                ARCHIVE_IMAGE_OBJECT,
                                &detail,
                            )
                            .await);
                    }
                }
            }
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("PUT s3://{}/{image_key}", self.bucket))
                );
            }
        }
        tracing::info!(key = %image_key, "uploaded");

        let manifest_key = self.object_key(&archive_id_short, ARCHIVE_MANIFEST_OBJECT);
        match client
            .put_object()
            .bucket(&self.bucket)
            .key(&manifest_key)
            .content_type("application/json")
            .if_none_match("*")
            .checksum_sha256(sha256_base64(&archive_id)?)
            .body(ByteStream::from(manifest_bytes.to_vec()))
            .send()
            .await
        {
            Ok(_) => {}
            Err(e) if put_lost_conditional(&e) => {
                // The standalone manifest is small and IS the identity
                // bytes: fetch it and re-hash — an at-rest copy that
                // reproduces the archive id is byte-identical to what this
                // publish is uploading, no checksum substrate needed.
                match self.get_object_bytes(client, &manifest_key).await? {
                    Some(bytes)
                        if identity::archive_id_from_manifest_bytes(&bytes) == archive_id =>
                    {
                        tracing::info!(
                            key = %manifest_key,
                            "lost the write-once conditional to a byte-identical manifest \
                             (a replayed PUT whose response was lost collides with itself); \
                             the claim is won"
                        );
                    }
                    Some(bytes) => {
                        let detail = format!(
                            "SHA-256 {} at rest vs {archive_id} staged",
                            identity::archive_id_from_manifest_bytes(&bytes)
                        );
                        return Err(self
                            .data_object_conflict(
                                client,
                                &archive_id_short,
                                ARCHIVE_MANIFEST_OBJECT,
                                &detail,
                            )
                            .await);
                    }
                    None => {
                        return Err(
                            self.swept_mid_publish(&archive_id_short, ARCHIVE_MANIFEST_OBJECT)
                        );
                    }
                }
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("PUT s3://{}/{manifest_key}", self.bucket)));
            }
        }
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

        // Re-observe every object the marker is about to vouch for: a
        // concurrent `replay delete` may have swept the prefix while the
        // uploads above were in flight (its sweep removes whatever a LIST
        // returned, and a mid-publish prefix has no marker to exclude it).
        // The marker must never claim completeness over unobserved state,
        // so each object it lists is re-HEADed immediately before the
        // conditional marker PUT — the HEAD set is derived from the marker
        // itself, so an object the marker lists can never escape this.
        // Existence (and size) is the load-bearing check; the stored
        // checksum hardens it where the backend returns one. Unverifiable
        // passes here — unlike in the lost-conditional arms — because this
        // publish itself just uploaded or digest-verified these objects;
        // the fetch path re-verifies every digest at consume time either
        // way. See the module doc for the residual race this narrows but
        // cannot close.
        for (object, expected) in &marker.objects {
            let key = self.object_key(&archive_id_short, object);
            match self.remote_object_match(client, &key, expected).await? {
                RemoteObjectMatch::Identical | RemoteObjectMatch::Unverifiable(_) => {}
                RemoteObjectMatch::Absent => {
                    return Err(self.swept_mid_publish(&archive_id_short, object));
                }
                RemoteObjectMatch::Foreign(detail) => anyhow::bail!(
                    "archive prefix s3://{}/{} was swept and re-claimed mid-publish: the \
                     {object} at rest is not the one this publish uploaded ({detail}) — \
                     re-run the publish and let the write-once conditionals settle the claim",
                    self.bucket,
                    self.prefix(&archive_id_short)
                ),
            }
        }

        let mut marker_bytes =
            serde_json::to_vec_pretty(&marker).context("serialize complete.json")?;
        marker_bytes.push(b'\n');
        let marker_sha256 = hex::encode(sha2::Sha256::digest(&marker_bytes));
        let complete_key = self.object_key(&archive_id_short, ARCHIVE_COMPLETE_OBJECT);
        match client
            .put_object()
            .bucket(&self.bucket)
            .key(&complete_key)
            .content_type("application/json")
            .if_none_match("*")
            .checksum_sha256(sha256_base64(&marker_sha256)?)
            .body(ByteStream::from(marker_bytes))
            .send()
            .await
        {
            Ok(_) => {}
            // The conditional PUT lost: a marker is already in place. That
            // marker is this publish's own when its identity fields match
            // (the SDK replays a PUT whose response was lost, colliding
            // with its own claim) — fetch and compare before refusing.
            Err(e) if put_lost_conditional(&e) => {
                match self.get_object_bytes(client, &complete_key).await? {
                    Some(bytes) => {
                        match PublishedArchive::from_complete_json_at(&bytes, &archive_id_short) {
                            Ok(existing) if marker_is_same_publish(existing.marker(), &marker) => {
                                tracing::info!(
                                    key = %complete_key,
                                    archive_id = %existing.archive_id(),
                                    "lost the completeness conditional to a marker vouching for \
                                     exactly this publish's objects; the claim is won — archive \
                                     published"
                                );
                                return Ok(existing.into_marker());
                            }
                            // A parseable foreign marker (different image
                            // bytes) claimed the prefix between the HEAD
                            // pre-check and this final upload: the prefix
                            // is a published archive and another publisher
                            // won. Surface the same actionable refusal as
                            // the pre-check.
                            Ok(_) => anyhow::bail!(self.write_once_refusal(&archive_id_short)),
                            // An unparseable marker claimed it (an
                            // out-of-band write at the marker key).
                            // [`PublishedArchive`] is the single candidacy
                            // predicate — fetch, launch, and list all fail
                            // on these same bytes — so "already published,
                            // re-record under a new id" would be false on
                            // both counts. Name the converging recovery the
                            // partial-prefix data-object arm names: sweep
                            // the junk, then retry THIS publish.
                            Err(parse_err) => anyhow::bail!(
                                "the conditional PUT of {ARCHIVE_COMPLETE_OBJECT} at s3://{}/{} \
                                 lost to an existing marker that does not parse as a completion \
                                 proof ({parse_err:#}); nothing at this prefix is retrievably \
                                 published — if no other publisher is currently uploading it, \
                                 run `cargo xtask replay delete {archive_id_short}` to sweep the \
                                 junk marker, then retry the publish",
                                self.bucket,
                                self.prefix(&archive_id_short),
                            ),
                        }
                    }
                    // The marker the conditional collided with is already
                    // gone again: a concurrent delete is sweeping the
                    // prefix.
                    None => {
                        return Err(
                            self.swept_mid_publish(&archive_id_short, ARCHIVE_COMPLETE_OBJECT)
                        );
                    }
                }
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
        let marker = PublishedArchive::from_complete_json_at(&marker_bytes, archive_id_short)
            .with_context(|| format!("s3://{}/{complete_key}", self.bucket))?
            .into_marker();

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
    /// or in-flight uploads are invisible. A marker that vanishes between
    /// the LIST and its GET is skipped, not an error: deletion removes
    /// `complete.json` strictly first, so the race means the prefix was
    /// just unpublished. Marker *content* is strict: each marker must
    /// parse and its `archive_id_short` is cross-checked against the
    /// prefix segment it was found under — this engine-facing listing
    /// refuses junk loudly, while the operator tooling's enumeration
    /// (`xtask replay list`) is the surface that degrades junk to
    /// renderable, deletable rows. Returns [`PublishedArchive`]s sorted
    /// by short id.
    pub async fn list(&self, client: &aws_sdk_s3::Client) -> anyhow::Result<Vec<PublishedArchive>> {
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
            let resp = match client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(resp) => resp,
                // Deleted between the LIST and this GET: a delete removes
                // complete.json strictly first, so the prefix was just
                // unpublished — skip it, exactly as `get_object_bytes`
                // maps the same race to `Ok(None)`.
                Err(err) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                    tracing::debug!(
                        "skipping {short}: {ARCHIVE_COMPLETE_OBJECT} deleted between LIST and GET"
                    );
                    continue;
                }
                Err(e) => {
                    return Err(
                        anyhow::Error::new(e).context(format!("GET s3://{}/{key}", self.bucket))
                    );
                }
            };
            let bytes = resp
                .body
                .collect()
                .await
                .with_context(|| format!("read s3://{}/{key}", self.bucket))?
                .into_bytes();
            // The prefix cross-check (marker says the short id it lives
            // under) is the constructor's: a hand-copied prefix or an
            // edited marker surfaces here instead of as an entry whose
            // fetch would fail anyway.
            let published = PublishedArchive::from_complete_json_at(&bytes, &short)
                .with_context(|| format!("s3://{}/{key}", self.bucket))?;
            archives.push(published);
        }
        archives.sort_by(|a, b| a.archive_id_short().cmp(b.archive_id_short()));
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

/// How an at-rest object compares against the digest a publish has in hand
/// for it — the verdict of the self-write check after a lost conditional.
/// Only [`Self::Identical`] may continue a publish: the completion marker
/// must never vouch for content that was not verified.
#[derive(Debug)]
enum RemoteObjectMatch {
    /// Size and stored SHA-256 both match: the object holds exactly the
    /// bytes this publish is uploading (its own committed PUT after a lost
    /// response, or a byte-identical re-run resuming in place).
    Identical,
    /// The object verifiably differs (size or checksum) — a foreign
    /// publisher's bytes. The detail names the mismatch.
    Foreign(String),
    /// The object exists but the backend returned nothing that can
    /// attribute it (no comparable checksum, or no size). Refused like
    /// foreign content: unverified bytes must never be vouched for.
    Unverifiable(String),
    /// No object at the key.
    Absent,
}

/// Did this PUT lose its `If-None-Match: *` conditional write? Covers both
/// outcomes AWS documents for conditional writes: HTTP 412
/// `PreconditionFailed` (the object already exists) and HTTP 409
/// `ConditionalRequestConflict` (a concurrent conditional operation on the
/// same key was in flight — the winner may or may not have materialized
/// yet, which the caller's content probe distinguishes). Generic over the
/// response type so we don't have to name aws-smithy-runtime-api types
/// (not a direct dependency).
fn put_lost_conditional<R>(
    err: &SdkError<aws_sdk_s3::operation::put_object::PutObjectError, R>,
) -> bool {
    use aws_sdk_s3::error::ProvideErrorMetadata as _;

    match err {
        SdkError::ServiceError(se) => matches!(
            se.err().code(),
            Some("PreconditionFailed") | Some("ConditionalRequestConflict")
        ),
        _ => false,
    }
}

/// Are two completion markers claims of the same publish? Compares the
/// identity fields — archive id, the full per-object digest map (image and
/// manifest SHA-256 + size), and the uploader — and deliberately ignores
/// `uploaded_at`: it is the only field two attempts of the same logical
/// publish can differ in, and a marker matching on everything else vouches
/// for exactly the objects this publish verified.
fn marker_is_same_publish(existing: &CompleteMarker, ours: &CompleteMarker) -> bool {
    existing.archive_id == ours.archive_id
        && existing.objects == ours.objects
        && existing.uploader == ours.uploader
}

/// A lowercase-hex SHA-256 digest re-encoded as the standard base64 the
/// `x-amz-checksum-sha256` header carries.
fn sha256_base64(hex_digest: &str) -> anyhow::Result<String> {
    let raw = hex::decode(hex_digest).with_context(|| {
        format!("re-encode SHA-256 digest {hex_digest:?} for the checksum header")
    })?;
    Ok(base64::engine::general_purpose::STANDARD.encode(raw))
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

    /// A HEAD rule for exactly the object at `key`, answering with the
    /// stored checksum and size a publish PUT recorded — what the
    /// self-write check and the pre-marker revalidation read back.
    fn head_object_rule(key: String, sha256_hex: &str, size: u64) -> Rule {
        let stored = sha256_base64(sha256_hex).unwrap();
        mock!(aws_sdk_s3::Client::head_object)
            .match_requests(move |req| req.key() == Some(key.as_str()))
            .then_output(move || {
                HeadObjectOutput::builder()
                    .content_length(size as i64)
                    .checksum_sha256(stored.clone())
                    .build()
            })
    }

    /// A fixed timestamp for markers built by tests.
    fn test_stamp() -> jiff::Timestamp {
        "2026-05-28T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn keys_follow_the_archive_layout() {
        let store = ArchiveStore::new("rio-chunks", "replay");
        assert_eq!(
            store.prefix("0123456789abcdef"),
            "replay/archives/0123456789abcdef/"
        );
        assert_eq!(
            store.object_key("0123456789abcdef", ARCHIVE_COMPLETE_OBJECT),
            "replay/archives/0123456789abcdef/complete.json"
        );

        // An empty root puts the archives tree at the bucket root without a
        // leading slash.
        let store = ArchiveStore::new("rio-chunks", "");
        assert_eq!(
            store.object_key("0123456789abcdef", ARCHIVE_IMAGE_OBJECT),
            "archives/0123456789abcdef/archive.dwarfs"
        );

        // Surrounding slashes on the root are normalized away.
        let store = ArchiveStore::new("rio-chunks", "replay/");
        assert_eq!(
            store.prefix("0123456789abcdef"),
            "replay/archives/0123456789abcdef/"
        );
    }

    #[tokio::test]
    async fn publish_uploads_in_order_and_every_put_is_write_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // Sequential rules: the write-once existence probe (NotFound), then
        // one PUT per object — image, manifest, complete.json strictly last.
        // Each PUT rule pins the object's key, its Content-Type, that EVERY
        // PUT is conditional (If-None-Match: *) — the data objects so a
        // racing publisher can never overwrite a landed object, the marker
        // so only one publisher can claim completeness — and that every PUT
        // attaches an x-amz-checksum-sha256 (the substrate the lost-
        // conditional self-write check compares against).
        let image_checksum =
            sha256_base64(&identity::sha256_hex(&std::fs::read(&image).unwrap())).unwrap();
        let manifest_checksum = sha256_base64(&archive_id).unwrap();
        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let puts: Vec<_> = [
            (
                ARCHIVE_IMAGE_OBJECT,
                "application/octet-stream",
                Some(image_checksum.clone()),
            ),
            (
                ARCHIVE_MANIFEST_OBJECT,
                "application/json",
                Some(manifest_checksum.clone()),
            ),
            // The marker's bytes (and so its digest) embed the upload
            // timestamp; pin only that a checksum is attached.
            (ARCHIVE_COMPLETE_OBJECT, "application/json", None),
        ]
        .into_iter()
        .map(|(object, ctype, checksum)| {
            let key = format!("replay/archives/{short}/{object}");
            mock!(aws_sdk_s3::Client::put_object)
                .match_requests(move |req| {
                    req.key() == Some(key.as_str())
                        && req.content_type() == Some(ctype)
                        && req.if_none_match() == Some("*")
                        && match &checksum {
                            Some(expected) => req.checksum_sha256() == Some(expected.as_str()),
                            None => req.checksum_sha256().is_some(),
                        }
                })
                .then_output(|| PutObjectOutput::builder().build())
        })
        .collect();
        // Between the data uploads and the marker PUT, publish re-HEADs
        // every object the marker lists (completeness is never claimed
        // over unobserved state); one rule per object, pinned to the key
        // and to checksum-mode retrieval, answering with the digests the
        // uploads stored.
        let revalidations: Vec<_> = [
            (
                ARCHIVE_IMAGE_OBJECT,
                image_checksum.clone(),
                std::fs::metadata(&image).unwrap().len() as i64,
            ),
            (
                ARCHIVE_MANIFEST_OBJECT,
                manifest_checksum.clone(),
                manifest_bytes.len() as i64,
            ),
        ]
        .into_iter()
        .map(|(object, stored, size)| {
            let key = format!("replay/archives/{short}/{object}");
            mock!(aws_sdk_s3::Client::head_object)
                .match_requests(move |req| {
                    req.key() == Some(key.as_str())
                        && req.checksum_mode() == Some(&aws_sdk_s3::types::ChecksumMode::Enabled)
                })
                .then_output(move || {
                    HeadObjectOutput::builder()
                        .content_length(size)
                        .checksum_sha256(stored.clone())
                        .build()
                })
        })
        .collect();
        let mut rules: Vec<&Rule> = vec![&head_404, &puts[0], &puts[1]];
        rules.extend(revalidations.iter());
        rules.push(&puts[2]);
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, rules);

        let store = ArchiveStore::new("rio-chunks", "replay");
        let marker = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap();
        for revalidation in &revalidations {
            assert_eq!(
                revalidation.num_calls(),
                1,
                "every object the marker lists is re-observed before the marker PUT"
            );
        }

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
        assert_eq!(marker.uploader, "rio-replay/test");
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

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
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
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // The HEAD pre-check sees no marker and the data uploads succeed,
        // but the final conditional PUT comes back 412: another publisher
        // of the same archive id claimed the prefix in between, and the
        // self-write probe finds ITS marker (same identity bytes, but
        // mkdwarfs is not deterministic, so a different image digest). The
        // surfaced error must be the same actionable write-once refusal as
        // the pre-check, not the raw SDK error.
        let foreign_marker = CompleteMarker {
            archive_id: archive_id.clone(),
            archive_id_short: short.clone(),
            objects: BTreeMap::from([
                (
                    ARCHIVE_IMAGE_OBJECT.to_string(),
                    MemberDigest {
                        sha256: "f0".repeat(32),
                        size: 4096,
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
            uploader: "rio-replay/test".to_string(),
        };
        let complete_suffix = format!("/{ARCHIVE_COMPLETE_OBJECT}");
        let head_marker_suffix = complete_suffix.clone();
        let head_marker_404 = mock!(aws_sdk_s3::Client::head_object)
            .match_requests(move |req| req.key().is_some_and(|k| k.ends_with(&head_marker_suffix)))
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        // The pre-marker revalidation HEADs find the data objects in place
        // (a bare 200 — existence is what it needs).
        let head_data_suffix = complete_suffix.clone();
        let head_data_present = mock!(aws_sdk_s3::Client::head_object)
            .match_requests(move |req| !req.key().is_some_and(|k| k.ends_with(&head_data_suffix)))
            .then_output(|| HeadObjectOutput::builder().build());
        let data_suffix = complete_suffix.clone();
        let put_data = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(move |req| !req.key().is_some_and(|k| k.ends_with(&data_suffix)))
            .then_output(|| PutObjectOutput::builder().build());
        let put_complete_412 = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(move |req| req.key().is_some_and(|k| k.ends_with(&complete_suffix)))
            .then_error(|| {
                PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
            });
        let get_foreign_marker = get_rule(
            format!("replay/archives/{short}/{ARCHIVE_COMPLETE_OBJECT}"),
            serde_json::to_vec(&foreign_marker).unwrap(),
        );
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[
                &head_marker_404,
                &head_data_present,
                &put_data,
                &put_complete_412,
                &get_foreign_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
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
        assert_eq!(
            get_foreign_marker.num_calls(),
            1,
            "the lost marker conditional must probe the existing marker before refusing"
        );
    }

    #[tokio::test]
    async fn publish_names_delete_then_retry_for_an_unparseable_marker_conflict() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // Same race as above, but the marker the conditional collided with
        // does not parse as a completion proof (an out-of-band write at the
        // marker key). PublishedArchive is the single candidacy predicate —
        // fetch, launch, and list all fail on these same bytes — so the
        // "already published … only a re-record needs a new prefix" refusal
        // would be false on both counts: nothing is retrievably published,
        // and a retry of THIS publish converges once the junk is removed.
        // The refusal must name the converging recovery the partial-prefix
        // data-object arm already names: `replay delete <short>`, then
        // retry.
        let complete_suffix = format!("/{ARCHIVE_COMPLETE_OBJECT}");
        let head_marker_suffix = complete_suffix.clone();
        let head_marker_404 = mock!(aws_sdk_s3::Client::head_object)
            .match_requests(move |req| req.key().is_some_and(|k| k.ends_with(&head_marker_suffix)))
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let head_data_suffix = complete_suffix.clone();
        let head_data_present = mock!(aws_sdk_s3::Client::head_object)
            .match_requests(move |req| !req.key().is_some_and(|k| k.ends_with(&head_data_suffix)))
            .then_output(|| HeadObjectOutput::builder().build());
        let data_suffix = complete_suffix.clone();
        let put_data = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(move |req| !req.key().is_some_and(|k| k.ends_with(&data_suffix)))
            .then_output(|| PutObjectOutput::builder().build());
        let put_complete_412 = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(move |req| req.key().is_some_and(|k| k.ends_with(&complete_suffix)))
            .then_error(|| {
                PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
            });
        let get_junk_marker = get_rule(
            format!("replay/archives/{short}/{ARCHIVE_COMPLETE_OBJECT}"),
            b"<html>502 Bad Gateway</html>".to_vec(),
        );
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[
                &head_marker_404,
                &head_data_present,
                &put_data,
                &put_complete_412,
                &get_junk_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(&format!("replay delete {short}")) && message.contains("retry"),
            "the refusal names the delete-then-retry recovery: {message}"
        );
        assert!(
            !message.contains("already published"),
            "an unparseable marker publishes nothing — the claim would be false: {message}"
        );
        assert!(
            !message.contains("PreconditionFailed"),
            "the raw SDK error must not surface: {message}"
        );
        assert_eq!(get_junk_marker.num_calls(), 1);
    }

    #[tokio::test]
    async fn racing_publishers_leave_one_winner_and_a_lost_data_put_backs_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());

        // Loser's view of a race it lost AFTER the winner finished: the
        // HEAD pre-check raced past (404), the conditional image PUT comes
        // back 412, the self-write probe finds an image of a DIFFERENT size
        // (mkdwarfs is not deterministic — the winner's bytes), and the
        // classifying probe finds complete.json present — the winner's
        // claim stands, nothing of the winner's prefix was overwritten, and
        // the loser gets the standard write-once refusal.
        let head_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let head_image_foreign = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().content_length(1).build());
        let head_200 = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&head_404, &put_image_412, &head_image_foreign, &head_200]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("write-once"),
            "a complete prefix maps to the write-once refusal: {message}"
        );
        assert!(
            message.contains("not this publish's upload"),
            "the refusal names the failed attribution: {message}"
        );
        assert_eq!(
            put_image_412.num_calls(),
            1,
            "the loser stops after its first lost PUT — no overwrite, no further uploads"
        );
    }

    #[tokio::test]
    async fn lost_data_put_without_a_marker_names_the_partial_prefix_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // The image PUT loses its conditional while complete.json is ABSENT:
        // these are partial objects from an interrupted publish (or a
        // publisher still mid-upload), NOT a published archive — claiming
        // "already published" would be a lie and an in-place retry can never
        // succeed. The self-write probe sees a different-sized image (not
        // ours), so the error must name the real state and the recovery path
        // (replay delete, whose sweep tolerates the missing marker; then
        // retry).
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let head_image_foreign = mock!(aws_sdk_s3::Client::head_object)
            .then_output(|| HeadObjectOutput::builder().content_length(1).build());
        let head_404_classify = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_image_412,
                &head_image_foreign,
                &head_404_classify
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("partial objects from an interrupted publish"),
            "got: {message}"
        );
        assert!(
            message.contains(&format!("replay delete {short}")),
            "the recovery path must name the delete command: {message}"
        );
        assert!(
            !message.contains("already published"),
            "a partial prefix must not be reported as already published: {message}"
        );
        assert_eq!(put_image_412.num_calls(), 1);
    }

    #[tokio::test]
    async fn publish_survives_a_replayed_image_put_colliding_with_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);
        let image_sha256 = identity::sha256_hex(&image_bytes);

        // The at-least-once interleaving the publish client is deliberately
        // configured for (raised retry budget, replayable bodies): the image
        // PUT commits server-side but its response is lost (a transient
        // 500), the SDK replays the PUT, and the replay collides with the
        // publisher's OWN object — 412 from itself. The self-write check
        // (HEAD with checksum mode, stored SHA-256 equals the digest in
        // hand) must recognize the claim as won and let the publish finish.
        let image_key = format!("replay/archives/{short}/{ARCHIVE_IMAGE_OBJECT}");
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_lost_then_412 = {
            let key = image_key.clone();
            mock!(aws_sdk_s3::Client::put_object)
                .match_requests(move |req| req.key() == Some(key.as_str()))
                .sequence()
                .http_status(500, None)
                .error(|| {
                    PutObjectError::generic(
                        ErrorMetadata::builder().code("PreconditionFailed").build(),
                    )
                })
                .build()
        };
        let head_image_ours = {
            let key = image_key.clone();
            let stored = sha256_base64(&image_sha256).unwrap();
            let size = image_bytes.len() as i64;
            mock!(aws_sdk_s3::Client::head_object)
                .match_requests(move |req| {
                    req.key() == Some(key.as_str())
                        && req.checksum_mode() == Some(&aws_sdk_s3::types::ChecksumMode::Enabled)
                })
                .then_output(move || {
                    HeadObjectOutput::builder()
                        .content_length(size)
                        .checksum_sha256(stored.clone())
                        .build()
                })
        };
        let put_manifest = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let reval_image =
            head_object_rule(image_key.clone(), &image_sha256, image_bytes.len() as u64);
        let reval_manifest = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_MANIFEST_OBJECT}"),
            &archive_id,
            manifest_bytes.len() as u64,
        );
        let put_marker = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_image_lost_then_412,
                &head_image_ours,
                &put_manifest,
                &reval_image,
                &reval_manifest,
                &put_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let marker = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap();
        assert_eq!(
            put_image_lost_then_412.num_calls(),
            2,
            "the SDK must have replayed the lost PUT before colliding with itself"
        );
        assert_eq!(marker.archive_id, archive_id);
        assert_eq!(marker.objects[ARCHIVE_IMAGE_OBJECT].sha256, image_sha256);
        assert_eq!(
            put_marker.num_calls(),
            1,
            "the marker still uploads after the rescued image claim"
        );
    }

    #[tokio::test]
    async fn publish_resumes_in_place_when_lost_data_puts_find_identical_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // A re-run over a crashed publish of byte-identical inputs (e.g.
        // `replay launch --archive` retried with the same image file): both
        // data PUTs lose their conditionals to objects that hold exactly
        // the bytes being uploaded — the image verified by HEAD against the
        // stored checksum, the manifest by fetching and re-hashing (its
        // digest IS the archive id). The publish resumes in place and only
        // the marker is newly claimed.
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let head_image_ours = {
            let stored = sha256_base64(&identity::sha256_hex(&image_bytes)).unwrap();
            let size = image_bytes.len() as i64;
            mock!(aws_sdk_s3::Client::head_object).then_output(move || {
                HeadObjectOutput::builder()
                    .content_length(size)
                    .checksum_sha256(stored.clone())
                    .build()
            })
        };
        let put_manifest_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let get_manifest_ours = get_rule(
            format!("replay/archives/{short}/{ARCHIVE_MANIFEST_OBJECT}"),
            manifest_bytes.clone(),
        );
        let reval_image = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_IMAGE_OBJECT}"),
            &identity::sha256_hex(&image_bytes),
            image_bytes.len() as u64,
        );
        let reval_manifest = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_MANIFEST_OBJECT}"),
            &archive_id,
            manifest_bytes.len() as u64,
        );
        let put_marker = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_image_412,
                &head_image_ours,
                &put_manifest_412,
                &get_manifest_ours,
                &reval_image,
                &reval_manifest,
                &put_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let marker = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap();
        assert_eq!(marker.archive_id, archive_id);
        assert_eq!(put_marker.num_calls(), 1, "the marker is claimed fresh");
    }

    #[tokio::test]
    async fn publish_returns_the_existing_marker_when_the_lost_marker_put_matches_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // The marker PUT commits but its response is lost; the replay gets
        // 412 from the publisher's own claim. The at-rest marker matches
        // this publish on every identity field (archive id, object digests,
        // uploader) and differs only in uploaded_at — publish must treat
        // the claim as won and return the AT-REST marker, because that is
        // the document consumers will read.
        let existing = CompleteMarker {
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
            uploader: "rio-replay/test".to_string(),
        };
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_data = mock!(aws_sdk_s3::Client::put_object)
            .sequence()
            .output(|| PutObjectOutput::builder().build())
            .times(2)
            .build();
        let reval_image = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_IMAGE_OBJECT}"),
            &identity::sha256_hex(&image_bytes),
            image_bytes.len() as u64,
        );
        let reval_manifest = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_MANIFEST_OBJECT}"),
            &archive_id,
            manifest_bytes.len() as u64,
        );
        let put_marker_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let get_existing_marker = get_rule(
            format!("replay/archives/{short}/{ARCHIVE_COMPLETE_OBJECT}"),
            serde_json::to_vec(&existing).unwrap(),
        );
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_data,
                &reval_image,
                &reval_manifest,
                &put_marker_412,
                &get_existing_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let marker = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap();
        assert_eq!(marker.archive_id, archive_id);
        assert_eq!(
            marker.uploaded_at,
            test_stamp(),
            "the at-rest marker (not the locally rebuilt one) is returned"
        );
    }

    #[tokio::test]
    async fn publish_treats_a_conditional_request_conflict_like_a_lost_conditional() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // S3's other documented conditional-write rejection: HTTP 409
        // ConditionalRequestConflict (a concurrent conditional operation on
        // the same key). It must route through the same self-write
        // disambiguation as a 412 — here the at-rest object is ours, so the
        // publish continues — instead of surfacing as a raw SDK error.
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_409 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(
                ErrorMetadata::builder()
                    .code("ConditionalRequestConflict")
                    .build(),
            )
        });
        let head_image_ours = {
            let stored = sha256_base64(&identity::sha256_hex(&image_bytes)).unwrap();
            let size = image_bytes.len() as i64;
            mock!(aws_sdk_s3::Client::head_object).then_output(move || {
                HeadObjectOutput::builder()
                    .content_length(size)
                    .checksum_sha256(stored.clone())
                    .build()
            })
        };
        let put_manifest = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let reval_image = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_IMAGE_OBJECT}"),
            &identity::sha256_hex(&image_bytes),
            image_bytes.len() as u64,
        );
        let reval_manifest = head_object_rule(
            format!("replay/archives/{short}/{ARCHIVE_MANIFEST_OBJECT}"),
            &archive_id,
            manifest_bytes.len() as u64,
        );
        let put_marker = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_image_409,
                &head_image_ours,
                &put_manifest,
                &reval_image,
                &reval_manifest,
                &put_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap();
        assert_eq!(put_image_409.num_calls(), 1);
        assert_eq!(put_marker.num_calls(), 1);
    }

    #[tokio::test]
    async fn publish_refuses_an_unattributable_image_conflict() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);
        let short = identity::short_id(&archive_id);

        // The existing image matches by size but the backend returned no
        // stored SHA-256 to compare (a backend without checksum support, or
        // an object uploaded without one). Unverified bytes must never be
        // vouched for: the conflict is refused with the recovery path, and
        // the message says WHY the object could not be attributed.
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let head_image_no_checksum = {
            let size = image_bytes.len() as i64;
            mock!(aws_sdk_s3::Client::head_object)
                .then_output(move || HeadObjectOutput::builder().content_length(size).build())
        };
        let head_404_classify = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_image_412,
                &head_image_no_checksum,
                &head_404_classify
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("no SHA-256 checksum to attribute it"),
            "the refusal explains the failed attribution: {message}"
        );
        assert!(
            message.contains(&format!("replay delete {short}")),
            "the recovery path is still named: {message}"
        );
    }

    #[tokio::test]
    async fn publish_names_the_sweep_when_a_conflicting_object_vanishes() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());

        // The image PUT loses its conditional, but by the time the
        // self-write probe looks the object is GONE: a concurrent
        // `replay delete` is sweeping the prefix. The error must name the
        // sweep and the recovery (re-run), not accuse a publisher.
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_image_412 = mock!(aws_sdk_s3::Client::put_object).then_error(|| {
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build())
        });
        let head_image_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&head_404_precheck, &put_image_412, &head_image_404]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("swept mid-publish"),
            "the sweep is named: {message}"
        );
        assert!(message.contains("re-run"), "recovery named: {message}");
    }

    #[tokio::test]
    async fn publish_refuses_when_the_prefix_was_swept_mid_publish() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());

        // Both data uploads land, but a concurrent `replay delete` sweeps
        // the prefix before the marker PUT (the sweep removes whatever a
        // LIST returned, and a mid-publish prefix has no marker to exclude
        // it). The pre-marker revalidation finds the image gone: publish
        // must refuse — the marker may never vouch for unobserved state —
        // and the marker PUT must never be issued.
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_data = mock!(aws_sdk_s3::Client::put_object)
            .sequence()
            .output(|| PutObjectOutput::builder().build())
            .times(2)
            .build();
        let reval_image_404 = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_marker = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&head_404_precheck, &put_data, &reval_image_404, &put_marker]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("swept mid-publish"),
            "the sweep is named: {message}"
        );
        assert!(message.contains("re-run"), "recovery named: {message}");
        assert_eq!(
            put_marker.num_calls(),
            0,
            "completeness must never be claimed over a swept prefix"
        );
    }

    #[tokio::test]
    async fn publish_refuses_a_replaced_object_at_completeness_time() {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, manifest_bytes) = packed_tiny_archive(dir.path());
        let image_bytes = std::fs::read(&image).unwrap();

        // Worse than swept: between this publish's image upload and its
        // marker PUT, the prefix was swept AND a foreign publisher of the
        // same archive id re-claimed the image key with different bytes
        // (mkdwarfs is not deterministic). The revalidation HEAD sees the
        // size match but a different stored checksum — the marker this
        // publish built hashes ITS image, not the one at rest, so claiming
        // completeness would publish a lie.
        let head_404_precheck = mock!(aws_sdk_s3::Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let put_data = mock!(aws_sdk_s3::Client::put_object)
            .sequence()
            .output(|| PutObjectOutput::builder().build())
            .times(2)
            .build();
        let reval_image_foreign = {
            let stored = sha256_base64(&"f0".repeat(32)).unwrap();
            let size = image_bytes.len() as i64;
            mock!(aws_sdk_s3::Client::head_object).then_output(move || {
                HeadObjectOutput::builder()
                    .content_length(size)
                    .checksum_sha256(stored.clone())
                    .build()
            })
        };
        let put_marker = mock!(aws_sdk_s3::Client::put_object)
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[
                &head_404_precheck,
                &put_data,
                &reval_image_foreign,
                &put_marker
            ]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("swept and re-claimed mid-publish"),
            "the replaced object is named: {message}"
        );
        assert_eq!(
            put_marker.num_calls(),
            0,
            "the marker must not vouch for another publisher's bytes"
        );
    }

    #[test]
    fn marker_identity_comparison_ignores_only_the_upload_time() {
        let archive_id = "a".repeat(64);
        let ours = CompleteMarker {
            archive_id: archive_id.clone(),
            archive_id_short: archive_id[..16].to_string(),
            objects: BTreeMap::from([(
                ARCHIVE_IMAGE_OBJECT.to_string(),
                MemberDigest {
                    sha256: "b".repeat(64),
                    size: 7,
                },
            )]),
            uploaded_at: test_stamp(),
            uploader: "rio-replay/test".to_string(),
        };

        // Same identity fields, different upload time: the same publish.
        let mut replayed = ours.clone();
        replayed.uploaded_at = "2026-06-01T00:00:00Z".parse().unwrap();
        assert!(marker_is_same_publish(&replayed, &ours));

        // Any identity field differing means a foreign publish: a different
        // image digest (mkdwarfs nondeterminism), uploader, or archive id.
        let mut foreign_image = ours.clone();
        foreign_image.objects.insert(
            ARCHIVE_IMAGE_OBJECT.to_string(),
            MemberDigest {
                sha256: "c".repeat(64),
                size: 7,
            },
        );
        assert!(!marker_is_same_publish(&foreign_image, &ours));
        let mut foreign_uploader = ours.clone();
        foreign_uploader.uploader = "rio-replay/other".to_string();
        assert!(!marker_is_same_publish(&foreign_uploader, &ours));
        let mut foreign_id = ours.clone();
        foreign_id.archive_id = "d".repeat(64);
        assert!(!marker_is_same_publish(&foreign_id, &ours));
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

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &image, &manifest_bytes, "rio-replay/test")
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

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store
            .publish(&client, &v0_image, &manifest_bytes, "rio-replay/test")
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
            uploader: "rio-replay/test".to_string(),
        };
        let marker_bytes = serde_json::to_vec(&marker).unwrap();

        let complete_key = format!("replay/archives/{short}/complete.json");
        let image_key = format!("replay/archives/{short}/archive.dwarfs");
        let manifest_key = format!("replay/archives/{short}/manifest.json");

        let get_complete = get_rule(complete_key.clone(), marker_bytes.clone());
        let get_image = get_rule(image_key.clone(), image_bytes.clone());
        let get_manifest = get_rule(manifest_key.clone(), manifest_bytes.clone());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&get_complete, &get_image, &get_manifest]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
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
            uploader: "rio-replay/test".to_string(),
        };
        let get_complete = get_rule(
            format!("replay/archives/{short}/complete.json"),
            serde_json::to_vec(&marker).unwrap(),
        );
        // Catch-all for any other GET; the guard must keep it at zero calls.
        let get_other = mock!(aws_sdk_s3::Client::get_object)
            .then_output(|| GetObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&get_complete, &get_other]);

        let store = ArchiveStore::new("rio-chunks", "replay");
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
            uploader: "rio-replay/test".to_string(),
        };
        let get_complete = get_rule(
            format!("replay/archives/{short}/complete.json"),
            serde_json::to_vec(&marker).unwrap(),
        );
        let get_other = mock!(aws_sdk_s3::Client::get_object)
            .then_output(|| GetObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&get_complete, &get_other]);

        let store = ArchiveStore::new("rio-chunks", "replay");
        let dir = tempfile::TempDir::new().unwrap();
        let err = store
            .fetch(&client, short, &dir.path().join("fetched"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("lives under"), "got: {err:#}");
        assert_eq!(
            get_other.num_calls(),
            0,
            "nothing may be downloaded after the refusal"
        );
    }

    #[test]
    fn published_archive_is_only_constructible_from_a_consistent_marker() {
        let archive_id = "a".repeat(64);
        let short = &archive_id[..16];
        let marker = CompleteMarker {
            archive_id: archive_id.clone(),
            archive_id_short: short.to_string(),
            objects: BTreeMap::new(),
            uploaded_at: test_stamp(),
            uploader: "rio-replay/test".to_string(),
        };
        let bytes = serde_json::to_vec(&marker).unwrap();

        // The complete.json bytes are the proof of publication.
        let published = PublishedArchive::from_complete_json(&bytes).unwrap();
        assert_eq!(published.archive_id(), archive_id);
        assert_eq!(published.archive_id_short(), short);
        assert_eq!(published.marker().uploader, "rio-replay/test");
        assert_eq!(published.into_marker().archive_id, archive_id);

        // The prefix-pinned constructor refuses a marker fetched from a
        // prefix it does not name.
        PublishedArchive::from_complete_json_at(&bytes, short).unwrap();
        let err = PublishedArchive::from_complete_json_at(&bytes, "bbbbbbbbbbbbbbbb").unwrap_err();
        assert!(format!("{err:#}").contains("lives under"), "got: {err:#}");

        // An internally inconsistent marker (short id is not the leading 16
        // characters of the full id) is refused by every constructor.
        let mut edited = marker.clone();
        edited.archive_id_short = "b".repeat(16);
        let err = PublishedArchive::from_complete_json(&serde_json::to_vec(&edited).unwrap())
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("internally inconsistent"),
            "got: {err:#}"
        );

        // A malformed archive id (wrong length / not lowercase hex) and
        // unparseable bytes are refused too.
        let mut malformed = marker.clone();
        malformed.archive_id = "abc".to_string();
        assert!(
            PublishedArchive::from_complete_json(&serde_json::to_vec(&malformed).unwrap()).is_err()
        );
        assert!(PublishedArchive::from_complete_json(b"not json").is_err());
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
            uploader: "rio-replay/test".to_string(),
        };
        let marker_a = serde_json::to_vec(&marker_for(short_a, 'a')).unwrap();
        let marker_b = serde_json::to_vec(&marker_for(short_b, 'b')).unwrap();

        // One page: two complete prefixes (deliberately listed out of order)
        // plus their data objects, and one incomplete prefix that has an
        // image but no complete.json yet.
        let list_page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|req| req.prefix() == Some("replay/archives/"))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .contents(
                        Object::builder()
                            .key(format!("replay/archives/{short_b}/complete.json"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key(format!("replay/archives/{short_b}/archive.dwarfs"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key(format!("replay/archives/{short_a}/complete.json"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key("replay/archives/cccccccccccccccc/archive.dwarfs")
                            .build(),
                    )
                    .build()
            });
        let get_a = get_rule(format!("replay/archives/{short_a}/complete.json"), marker_a);
        let get_b = get_rule(format!("replay/archives/{short_b}/complete.json"), marker_b);
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&list_page, &get_a, &get_b]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let archives = store.list(&client).await.unwrap();
        assert_eq!(archives.len(), 2, "the incomplete prefix is invisible");
        assert_eq!(archives[0].archive_id_short(), short_a);
        assert_eq!(archives[0].archive_id(), "a".repeat(64));
        assert_eq!(archives[1].archive_id_short(), short_b);
        assert_eq!(archives[1].archive_id(), "b".repeat(64));
        assert_eq!(get_a.num_calls(), 1);
        assert_eq!(get_b.num_calls(), 1);
    }

    #[tokio::test]
    async fn list_skips_a_prefix_swept_between_list_and_get() {
        // `replay delete` unpublishes a prefix by removing complete.json
        // strictly first, and the LIST→GET window here is unbounded: a
        // listed marker can be gone by the time it is read. That is the
        // deletion order doing its job — the prefix is no longer published
        // — so the entry is skipped and every other archive still lists
        // (the same race `get_object_bytes` maps to `Ok(None)`). Junk
        // marker *content*, by contrast, stays a hard error here: see
        // `list_cross_checks_the_marker_against_its_prefix`.
        use aws_sdk_s3::operation::get_object::GetObjectError;
        use aws_sdk_s3::types::error::NoSuchKey;

        let short_a = "aaaaaaaaaaaaaaaa";
        let short_b = "bbbbbbbbbbbbbbbb";
        let marker_a = CompleteMarker {
            archive_id: "a".repeat(64),
            archive_id_short: short_a.to_string(),
            objects: BTreeMap::new(),
            uploaded_at: test_stamp(),
            uploader: "rio-replay/test".to_string(),
        };
        let list_page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|req| req.prefix() == Some("replay/archives/"))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .contents(
                        Object::builder()
                            .key(format!("replay/archives/{short_a}/complete.json"))
                            .build(),
                    )
                    .contents(
                        Object::builder()
                            .key(format!("replay/archives/{short_b}/complete.json"))
                            .build(),
                    )
                    .build()
            });
        let get_a = get_rule(
            format!("replay/archives/{short_a}/complete.json"),
            serde_json::to_vec(&marker_a).unwrap(),
        );
        let swept_key = format!("replay/archives/{short_b}/complete.json");
        let get_b_swept = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(move |req| req.key() == Some(swept_key.as_str()))
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&list_page, &get_a, &get_b_swept]
        );

        let store = ArchiveStore::new("rio-chunks", "replay");
        let archives = store.list(&client).await.unwrap();
        assert_eq!(
            archives.len(),
            1,
            "the concurrently-deleted prefix is skipped, not a listing error"
        );
        assert_eq!(archives[0].archive_id_short(), short_a);
        assert_eq!(get_b_swept.num_calls(), 1);
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
            uploader: "rio-replay/test".to_string(),
        };
        let list_page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|req| req.prefix() == Some("replay/archives/"))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .contents(
                        Object::builder()
                            .key(format!("replay/archives/{short}/complete.json"))
                            .build(),
                    )
                    .build()
            });
        let get_marker = get_rule(
            format!("replay/archives/{short}/complete.json"),
            serde_json::to_vec(&marker).unwrap(),
        );
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list_page, &get_marker]);

        let store = ArchiveStore::new("rio-chunks", "replay");
        let err = store.list(&client).await.unwrap_err();
        assert!(format!("{err:#}").contains("lives under"), "got: {err:#}");
    }
}
