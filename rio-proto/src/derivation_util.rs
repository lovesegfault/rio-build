//! Helpers on the `rio.drv.v1.Derivation` wire type: canonical
//! encode + digest, structural validation of untrusted messages, the
//! rio-nix `Derivation` ⇄ proto converters, and the gateway's
//! drv-blob cross-check.
//!
//! ADR-024 makes `blake3(canonical proto bytes)` the negotiation/
//! storage key for derivation content. Like
//! [`castore_util`](crate::castore_util), the digest computation lives
//! in the crate that owns the type so producers (the `rio build`
//! client) and validators (the gateway) cannot drift.
//!
//! The identity rules this module enforces (verified at 48,223/48,223
//! store drvs byte-identical round-trip, DRVPROTO):
//!
//! - Canonical encode = prost's default field-order encode of a
//!   message whose repeated lists are sorted ([`to_proto`] sorts on
//!   conversion; [`validate_derivation`] rejects unsorted input).
//!   Defaults are omitted by prost.
//! - Hostile unsorted/dup-key messages FAIL validation — they are
//!   never silently re-sorted, because re-sorting re-keys the digest
//!   and would give one drv_path unboundedly many digests.
//! - drv_path and `hashDerivationModulo` remain Nix's own values,
//!   computed from the reconstructed ATerm. ATerm exists only inside
//!   those hash computations ([`verify_drv_blob`]).

use std::collections::{BTreeMap, BTreeSet};

use prost::Message;

use rio_nix::derivation::{
    Derivation as NixDerivation, DerivationError as NixDerivationError, DerivationLike,
    DerivationOutput,
};
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::{StorePath, StorePathError};

use crate::drv::{Derivation, EnvVar, InputDrv, Output};

/// Canonical encoding of a `Derivation` message: prost's default
/// ascending-field-number encode with proto3 defaults omitted.
///
/// The *caller* is responsible for the message being in canonical
/// form (sorted repeated fields) — this function does not re-sort.
/// Messages built by [`to_proto`] are canonical by construction;
/// untrusted messages must pass [`validate_derivation`] first. A
/// mis-sorted message encodes to *different* bytes and therefore a
/// different digest, which is exactly what makes validation + a
/// digest recompute sufficient to reject it.
pub fn canonical_encode(d: &Derivation) -> Vec<u8> {
    d.encode_to_vec()
}

/// `drv_digest = blake3(canonical_encode(Derivation))`, 32 raw bytes.
///
/// The Merkle negotiation key of ADR-024's build plan. Same caller
/// contract as [`canonical_encode`]: sortedness is the caller's
/// responsibility.
pub fn derivation_digest(d: &Derivation) -> [u8; 32] {
    *blake3::hash(&d.encode_to_vec()).as_bytes()
}

/// Why an untrusted [`Derivation`] message was rejected by
/// [`validate_derivation`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DerivationError {
    #[error("derivation has no outputs")]
    NoOutputs,
    #[error("empty output name")]
    EmptyOutputName,
    #[error("{field} not sorted byte-lexicographically at {at:?}")]
    Unsorted { field: &'static str, at: String },
    #[error("duplicate {field} entry {at:?}")]
    Duplicate { field: &'static str, at: String },
}

/// Display of possibly-non-UTF-8 key bytes for error messages.
/// Escaped and truncated for the same reason as castore_util's:
/// these strings end up in tonic `Status` messages and log lines,
/// and the bytes are attacker-chosen.
fn show(name: &[u8]) -> String {
    let head = &name[..name.len().min(64)];
    let mut s = head.escape_ascii().to_string();
    if name.len() > head.len() {
        s.push('…');
    }
    s
}

fn check_sorted_unique<'a>(
    field: &'static str,
    keys: impl Iterator<Item = &'a [u8]>,
) -> Result<(), DerivationError> {
    let mut prev: Option<&[u8]> = None;
    for k in keys {
        if let Some(p) = prev {
            if p == k {
                return Err(DerivationError::Duplicate { field, at: show(k) });
            }
            if p > k {
                return Err(DerivationError::Unsorted { field, at: show(k) });
            }
        }
        prev = Some(k);
    }
    Ok(())
}

