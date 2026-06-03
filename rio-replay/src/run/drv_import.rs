//! Derivation-text import source backed by the open replay archive.
//!
//! The client-ops submitter must register a batch's `.drv` closure on the
//! gateway before `BuildPathsWithResults` can run. [`DrvArchive`] walks
//! that closure over the ATerm texts embedded in the replay archive
//! (`nix/store/*.drv` members) and materializes each derivation as a
//! worker-protocol upload entry, so derivation texts are imported over
//! `AddMultipleToStore` without a local Nix store or a `nix` binary.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use rio_nix::derivation::Derivation;
use rio_nix::nar::NarNode;
use rio_nix::protocol::client::{NarPayload, StoreEntry};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use rio_nix::store_path::nixbase32;
use sha2::{Digest as _, Sha256};

use crate::archive::reader::ReplayArchive;

/// Derivation-text lookup over an open replay archive.
///
/// Construction is metadata-only; ATerm texts are read and NAR-encoded on
/// demand per call, so memory stays proportional to one derivation (plus
/// the closure's path strings), never to the whole archive.
#[derive(Debug)]
pub struct DrvArchive {
    archive: Arc<ReplayArchive>,
}

/// Result of [`DrvArchive::closure`]: what the archive can offer for a
/// batch, and what it cannot.
#[derive(Debug)]
pub struct DrvClosure {
    /// Importable derivations in reference order (post-order,
    /// deduplicated across roots).
    pub order: Vec<String>,
    /// Interior input derivations the walk reached but the archive does
    /// not embed (sorted, deduplicated): the thin-archive gap set, skipped
    /// per the offer policy and surfaced on the batch record.
    pub skipped: Vec<String>,
}

impl DrvArchive {
    /// Wrap an open replay archive. Every `.drv` member the archive embeds
    /// becomes importable; nothing else is.
    pub fn new(archive: Arc<ReplayArchive>) -> Self {
        Self { archive }
    }

    /// Read and parse one embedded derivation's ATerm text.
    fn derivation(&self, drv_path: &str) -> Result<(String, Derivation)> {
        let text = self.archive.read_drv(drv_path)?;
        let parsed = Derivation::parse(&text)
            .map_err(|e| anyhow::anyhow!("parse derivation {drv_path}: {e}"))?;
        Ok((text, parsed))
    }

    /// Walk the derivation closure of `roots` over the archive's embedded
    /// ATerms: every derivation appears after all of its in-archive input
    /// derivations (post-order), deduplicated across roots, plus the set
    /// of interior inputs the archive does not embed.
    ///
    /// This walk answers "what can the archive OFFER", not closure truth,
    /// so gaps go through the shared policy's offer arm
    /// (`run::closure_gap`): a non-embedded interior
    /// input is the thin-archive shape — the target resolves it itself —
    /// skipped at `warn!` and returned in [`DrvClosure::skipped`] so the
    /// submitter can surface it on the batch record (when the target
    /// CANNOT resolve it, the per-root failure must be attributable to
    /// the archive instead of charged to the unit). A ROOT missing from
    /// the archive is an error naming the path: batches are planned from
    /// this archive's workload units, so a missing root means the archive
    /// is incomplete or corrupted.
    pub fn closure(&self, roots: &[String]) -> Result<DrvClosure> {
        /// Explicit DFS frame: visit parses the ATerm and queues input
        /// derivations, emit appends the path once everything it references
        /// is out.
        enum Frame {
            Visit(String),
            Emit(String),
        }
        let embedded: HashSet<String> = self.archive.embedded_drvs().into_iter().collect();
        let root_set: HashSet<&str> = roots.iter().map(String::as_str).collect();
        // `entered` marks derivations whose ATerm has been read so each
        // member is parsed at most once and reference cycles in a corrupted
        // archive cannot loop the walk.
        let mut entered: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        // Roots are seeded in reverse so the LIFO stack walks them (and each
        // derivation's input list) in their given order.
        let mut stack: Vec<Frame> = roots
            .iter()
            .rev()
            .map(|r| Frame::Visit(r.clone()))
            .collect();
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Visit(path) => {
                    if !entered.insert(path.clone()) {
                        continue;
                    }
                    if !embedded.contains(&path) {
                        super::closure_gap::closure_gap(
                            super::closure_gap::ClosureGapPolicy::Offer {
                                is_root: root_set.contains(path.as_str()),
                            },
                            &path,
                            "is not embedded in the archive",
                        )?;
                        skipped.push(path);
                        continue;
                    }
                    let (_, parsed) = self.derivation(&path)?;
                    stack.push(Frame::Emit(path));
                    for input in parsed.input_drvs().keys().rev() {
                        if !entered.contains(input) {
                            stack.push(Frame::Visit(input.clone()));
                        }
                    }
                }
                Frame::Emit(path) => order.push(path),
            }
        }
        skipped.sort();
        Ok(DrvClosure { order, skipped })
    }

    /// Materialize one embedded derivation as a worker-protocol upload
    /// entry: its wire path-info plus the NAR-encoded ATerm text held in
    /// memory (derivation texts are tiny, so per-entry payloads stay small;
    /// callers control how many entries they hold at once).
    ///
    /// The path-info mirrors what a client-side derivation copy registers
    /// for a `.drv` store object: references are the derivation's input
    /// derivations and input sources, and the content address is the
    /// `text:sha256:` hash over the ATerm bytes.
    pub fn entry(&self, store_path: &str) -> Result<StoreEntry> {
        let (text, parsed) = self.derivation(store_path)?;
        let mut nar = Vec::new();
        rio_nix::nar::serialize(
            &mut nar,
            &NarNode::Regular {
                executable: false,
                contents: text.clone().into_bytes(),
            },
        )
        .with_context(|| format!("NAR-encode the derivation text of {store_path}"))?;
        let mut references: Vec<String> = parsed
            .input_drvs()
            .keys()
            .cloned()
            .chain(parsed.input_srcs().iter().cloned())
            .collect();
        references.sort();
        references.dedup();
        let content_address = format!(
            "text:sha256:{}",
            nixbase32::encode(&Sha256::digest(text.as_bytes()))
        );
        Ok(StoreEntry {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                deriver: None,
                nar_hash: Sha256::digest(&nar).to_vec(),
                references,
                registration_time: 0,
                nar_size: nar.len() as u64,
                ultimate: false,
                signatures: Vec::new(),
                content_address: Some(content_address),
            },
            nar: NarPayload::Bytes(nar),
        })
    }
}

