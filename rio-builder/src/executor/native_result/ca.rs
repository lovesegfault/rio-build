//! Floating content-addressed output finalization.
//!
//! Peak memory: the full (rewritten) NAR of each CA output is held in
//! memory during hashing/finalization — the operative bound for very
//! large content-addressed outputs. Flat-method outputs additionally
//! read the file once more for hashing; TODO: collapse that double
//! read by hashing flat-method outputs from the bytes already held in
//! memory.
//!
//! A floating-CA output (`outputHashAlgo` set, no declared path, no
//! declared hash) is built into a deterministic *scratch* path because
//! its real store path depends on its own content. After the build, the
//! real path is derived and the content is moved there — this module is
//! that derivation, mirroring CppNix's `DerivationBuilderImpl::
//! registerOutputs` rewrite branch:
//!
//! 1. apply the scratch→final rewrites of every already-finalized
//!    sibling to this output's content (so a reference to a sibling is
//!    hashed at its *final* path — the order the design doc calls
//!    "rewrites before hashing");
//! 2. hash the rewritten content modulo this output's *own* scratch
//!    hash ([`HashModuloSink`]) with the declared algorithm;
//! 3. derive the final path
//!    ([`StorePath::make_fixed_output_with_self`]) from that hash, the
//!    sibling-remapped references, and whether a self-reference was
//!    seen;
//! 4. rewrite this output's own scratch hash to the final hash, restore
//!    the rewritten NAR at the final on-disk location, and record the
//!    mapping for later siblings;
//! 5. recompute `narHash`/`narSize` over the final bytes.
//!
//! The whole pass works on the NAR serialization held in memory (one
//! buffer per output, exactly like CppNix's `rewriteOutput`, which
//! round-trips the NAR through a `std::string`): hash-part rewrites
//! must reach symlink targets and file *names* too, and the NAR is the
//! only representation that exposes all of them uniformly. Outputs
//! whose content does not change (no sibling hits, no self-reference)
//! skip the restore entirely and are renamed in place.
//!
//! Mixed shapes (a floating-CA output next to an input-addressed or
//! fixed-output sibling) are rejected outright — CppNix's
//! `BasicDerivation::type()` refuses to even classify them ("can't mix
//! derivation output types"), and finalizing one here would remap the
//! non-CA sibling's references without ever rewriting its bytes. The
//! request glue rejects the shape before planning; `from_outputs`
//! re-checks it as defense in depth.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use sha2::Digest;
use tracing::debug;

use rio_nix::ca::{HashModuloSink, RewritingSink};
use rio_nix::derivation::DerivationOutput;
use rio_nix::hash::HashAlgo;
use rio_nix::store_path::StorePath;

use super::{OutputRejection, ProcessedOutput, canonicalise};

/// How one floating-CA output is to be ingested: `(recursive, algo)`.
///
/// Derived from the derivation's `outputHashAlgo` (`"r:sha256"` →
/// recursive SHA-256, `"sha256"` → flat SHA-256, …). Outputs that are
/// not floating-CA (declared path, or declared hash = a FOD) are not in
/// the map.
#[derive(Debug, Default)]
pub(crate) struct FloatingCaSpec {
    methods: HashMap<String, (bool, HashAlgo)>,
}