/// Structural validation of an untrusted `Derivation` message.
///
/// Checks, per `derivation.proto`'s documented invariants:
/// - at least one output (Nix invariant), every output name non-empty;
/// - `outputs` sorted byte-lexicographically by `name`, unique;
/// - `input_drvs` sorted by `drv_path`, unique; each edge's output
///   selection sorted, unique;
/// - `input_srcs` sorted, unique;
/// - `env` sorted by `key`, unique.
///
/// Sortedness is a *reject*, never a re-sort: canonicalizing hostile
/// input would re-key its digest (ADR-024). Passing this check means
/// the message is in canonical form, so [`canonical_encode`] of it is
/// THE canonical bytes — [`verify_drv_blob`] additionally byte-compares
/// that re-encode against the received blob to reject non-minimal wire
/// games (unknown fields, non-minimal varints).
///
/// Semantic path/hash consistency (drv_path, FOD output paths) is NOT
/// checked here — that needs the ATerm reconstruction and lives in
/// [`verify_drv_blob`].
pub fn validate_derivation(d: &Derivation) -> Result<(), DerivationError> {
    if d.outputs.is_empty() {
        return Err(DerivationError::NoOutputs);
    }
    if d.outputs.iter().any(|o| o.name.is_empty()) {
        return Err(DerivationError::EmptyOutputName);
    }
    check_sorted_unique("outputs", d.outputs.iter().map(|o| o.name.as_slice()))?;
    check_sorted_unique(
        "input_drvs",
        d.input_drvs.iter().map(|i| i.drv_path.as_slice()),
    )?;
    for i in &d.input_drvs {
        check_sorted_unique("input_drvs.outputs", i.outputs.iter().map(Vec::as_slice))?;
    }
    check_sorted_unique("input_srcs", d.input_srcs.iter().map(Vec::as_slice))?;
    check_sorted_unique("env", d.env.iter().map(|e| e.key.as_slice()))?;
    Ok(())
}

