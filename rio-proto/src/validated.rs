//! Validated wrappers around generated proto types.
//!
//! Proto types (`PathInfo`, etc.) have all-public fields with no invariants.
//! `PathInfo.store_path: String` and `nar_hash: Vec<u8>` flow through the
//! whole system unvalidated; before this module, validation existed only at
//! the rio-store gRPC ingress (which parsed `StorePath` and immediately
//! discarded the parsed value) and nowhere else. `NarinfoRow::into_path_info`
//! (DB egress) did zero validation — a malformed row would propagate silently.
//!
//! `ValidatedPathInfo` parses `store_path` and each reference into a real
//! `StorePath` (rejects path traversal, bad nixbase32, oversized names),
//! and enforces `nar_hash` is exactly 32 bytes (SHA-256). It provides
//! `TryFrom<PathInfo>` for inbound conversion and `From<ValidatedPathInfo>`
//! for outbound (infallible — a validated type is always a valid raw type).
//!
//! # Validation policy
//!
//! - `store_path`: **hard-fail** — this is the primary key everywhere.
//! - `nar_hash`: **hard-fail** if not 32 bytes — SHA-256 is the only hash
//!   we speak on the wire; anything else is protocol corruption.
//! - `references`: **hard-fail** on any invalid entry — references feed
//!   into the builder's synthetic Nix SQLite DB (`synth_db.rs`), where a
//!   malformed path in the `Refs` table is a correctness/security issue.
//! - `deriver`: **soft-fail** — the Nix daemon sends empty deriver for
//!   source paths; `NarinfoRow.deriver` is `Option<String>`. Hard-failing
//!   on empty breaks everything. A non-empty-but-invalid deriver (DB
//!   corruption, ancient client) is logged and coerced to `None`;
//!   deriver is informational, not security-relevant.
//! - `content_address`: no structural validation (pass-through string);
//!   empty → `None`.
//! - `store_path_hash`: **not validated** — may be empty (computed by
//!   rio-store from the parsed `store_path`); this is a server-side
//!   derived field.

use rio_nix::store_path::{StorePath, StorePathError};

use crate::types::PathInfo;

/// A `PathInfo` whose `store_path` parses as a valid `StorePath`, whose
/// `nar_hash` is exactly 32 bytes (SHA-256), and whose `references` all
/// parse as valid `StorePath`s.
#[derive(Debug, Clone)]
pub struct ValidatedPathInfo {
    pub store_path: StorePath,
    /// Binary hash of the store path. May be empty — rio-store computes it
    /// server-side from the parsed `store_path` if not supplied.
    pub store_path_hash: Vec<u8>,
    /// `None` if empty or unparseable (soft-fail; see module docs).
    pub deriver: Option<StorePath>,
    pub nar_hash: [u8; 32],
    pub nar_size: u64,
    pub references: Vec<StorePath>,
    pub registration_time: u64,
    pub ultimate: bool,
    pub signatures: Vec<String>,
    /// `None` if empty.
    pub content_address: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PathInfoValidationError {
    #[error("invalid store_path {path:?}: {source}")]
    StorePath {
        path: String,
        source: StorePathError,
    },
    #[error("nar_hash must be 32 bytes (SHA-256), got {0}")]
    NarHashLen(usize),
    #[error("invalid reference path {path:?}: {source}")]
    Reference {
        path: String,
        source: StorePathError,
    },
}

impl TryFrom<PathInfo> for ValidatedPathInfo {
    type Error = PathInfoValidationError;