impl FloatingCaSpec {
    /// Identify the floating-CA outputs of a derivation: `path` empty,
    /// `hash_algo` set, `hash` empty (a set hash would make it a
    /// fixed-output derivation, which is verified — not finalized).
    ///
    /// Mixed shapes (a floating-CA output next to any other kind) are
    /// unrepresentable past the typed parse boundary ("can't mix
    /// derivation output types" fires at construction), so the pipeline
    /// can never be asked to finalize a shape where a non-CA sibling's
    /// bytes would be left unrewritten — there is no mixed re-check
    /// left to run.
    // r[impl builder.exec.output-types-unmixed+1]
    pub(crate) fn from_outputs(outputs: &[DerivationOutput]) -> Result<Self, OutputRejection> {
        use rio_nix::derivation::OutputKind;
        let mut methods = HashMap::new();
        for o in outputs {
            let OutputKind::Floating { hash_algo: raw } = o.kind() else {
                continue;
            };
            let (recursive, algo_str) = match raw.strip_prefix("r:") {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let algo = match algo_str {
                "sha1" => HashAlgo::SHA1,
                "sha256" => HashAlgo::SHA256,
                "sha512" => HashAlgo::SHA512,
                _ => {
                    return Err(OutputRejection::CaUnsupportedAlgo {
                        output: o.name().to_owned(),
                        algo: raw.to_owned(),
                    });
                }
            };
            methods.insert(o.name().to_owned(), (recursive, algo));
        }
        Ok(Self { methods })
    }

    /// True if the derivation has any floating-CA outputs at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }
}

/// Finalize every floating-CA output of `outputs` (already in
/// topological order, dependencies first), updating each finalized
/// output's `store_path`, `host_path`, `nar_hash`, `nar_size`,
/// `references`, and `content_address` in place, and remapping
/// references to finalized siblings in *every* output (CA or not).
///
/// On success the on-disk scratch trees have been replaced by trees at
/// the final store paths' basenames in the same parent directory.
// r[impl builder.exec.ca-finalize]
pub(crate) fn finalize_floating_ca(
    outputs: &mut [ProcessedOutput],
    spec: &FloatingCaSpec,
) -> Result<(), OutputRejection> {
    if spec.is_empty() {
        return Ok(());
    }

    // scratch hash part → final hash part (content rewrites for later
    // siblings) and scratch full path → final full path (reference
    // remapping).
    let mut hash_rewrites: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut path_rewrites: BTreeMap<String, String> = BTreeMap::new();

    for out in outputs.iter_mut() {
        // Remap references to already-finalized siblings first — this
        // applies to every output (an input-addressed output may
        // legitimately reference a CA sibling) and must happen before
        // this output's own path computation uses the reference set.
        for r in &mut out.references {
            if let Some(final_path) = path_rewrites.get(r) {
                *r = final_path.clone();
            }
        }

        let Some(&(recursive, algo)) = spec.methods.get(&out.name) else {
            continue;
        };

        let scratch =
            StorePath::parse(&out.store_path).map_err(|e| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("scratch path {} does not parse: {e}", out.store_path),
            })?;
        let scratch_hash = scratch.hash_part();

        // Flat ingestion: must be a single non-executable regular file
        // (CppNix rejects anything else before hashing).
        if !recursive {
            let md = std::fs::symlink_metadata(&out.host_path).map_err(|e| {
                OutputRejection::CaFinalize {
                    output: out.name.clone(),
                    message: format!("stat {}: {e}", out.host_path.display()),
                }
            })?;
            use std::os::unix::fs::PermissionsExt;
            if !md.is_file() || md.permissions().mode() & 0o111 != 0 {
                return Err(OutputRejection::CaFlatNotSingleFile {
                    output: out.name.clone(),
                });
            }
        }

        // One NAR pass: dump the output with the accumulated sibling
        // rewrites applied. `sibling_hits` tells us whether the bytes
        // actually changed.
        let (nar_buf, sibling_hits) =
            dump_with_rewrites(&out.host_path, &hash_rewrites).map_err(|e| {
                OutputRejection::CaFinalize {
                    output: out.name.clone(),
                    message: format!("serializing {}: {e}", out.host_path.display()),
                }
            })?;

        // Hash modulo the output's own scratch hash. Recursive method
        // hashes the NAR; flat hashes the file bytes — with the
        // accumulated sibling scratch→final rewrites applied first, so
        // the hash describes the bytes that are actually restored and
        // uploaded (CppNix runs `rewriteOutput` before hashing in flat
        // mode too). Textual store references CAN appear in flat
        // content even though `make_fixed_output_with_self` rejects
        // *recorded* flat references: structured-attrs
        // `unsafeDiscardReferences` empties the recorded set while
        // leaving the bytes untouched.
        let (modulo_hash, self_hits) = if recursive {
            let mut sink = HashModuloSink::new(algo, &scratch_hash);
            sink.write_all(&nar_buf).expect("hashing is infallible");
            sink.finish()
        } else {
            let mut bytes =
                std::fs::read(&out.host_path).map_err(|e| OutputRejection::CaFinalize {
                    output: out.name.clone(),
                    message: format!("reading {}: {e}", out.host_path.display()),
                })?;
            for (from, to) in &hash_rewrites {
                replace_in_buf(&mut bytes, from, to);
            }
            let mut sink = HashModuloSink::new(algo, &scratch_hash);
            sink.write_all(&bytes).expect("hashing is infallible");
            sink.finish()
        };
        // The `:self` fingerprint flag comes from the *recorded* reference
        // set, exactly like CppNix derives it from the scanned references:
        // under structured-attrs `unsafeDiscardReferences` the recorded set
        // is empty, so the path is minted WITHOUT the self token even when
        // the bytes embed the output's own scratch hash. The embedded hash
        // is still rewritten to the final hash below (`self_hits` keeps
        // deciding that), mirroring CppNix's unconditional rewriteOutput —
        // and the store gate accepts that shape because the `fixed:`
        // descriptor carries the same modulo hash.
        let self_referenced = out.references.contains(&out.store_path);

        // References for the path fingerprint: the (already remapped)
        // scanned references, excluding the output itself (self is
        // expressed via the flag).
        let refs_for_path: Vec<StorePath> = out
            .references
            .iter()
            .filter(|r| *r != &out.store_path)
            .map(|r| StorePath::parse(r))
            .collect::<Result<_, _>>()
            .map_err(|e| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("reference does not parse: {e}"),
            })?;