#[cfg(test)]
mod tests {
    use rio_nix::nar::extract_single_file;
    use rio_nix::protocol::client::NarPayload;

    use super::*;
    use crate::run::archive_input::{load_units, write_mini_archive};

    /// Mini replay archive opened for an import test, plus the drv paths of
    /// the appB → libA → stdenv chain it embeds.
    fn open_chain() -> (tempfile::TempDir, Arc<ReplayArchive>, [String; 3]) {
        let tmp = tempfile::tempdir().unwrap();
        write_mini_archive(tmp.path());
        let archive = Arc::new(ReplayArchive::open(tmp.path()).unwrap());
        let app_b = load_units(&archive)
            .unwrap()
            .into_iter()
            .find(|u| u.job == "appB.x86_64-linux")
            .unwrap()
            .drv_path;
        let drv_for = |needle: &str| {
            archive
                .embedded_drvs()
                .into_iter()
                .find(|d| d.contains(needle))
                .unwrap()
        };
        let chain = [drv_for("-stdenv-"), drv_for("-libA-1"), app_b];
        (tmp, archive, chain)
    }

    /// Render the error of an `entry()` call that must fail. (`StoreEntry`
    /// carries a streaming payload variant and has no `Debug` impl, so
    /// `unwrap_err()` cannot be used on the result directly.)
    fn entry_err(archive: &DrvArchive, store_path: &str) -> String {
        match archive.entry(store_path) {
            Ok(_) => panic!("entry({store_path}) unexpectedly succeeded"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn closure_walks_references_before_referrers_and_entries_round_trip() {
        let (_tmp, archive, [stdenv, lib_a, app_b]) = open_chain();
        let drv_archive = DrvArchive::new(archive.clone());

        // appB reaches stdenv only through libA; references come out before
        // their referrers so uploads register dependencies first. A fully
        // embedded closure has nothing to skip.
        let closure = drv_archive.closure(std::slice::from_ref(&app_b)).unwrap();
        assert_eq!(
            closure.order,
            vec![stdenv.clone(), lib_a.clone(), app_b.clone()]
        );
        assert!(closure.skipped.is_empty());
        // Multiple roots and already-reached roots dedup.
        let closure_both = drv_archive
            .closure(&[app_b.clone(), lib_a.clone()])
            .unwrap();
        assert_eq!(
            closure_both.order,
            vec![stdenv.clone(), lib_a.clone(), app_b.clone()]
        );

        // The upload entry round-trips the embedded ATerm text and carries
        // the path-info a client-side derivation copy would register.
        let entry = drv_archive.entry(&lib_a).unwrap();
        assert_eq!(entry.store_path, lib_a);
        let nar = match entry.nar {
            NarPayload::Bytes(bytes) => bytes,
            _ => panic!("derivation entries are in-memory"),
        };
        assert_eq!(entry.info.nar_size, nar.len() as u64);
        assert_eq!(entry.info.nar_hash, Sha256::digest(&nar).to_vec());
        let text = archive.read_drv(&lib_a).unwrap();
        assert_eq!(extract_single_file(&nar).unwrap(), text.as_bytes());
        assert_eq!(entry.info.references, vec![stdenv.clone()]);
        assert_eq!(entry.info.deriver, None);
        assert!(entry.info.signatures.is_empty());
        assert_eq!(
            entry.info.content_address.as_deref(),
            Some(
                format!(
                    "text:sha256:{}",
                    nixbase32::encode(&Sha256::digest(text.as_bytes()))
                )
                .as_str()
            )
        );

        // A root absent from the archive is an error naming the path; a
        // non-derivation member is refused by the reader.
        let missing = "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-none.drv".to_string();
        let err = format!(
            "{:#}",
            drv_archive
                .closure(std::slice::from_ref(&missing))
                .unwrap_err()
        );
        assert!(
            err.contains("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-none.drv"),
            "{err}"
        );
        let err = entry_err(&drv_archive, &missing);
        assert!(err.contains("not present in the archive"), "{err}");
    }

    #[test]
    fn closure_skips_inputs_the_archive_does_not_embed() {
        use crate::archive::schema::{Capabilities, RequestRecord, RequestTarget, Substituters};
        use crate::archive::writer::{ArchiveWriter, ManifestSeed};
        use crate::run::archive_input::fake_hash;

        // A two-derivation archive whose extra (non-target) derivation
        // references one embedded derivation, one derivation the archive does
        // not embed, and an input source: the walk imports what the archive
        // can offer and skips the rest, while the upload entry still declares
        // every reference.
        let tmp = tempfile::tempdir().unwrap();
        let base_drv = format!("/nix/store/{}-base-1.0.drv", fake_hash("base-drv"));
        let base_out = format!("/nix/store/{}-base-1.0", fake_hash("base-out"));
        let extra_drv = format!("/nix/store/{}-extra-1.0.drv", fake_hash("extra-drv"));
        let extra_out = format!("/nix/store/{}-extra-1.0", fake_hash("extra-out"));
        let absent_drv = "/nix/store/yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy-absent.drv";
        let src = "/nix/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-src.tar.gz";
        let writer = ArchiveWriter::create(tmp.path()).unwrap();
        writer
            .add_drv(
                &base_drv,
                &format!(
                    r#"Derive([("out","{base_out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{base_out}")])"#
                ),
            )
            .unwrap();
        writer
            .add_drv(
                &extra_drv,
                &format!(
                    r#"Derive([("out","{extra_out}","","")],[("{absent_drv}",["out"]),("{base_drv}",["out"])],["{src}"],"x86_64-linux","/bin/sh",["-c","true"],[("out","{extra_out}")])"#
                ),
            )
            .unwrap();
        // Only the dependency-free derivation is a workload target, so the
        // writer's closure-completeness walk tolerates the extra member's
        // non-embedded input.
        writer
            .write_requests(&[RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: base_drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            }])
            .unwrap();
        let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
        writer
            .finalize(ManifestSeed {
                created_at: stamp,
                from: stamp,
                to: stamp,
                capabilities: Capabilities::default(),
                substituters: Substituters {
                    relay: vec!["https://cache.example.org".to_string()],
                    target: Vec::new(),
                },
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();
        let archive = Arc::new(ReplayArchive::open(tmp.path()).unwrap());
        let drv_archive = DrvArchive::new(archive);

        let closure = drv_archive
            .closure(std::slice::from_ref(&extra_drv))
            .unwrap();
        assert_eq!(closure.order, vec![base_drv.clone(), extra_drv.clone()]);
        assert_eq!(
            closure.skipped,
            vec![absent_drv.to_string()],
            "the offered set names what it could not offer — the gap is \
             reported, not swallowed (the input source is not a derivation \
             and is never walked)"
        );

        let entry = drv_archive.entry(&extra_drv).unwrap();
        let mut want_refs = vec![base_drv, absent_drv.to_string(), src.to_string()];
        want_refs.sort();
        assert_eq!(entry.info.references, want_refs);
    }
}