/// rio-nix `Derivation` → canonical proto message.
///
/// Total: every field of the rio-nix model maps to exactly one proto
/// field; rio-nix's model is String-typed (it rejects non-UTF-8 drvs
/// at parse), so String → bytes is trivially lossless. The result is
/// canonical by construction: `outputs` is sorted here; the BTree-backed
/// fields (`input_drvs`, `input_srcs`, `env`) iterate in byte-lex
/// order already.
pub fn to_proto(d: &NixDerivation) -> Derivation {
    let mut outputs: Vec<Output> = d
        .outputs()
        .iter()
        .map(|o| Output {
            name: o.name().as_bytes().to_vec(),
            path: o.path().as_bytes().to_vec(),
            hash_algo: o.hash_algo().as_bytes().to_vec(),
            hash: o.hash().as_bytes().to_vec(),
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    Derivation {
        outputs,
        input_drvs: d
            .input_drvs()
            .iter()
            .map(|(path, outs)| InputDrv {
                drv_path: path.as_bytes().to_vec(),
                outputs: outs.iter().map(|o| o.as_bytes().to_vec()).collect(),
            })
            .collect(),
        input_srcs: d
            .input_srcs()
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect(),
        platform: d.platform().as_bytes().to_vec(),
        builder: d.builder().as_bytes().to_vec(),
        args: d.args().iter().map(|a| a.as_bytes().to_vec()).collect(),
        env: d
            .env()
            .iter()
            .map(|(k, v)| EnvVar {
                key: k.as_bytes().to_vec(),
                value: v.as_bytes().to_vec(),
            })
            .collect(),
    }
}

/// Why a drv blob failed [`verify_drv_blob`] (or a proto → rio-nix
/// conversion failed in [`from_proto`]).
#[derive(Debug, thiserror::Error)]
pub enum DrvBlobError {
    #[error("blob digest mismatch: claimed {claimed}, computed {computed}")]
    DigestMismatch { claimed: String, computed: String },
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error(transparent)]
    Invalid(#[from] DerivationError),
    #[error("blob is not the canonical encoding of its decoded message")]
    NonCanonical,
    #[error("non-UTF-8 bytes in {field} ({bytes:?})")]
    NonUtf8 { field: &'static str, bytes: String },
    #[error("derivation invariant violated: {0}")]
    Nix(#[from] NixDerivationError),
    #[error("claimed drv path is invalid: {0}")]
    BadDrvPath(#[from] StorePathError),
    #[error("claimed drv path {0:?} does not name a .drv")]
    NotADrvPath(String),
    #[error("drv path mismatch: claimed {claimed}, recomputed {computed}")]
    DrvPathMismatch { claimed: String, computed: String },
    #[error(
        "fixed-output path mismatch for output {name:?}: recorded {recorded}, recomputed {computed}"
    )]
    FodPathMismatch {
        name: String,
        recorded: String,
        computed: String,
    },
}

fn hex32(b: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::with_capacity(64), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn utf8(b: &[u8], field: &'static str) -> Result<String, DrvBlobError> {
    String::from_utf8(b.to_vec()).map_err(|_| DrvBlobError::NonUtf8 {
        field,
        bytes: show(b),
    })
}

/// Proto message → rio-nix `Derivation` (the inverse converter).
///
/// Total over any *valid* message whose bytes fields are valid UTF-8
/// (the only form rio-nix can currently represent); non-UTF-8 input is
/// a hard error, never silent lossy replacement. The caller should run
/// [`validate_derivation`] first: this function preserves `outputs`
/// order verbatim but rebuilds the BTree-backed fields, so feeding it
/// an *unvalidated* unsorted message would silently canonicalize —
/// exactly what the identity rules prohibit on the trust boundary.
///
/// The round-trip guarantee (DRVPROTO, 48,223/48,223): for any drv Nix
/// emitted, `from_proto(to_proto(parse(aterm))).to_aterm() == aterm`
/// byte-identically.
pub fn from_proto(p: &Derivation) -> Result<NixDerivation, DrvBlobError> {
    let outputs = p
        .outputs
        .iter()
        .map(|o| {
            Ok(DerivationOutput::new(
                utf8(&o.name, "output name")?,
                utf8(&o.path, "output path")?,
                utf8(&o.hash_algo, "output hash_algo")?,
                utf8(&o.hash, "output hash")?,
            )?)
        })
        .collect::<Result<Vec<_>, DrvBlobError>>()?;
    let input_drvs = p
        .input_drvs
        .iter()
        .map(|i| {
            Ok((
                utf8(&i.drv_path, "input drv path")?,
                i.outputs
                    .iter()
                    .map(|o| utf8(o, "input output name"))
                    .collect::<Result<BTreeSet<_>, _>>()?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, DrvBlobError>>()?;
    let input_srcs = p
        .input_srcs
        .iter()
        .map(|s| utf8(s, "input src"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let env = p
        .env
        .iter()
        .map(|e| Ok((utf8(&e.key, "env key")?, utf8(&e.value, "env value")?)))
        .collect::<Result<BTreeMap<_, _>, DrvBlobError>>()?;
    Ok(NixDerivation::new(
        outputs,
        input_drvs,
        input_srcs,
        utf8(&p.platform, "platform")?,
        utf8(&p.builder, "builder")?,
        p.args
            .iter()
            .map(|a| utf8(a, "arg"))
            .collect::<Result<Vec<_>, _>>()?,
        env,
    )?)
}

/// A drv blob that passed every cross-check in [`verify_drv_blob`].
#[derive(Debug, Clone)]
pub struct VerifiedDrv {
    /// The parsed rio-nix form — ready for `hash_derivation_modulo`,
    /// output-path computation, and scheduler translation.
    pub derivation: NixDerivation,
    /// The reconstructed ATerm text. Byte-identical to what Nix would
    /// write for this derivation (this is what `make_text` hashed to
    /// confirm `drv_path`).
    pub aterm: String,
    /// The recomputed `.drv` store path (== the claimed one).
    pub drv_path: StorePath,
    /// `blake3(blob)` — the verified content digest, the negotiation
    /// key this blob is stored under.
    pub digest: [u8; 32],
}

/// The gateway's drv-blob cross-check (ADR-024 "Identity rules").
///
/// `blob` is the received drv content, `claimed_digest` the digest the
/// client negotiated/uploaded it under, `claimed_drv_path` the drv
/// path the submitting skeleton node declares. The drv name is not
/// part of the content (Nix derives it from the store path), which is
/// why the claimed path is an input: its *name* seeds the recompute,
/// while its hash part is recomputed from content and compared.
///
/// Pipeline, all of which must pass:
/// 1. `blake3(blob) == claimed_digest` — hashing is over received
///    bytes, never a re-serialization;
/// 2. protobuf decode;
/// 3. [`validate_derivation`] — hostile unsorted/dup-key messages are
///    rejected, never re-sorted;
/// 4. canonical re-encode byte-compares equal to `blob` — rejects
///    non-canonical byte-variants (unknown fields, non-minimal
///    varints) that would otherwise give one drv_path unbounded
///    digests;
/// 5. ATerm reconstruction via rio-nix (hard error on non-UTF-8);
/// 6. drv_path recompute: `make_text(name, sha256(aterm), refs)` with
///    `refs = input_drvs keys + input_srcs`, compared to the claimed
///    path — the same external anchor Nix minted the on-disk path
///    with;
/// 7. for a fixed-output derivation, the output path is additionally
///    recomputed via `make_fixed_output` and compared (statically
///    checkable without graph context; input-addressed output paths
///    need the full input graph via `hash_derivation_modulo` and are
///    the scheduler-side sweep's job). An unparseable `hash_algo`
///    skips this step only — the stored form preserves unknown algos
///    by design, and steps 1–6 still anchor the content.
pub fn verify_drv_blob(
    blob: &[u8],
    claimed_digest: &[u8; 32],
    claimed_drv_path: &str,
) -> Result<VerifiedDrv, DrvBlobError> {
    let digest = *blake3::hash(blob).as_bytes();
    if digest != *claimed_digest {
        return Err(DrvBlobError::DigestMismatch {
            claimed: hex32(claimed_digest),
            computed: hex32(&digest),
        });
    }

    let msg = Derivation::decode(blob)?;
    validate_derivation(&msg)?;
    if msg.encode_to_vec() != blob {
        return Err(DrvBlobError::NonCanonical);
    }

    let derivation = from_proto(&msg)?;
    let aterm = derivation.to_aterm();

    let sp = StorePath::parse(claimed_drv_path)?;
    let Some(drv_name) = sp.name().strip_suffix(".drv") else {
        return Err(DrvBlobError::NotADrvPath(claimed_drv_path.to_string()));
    };
    let refs = derivation
        .input_drvs()
        .keys()
        .chain(derivation.input_srcs().iter())
        .map(|r| StorePath::parse(r))
        .collect::<Result<Vec<_>, _>>()?;
    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    let computed = StorePath::make_text(sp.name(), &h, &refs)?;
    if computed.as_str() != claimed_drv_path {
        return Err(DrvBlobError::DrvPathMismatch {
            claimed: claimed_drv_path.to_string(),
            computed: computed.as_str().to_string(),
        });
    }

    if derivation.is_fixed_output() {
        let o = &derivation.outputs()[0];
        let (recursive, algo_str) = match o.hash_algo().strip_prefix("r:") {
            Some(rest) => (true, rest),
            None => (false, o.hash_algo()),
        };
        // Unknown algo / undecodable hash: skip the FOD path check
        // (preserve-bytes rule — see step 7 in the doc comment).
        if let Ok(algo) = algo_str.parse::<HashAlgo>()
            && let Ok(digest_bytes) = hex_decode(o.hash())
            && let Ok(nh) = NixHash::new(algo, digest_bytes)
        {
            let p = StorePath::make_fixed_output(drv_name, &nh, recursive, &[])?;
            if p.as_str() != o.path() {
                return Err(DrvBlobError::FodPathMismatch {
                    name: o.name().to_string(),
                    recorded: o.path().to_string(),
                    computed: p.as_str().to_string(),
                });
            }
        }
    }

    Ok(VerifiedDrv {
        derivation,
        aterm,
        drv_path: computed,
        digest,
    })
}

/// Minimal lowercase/uppercase base16 decode (avoids a `hex` dep for
/// one call site; the input is a short expected-output hash).
fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    fn nibble(b: u8) -> Result<u8, ()> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(()),
        }
    }
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    s.chunks_exact(2)
        .map(|c| Ok(nibble(c[0])? << 4 | nibble(c[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &[u8]) -> Output {
        Output {
            name: name.to_vec(),
            path: format!(
                "/nix/store/00000000000000000000000000000000-x-{}",
                str::from_utf8(name).expect("test names are ASCII")
            )
            .into_bytes(),
            hash_algo: vec![],
            hash: vec![],
        }
    }

    fn env(key: &[u8]) -> EnvVar {
        EnvVar {
            key: key.to_vec(),
            value: b"v".to_vec(),
        }
    }

    fn minimal() -> Derivation {
        Derivation {
            outputs: vec![out(b"out")],
            input_drvs: vec![],
            input_srcs: vec![],
            platform: b"x86_64-linux".to_vec(),
            builder: b"/bin/sh".to_vec(),
            args: vec![],
            env: vec![],
        }
    }

    #[test]
    fn validate_accepts_canonical_message() {
        let d = Derivation {
            outputs: vec![out(b"dev"), out(b"out")],
            input_drvs: vec![
                InputDrv {
                    drv_path: b"/nix/store/a.drv".to_vec(),
                    outputs: vec![b"dev".to_vec(), b"out".to_vec()],
                },
                InputDrv {
                    drv_path: b"/nix/store/b.drv".to_vec(),
                    outputs: vec![b"out".to_vec()],
                },
            ],
            input_srcs: vec![b"/nix/store/a-src".to_vec(), b"/nix/store/b-src".to_vec()],
            env: vec![env(b"a"), env(b"b")],
            ..minimal()
        };
        assert!(validate_derivation(&d).is_ok());
    }

    #[test]
    fn validate_rejects_no_outputs_and_empty_names() {
        let no_outputs = Derivation {
            outputs: vec![],
            ..minimal()
        };
        assert_eq!(
            validate_derivation(&no_outputs),
            Err(DerivationError::NoOutputs)
        );
        let empty_name = Derivation {
            outputs: vec![out(b"")],
            ..minimal()
        };
        assert_eq!(
            validate_derivation(&empty_name),
            Err(DerivationError::EmptyOutputName)
        );
    }

    #[test]
    fn validate_rejects_unsorted_and_duplicates_per_field() {
        // Unsorted outputs.
        let d = Derivation {
            outputs: vec![out(b"out"), out(b"dev")],
            ..minimal()
        };
        assert!(matches!(
            validate_derivation(&d),
            Err(DerivationError::Unsorted {
                field: "outputs",
                ..
            })
        ));
        // Duplicate env keys (the digest-splitting attack: same map,
        // two encodings).
        let d = Derivation {
            env: vec![env(b"k"), env(b"k")],
            ..minimal()
        };
        assert!(matches!(
            validate_derivation(&d),
            Err(DerivationError::Duplicate { field: "env", .. })
        ));
        // Unsorted output selection inside one edge.
        let d = Derivation {
            input_drvs: vec![InputDrv {
                drv_path: b"/nix/store/a.drv".to_vec(),
                outputs: vec![b"out".to_vec(), b"dev".to_vec()],
            }],
            ..minimal()
        };
        assert!(matches!(
            validate_derivation(&d),
            Err(DerivationError::Unsorted {
                field: "input_drvs.outputs",
                ..
            })
        ));
        // Unsorted input_srcs.
        let d = Derivation {
            input_srcs: vec![b"/nix/store/b".to_vec(), b"/nix/store/a".to_vec()],
            ..minimal()
        };
        assert!(matches!(
            validate_derivation(&d),
            Err(DerivationError::Unsorted {
                field: "input_srcs",
                ..
            })
        ));
    }

    #[test]
    fn digest_is_blake3_of_canonical_encode() {
        let d = minimal();
        assert_eq!(
            derivation_digest(&d),
            *blake3::hash(&canonical_encode(&d)).as_bytes()
        );
        let d2 = Derivation {
            platform: b"aarch64-linux".to_vec(),
            ..minimal()
        };
        assert_ne!(derivation_digest(&d), derivation_digest(&d2));
    }

    #[test]
    fn error_display_escapes_hostile_key_bytes() {
        // Env keys are attacker-chosen bytes that flow into tonic
        // Status messages and log lines.
        let hostile = b"evil\n\x1b[31mINJECTED";
        let d = Derivation {
            env: vec![env(hostile), env(hostile)],
            ..minimal()
        };
        let msg = validate_derivation(&d).unwrap_err().to_string();
        assert!(
            !msg.contains('\n') && !msg.contains('\x1b'),
            "error display must escape control bytes, got: {msg:?}"
        );
    }

    #[test]
    fn proto3_defaults_are_wire_absent() {
        // The empty-vs-missing collapse (ADR-024 judgment call 5):
        // an IA output's empty path/hash_algo/hash must produce zero
        // wire bytes, or two logically-equal drvs would digest apart.
        let o = Output {
            name: b"out".to_vec(),
            path: vec![],
            hash_algo: vec![],
            hash: vec![],
        };
        // tag(1)+len+“out” only.
        assert_eq!(o.encode_to_vec(), b"\x0a\x03out");
    }
}