        let final_path = StorePath::make_fixed_output_with_self(
            scratch.name(),
            &modulo_hash,
            recursive,
            &refs_for_path,
            self_referenced,
        )
        .map_err(|e| OutputRejection::CaFinalize {
            output: out.name.clone(),
            message: format!("computing the content-addressed path: {e}"),
        })?;
        let final_str = final_path.as_str().to_owned();
        let final_hash = final_path.hash_part();

        // The Nix content-address descriptor served to substituting
        // clients (`narinfo CA:` / `PathInfo.ca`).
        let ca_descriptor = format!(
            "fixed:{}{}",
            if recursive { "r:" } else { "" },
            modulo_hash.to_colon()
        );

        let scratch_str = out.store_path.clone();
        // Rewriting is keyed on what the bytes contain (textual hits), not
        // on whether the self-reference is recorded — a discarded
        // self-reference still gets its scratch hash rewritten to the
        // final hash, like CppNix.
        let content_changed = sibling_hits > 0 || self_hits > 0;

        debug!(
            output = %out.name,
            scratch = %scratch_str,
            final_path = %final_str,
            self_referenced,
            self_hits,
            sibling_hits,
            "finalizing floating-CA output"
        );

        // Materialize the content at the final location.
        let parent = out
            .host_path
            .parent()
            .ok_or_else(|| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("output host path {} has no parent", out.host_path.display()),
            })?
            .to_path_buf();
        let new_host = parent.join(final_path.basename());

        let (final_nar_hash, final_nar_size) = if content_changed {
            // Rewrite this output's own scratch hash to the final hash
            // in the NAR buffer, restore it at the final location, and
            // re-normalize the metadata the restore created as us.
            let mut buf = nar_buf;
            replace_in_buf(&mut buf, scratch_hash.as_bytes(), final_hash.as_bytes());

            let staging = parent.join(format!(".rio-ca-{final_hash}"));
            remove_tree_force(&staging).map_err(|e| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("clearing stale staging dir: {e}"),
            })?;
            rio_nix::nar::restore_path_streaming(&mut &buf[..], &staging).map_err(|e| {
                OutputRejection::CaFinalize {
                    output: out.name.clone(),
                    message: format!("restoring rewritten output: {e}"),
                }
            })?;
            // The restored tree is owned by *us* (the trusted executor),
            // not the build user — canonicalise it against our own uid
            // purely for the metadata normalization (modes, mtime); the
            // untrusted content was already checked before finalization.
            canonicalise::canonicalise_output(
                &staging,
                nix::unistd::geteuid().as_raw(),
                &mut HashSet::new(),
            )?;

            remove_tree_force(&new_host).map_err(|e| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("clearing the final path before the move: {e}"),
            })?;
            std::fs::rename(&staging, &new_host).map_err(|e| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("moving finalized output into place: {e}"),
            })?;
            // Drop the scratch tree — its content now lives (rewritten)
            // at the final path.
            remove_tree_force(&out.host_path).map_err(|e| OutputRejection::CaFinalize {
                output: out.name.clone(),
                message: format!("removing the scratch tree: {e}"),
            })?;

            let nar_hash: [u8; 32] = sha2::Sha256::digest(&buf).into();
            (nar_hash, buf.len() as u64)
        } else {
            // Content is byte-identical (no references to rewrite):
            // rename the scratch tree to the final basename and keep
            // the NAR hash/size already computed by the scan pass.
            if new_host != out.host_path {
                remove_tree_force(&new_host).map_err(|e| OutputRejection::CaFinalize {
                    output: out.name.clone(),
                    message: format!("clearing the final path before the move: {e}"),
                })?;
                std::fs::rename(&out.host_path, &new_host).map_err(|e| {
                    OutputRejection::CaFinalize {
                        output: out.name.clone(),
                        message: format!("renaming output to its final path: {e}"),
                    }
                })?;
            }
            (out.nar_hash, out.nar_size)
        };

        // Record the mapping for later siblings, then update this
        // output's own record (including the self-reference, which now
        // points at the final path).
        if final_str != scratch_str {
            hash_rewrites.push((
                scratch_hash.clone().into_bytes(),
                final_hash.clone().into_bytes(),
            ));
            path_rewrites.insert(scratch_str.clone(), final_str.clone());
        }

        for r in &mut out.references {
            if *r == scratch_str {
                *r = final_str.clone();
            }
        }
        out.references.sort();
        out.references.dedup();
        out.store_path = final_str;
        out.host_path = new_host;
        out.nar_hash = final_nar_hash;
        out.nar_size = final_nar_size;
        out.content_address = Some(ca_descriptor);
    }

    Ok(())
}