    fn try_from(p: PathInfo) -> Result<Self, Self::Error> {
        let store_path = StorePath::parse(&p.store_path).map_err(|source| {
            PathInfoValidationError::StorePath {
                path: p.store_path.clone(),
                source,
            }
        })?;

        let nar_hash: [u8; 32] = p
            .nar_hash
            .as_slice()
            .try_into()
            .map_err(|_| PathInfoValidationError::NarHashLen(p.nar_hash.len()))?;

        let references = p
            .references
            .into_iter()
            .map(|r| {
                StorePath::parse(&r)
                    .map_err(|source| PathInfoValidationError::Reference { path: r, source })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // deriver: soft-fail. Empty → None. Non-empty-but-invalid → log + None.
        let deriver = if p.deriver.is_empty() {
            None
        } else {
            match StorePath::parse(&p.deriver) {
                Ok(sp) => Some(sp),
                Err(e) => {
                    tracing::warn!(
                        deriver = %p.deriver,
                        error = %e,
                        "invalid deriver path in PathInfo; coercing to None (soft-fail)"
                    );
                    None
                }
            }
        };

        let content_address = if p.content_address.is_empty() {
            None
        } else {
            Some(p.content_address)
        };

        Ok(ValidatedPathInfo {
            store_path,
            store_path_hash: p.store_path_hash,
            deriver,
            nar_hash,
            nar_size: p.nar_size,
            references,
            registration_time: p.registration_time,
            ultimate: p.ultimate,
            signatures: p.signatures,
            content_address,
        })
    }
}

impl From<ValidatedPathInfo> for PathInfo {
    fn from(v: ValidatedPathInfo) -> Self {
        PathInfo {
            // StorePath::to_string() is cheap: clones the cached `full` field.
            store_path: v.store_path.to_string(),
            store_path_hash: v.store_path_hash,
            deriver: v.deriver.map(|d| d.to_string()).unwrap_or_default(),
            nar_hash: v.nar_hash.to_vec(),
            nar_size: v.nar_size,
            references: v.references.into_iter().map(|r| r.to_string()).collect(),
            registration_time: v.registration_time,
            ultimate: v.ultimate,
            signatures: v.signatures,
            content_address: v.content_address.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const VALID_PATH: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0";
    const VALID_DRV: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello.drv";
    const VALID_REF: &str = "/nix/store/cccccccccccccccccccccccccccccccc-glibc-2.40";

    /// A fully-valid baseline `PathInfo`. Tests mutate one field at a time.
    fn make_raw() -> PathInfo {
        PathInfo {
            store_path: VALID_PATH.into(),
            store_path_hash: vec![],
            deriver: String::new(),
            nar_hash: vec![0; 32],
            nar_size: 1024,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: String::new(),
        }
    }

    #[test]
    fn happy_path() -> anyhow::Result<()> {
        let raw = PathInfo {
            deriver: VALID_DRV.into(),
            nar_hash: vec![0x42; 32],
            references: vec![VALID_REF.into()],
            content_address: "fixed:r:sha256:abc".into(),
            ..make_raw()
        };
        let v = ValidatedPathInfo::try_from(raw)?;
        assert_eq!(v.store_path.as_str(), VALID_PATH);
        assert_eq!(v.nar_hash, [0x42; 32]);
        assert_eq!(v.deriver.as_ref().map(|d| d.as_str()), Some(VALID_DRV));
        assert_eq!(v.references[0].as_str(), VALID_REF);
        assert_eq!(v.content_address.as_deref(), Some("fixed:r:sha256:abc"));
        Ok(())
    }

    /// Hard-fail validations: each case corrupts one field of a known-good
    /// `PathInfo`, then asserts the specific error variant AND that the
    /// offending value surfaces in the error message.
    #[rstest]
    #[case::bad_store_path(
        |p: &mut PathInfo| p.store_path = "not-a-store-path".into(),
        "StorePath", "not-a-store-path"
    )]
    #[case::short_hash(|p: &mut PathInfo| p.nar_hash = vec![0; 31], "NarHashLen", "31")]
    #[case::long_hash(|p: &mut PathInfo| p.nar_hash = vec![0; 64], "NarHashLen", "64")]
    #[case::bad_reference(
        |p: &mut PathInfo| p.references = vec![VALID_REF.into(), "/tmp/evil".into()],
        "Reference", "/tmp/evil"
    )]
    fn rejects_invalid(
        #[case] mutate: fn(&mut PathInfo),
        #[case] want_variant: &str,
        #[case] want_in_msg: &str,
    ) {
        let mut raw = make_raw();
        mutate(&mut raw);
        let err = ValidatedPathInfo::try_from(raw).unwrap_err();
        let got = match &err {
            PathInfoValidationError::StorePath { .. } => "StorePath",
            PathInfoValidationError::NarHashLen(_) => "NarHashLen",
            PathInfoValidationError::Reference { .. } => "Reference",
        };
        assert_eq!(got, want_variant, "got: {err:?}");
        assert!(err.to_string().contains(want_in_msg), "got: {err}");
    }

    /// Soft-fail: `deriver` never causes `Err` — empty or unparseable
    /// values normalize to `None`. Also covers empty `content_address`
    /// → `None` (the baseline leaves it empty).
    #[rstest]
    #[case::empty("", None)]
    #[case::invalid("garbage-not-a-path", None)]
    #[case::valid(VALID_DRV, Some(VALID_DRV))]
    fn deriver_softfail(#[case] deriver: &str, #[case] want: Option<&str>) {
        let raw = PathInfo {
            deriver: deriver.into(),
            ..make_raw()
        };
        let v = ValidatedPathInfo::try_from(raw).expect("deriver never hard-fails");
        assert_eq!(v.deriver.as_ref().map(|d| d.as_str()), want);
        assert!(v.content_address.is_none());
    }

    /// `Raw → Validated → Raw` is identity (prost-derived `PartialEq` on
    /// `PathInfo`). Covers Option↔empty-string normalization both ways:
    /// the `none_fields` case starts with empty `deriver`/`content_address`
    /// and round-trips back to empty via `None`.
    #[rstest]
    #[case::full(PathInfo {
        store_path_hash: vec![0x11; 20],
        deriver: VALID_DRV.into(),
        nar_hash: vec![0x42; 32],
        nar_size: 4096,
        references: vec![VALID_REF.into()],
        registration_time: 1_700_000_000,
        ultimate: true,
        signatures: vec!["sig1".into(), "sig2".into()],
        content_address: "fixed:r:sha256:xyz".into(),
        ..make_raw()
    })]
    #[case::none_fields(make_raw())]
    fn roundtrip(#[case] raw: PathInfo) -> anyhow::Result<()> {
        let v = ValidatedPathInfo::try_from(raw.clone())?;
        let raw2: PathInfo = v.into();
        assert_eq!(raw, raw2);
        Ok(())
    }
}