/// Remove a canonicalised tree (or file). Canonicalisation strips the
/// write bit from directories (0555), which makes a plain
/// `remove_dir_all` fail with EACCES for non-root users — restore the
/// owner write bit on directories on the way down, exactly like
/// CppNix's `deletePath`.
fn remove_tree_force(path: &Path) -> std::io::Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if md.is_dir() {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = md.permissions();
        if perms.mode() & 0o200 == 0 {
            perms.set_mode(perms.mode() | 0o200);
            std::fs::set_permissions(path, perms)?;
        }
        for entry in std::fs::read_dir(path)? {
            remove_tree_force(&entry?.path())?;
        }
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// NAR-serialize `path`, applying `rewrites` on the way, into a buffer.
/// Returns the buffer and the number of rewrite hits.
fn dump_with_rewrites(
    path: &Path,
    rewrites: &[(Vec<u8>, Vec<u8>)],
) -> std::io::Result<(Vec<u8>, u64)> {
    if rewrites.is_empty() {
        let mut buf = Vec::new();
        rio_nix::nar::dump_path_streaming(path, &mut buf)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        return Ok((buf, 0));
    }
    let sink = RewritingSink::new(rewrites.iter().cloned(), Vec::new())
        .expect("hash-part rewrite pairs are non-empty and equal-length");
    let mut sink = sink;
    rio_nix::nar::dump_path_streaming(path, &mut sink)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let (buf, hits) = sink.finish()?;
    Ok((buf, hits))
}

/// In-place, non-overlapping replacement of `from` with `to`
/// (`from.len() == to.len()`) in `buf`. Used for the output's *own*
/// scratch→final rewrite, which happens on an already-materialized
/// buffer.
fn replace_in_buf(buf: &mut [u8], from: &[u8], to: &[u8]) {
    debug_assert_eq!(from.len(), to.len());
    if from.is_empty() || buf.len() < from.len() {
        return;
    }
    let mut i = 0;
    while i + from.len() <= buf.len() {
        if &buf[i..i + from.len()] == from {
            buf[i..i + to.len()].copy_from_slice(to);
            i += from.len();
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_in_buf_handles_adjacent_and_no_match() {
        let mut buf = b"xxabcabcyy".to_vec();
        replace_in_buf(&mut buf, b"abc", b"def");
        assert_eq!(buf, b"xxdefdefyy");

        let mut buf = b"nothing here".to_vec();
        replace_in_buf(&mut buf, b"abc", b"def");
        assert_eq!(buf, b"nothing here");

        let mut short = b"ab".to_vec();
        replace_in_buf(&mut short, b"abc", b"def");
        assert_eq!(short, b"ab");
    }

    /// Build a scratch output directory containing one file that embeds
    /// the scratch store path, plus the matching `ProcessedOutput` and a
    /// recursive-SHA256 `FloatingCaSpec` for it.
    fn scratch_fixture(
        tmp: &Path,
        references: Vec<String>,
    ) -> (ProcessedOutput, FloatingCaSpec, StorePath) {
        let drv =
            StorePath::parse("/nix/store/00000000000000000000000000000000-rio-ca-self-test.drv")
                .unwrap();
        let scratch = StorePath::make_scratch_output_path(&drv, "out").unwrap();

        let host = tmp.join("scratch-out");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(
            host.join("marker"),
            format!("I live at {}\n", scratch.as_str()),
        )
        .unwrap();

        let out = ProcessedOutput {
            name: "out".into(),
            store_path: scratch.as_str().to_owned(),
            host_path: host,
            nar_hash: [0u8; 32],
            nar_size: 0,
            references,
            content_address: None,
        };
        let spec = FloatingCaSpec {
            methods: HashMap::from([("out".to_owned(), (true, HashAlgo::SHA256))]),
        };
        (out, spec, scratch)
    }

    /// Expected modulo hash + final path for the fixture above, computed
    /// independently of the code under test.
    fn expected_final(scratch: &StorePath, host: &Path, self_flag: bool) -> (String, StorePath) {
        let nar = rio_nix::nar::dump_path(host).unwrap();
        let mut sink = HashModuloSink::new(HashAlgo::SHA256, &scratch.hash_part());
        sink.write_all(&nar).unwrap();
        let (modulo, _) = sink.finish();
        let path =
            StorePath::make_fixed_output_with_self(scratch.name(), &modulo, true, &[], self_flag)
                .unwrap();
        (format!("fixed:r:{}", modulo.to_colon()), path)
    }

    /// `unsafeDiscardReferences` empties the recorded reference set before
    /// finalization: the path must be minted WITHOUT the `:self` token
    /// (CppNix derives the flag from the recorded references), the
    /// references must stay empty, and the embedded scratch hash must
    /// still be rewritten to the final hash.
    #[test]
    fn discarded_self_reference_omits_the_self_token_but_still_rewrites() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut out, spec, scratch) = scratch_fixture(tmp.path(), vec![]);
        let (want_descriptor, want_path) =
            expected_final(&scratch, &out.host_path, /* self */ false);
        let (_, with_self) = expected_final(&scratch, &out.host_path, /* self */ true);

        finalize_floating_ca(std::slice::from_mut(&mut out), &spec).unwrap();

        assert_eq!(out.store_path, want_path.as_str(), "path must omit :self");
        assert_ne!(
            out.store_path,
            with_self.as_str(),
            "the self flag must actually change the fingerprint"
        );
        assert!(out.references.is_empty(), "discarded references stay empty");
        assert_eq!(
            out.content_address.as_deref(),
            Some(want_descriptor.as_str())
        );

        let rewritten = std::fs::read_to_string(out.host_path.join("marker")).unwrap();
        assert!(
            rewritten.contains(&want_path.hash_part()),
            "embedded hash must be rewritten to the final hash"
        );
        assert!(
            !rewritten.contains(&scratch.hash_part()),
            "the scratch hash must not survive finalization"
        );
    }

    /// A *recorded* self-reference (the normal, non-discarded case) keeps
    /// the `:self` fingerprint token and is remapped to the final path in
    /// the recorded reference set.
    #[test]
    fn declared_self_reference_keeps_the_self_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut out, spec, scratch) = scratch_fixture(
            tmp.path(),
            vec![/* recorded self reference */ String::new()],
        );
        out.references[0] = scratch.as_str().to_owned();
        let (_, want_path) = expected_final(&scratch, &out.host_path, /* self */ true);

        finalize_floating_ca(std::slice::from_mut(&mut out), &spec).unwrap();

        assert_eq!(out.store_path, want_path.as_str(), "path must carry :self");
        assert_eq!(
            out.references,
            vec![want_path.as_str().to_owned()],
            "the recorded self-reference must be remapped to the final path"
        );
    }
}
