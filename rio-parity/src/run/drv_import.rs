//! Derivation-closure import source backed by the eval set's drv archive.
//!
//! The eval set's `drvs.tar.zst` untars to an uncompressed `file://`
//! binary-cache layout (`nix-cache-info`, one `<hash>.narinfo` per path,
//! `nar/…` payloads) — the same files `nix copy --derivation --from
//! file://…` reads. [`DrvArchive`] walks a batch's derivation closure
//! inside that layout and materializes each path as a worker-protocol
//! upload entry, so the client-ops submitter can import derivation texts
//! over `AddMultipleToStore` without a local Nix store or a `nix` binary.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use rio_nix::narinfo::NarInfo;
use rio_nix::protocol::client::{NarPayload, StoreEntry};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use rio_nix::store_path::StorePath;
use sha2::{Digest as _, Sha256};

/// Untarred eval-set drv archive: an uncompressed `file://` binary-cache
/// layout holding the `.drv` closure of every manifest target.
///
/// Construction is metadata-only; narinfos and NAR payloads are read on
/// demand per call, so memory stays proportional to one path (plus the
/// closure's path strings), never to the whole archive.
#[derive(Debug)]
pub struct DrvArchive {
    /// Layout root — the directory `drvs.tar.zst` was untarred into.
    dir: PathBuf,
}

impl DrvArchive {
    /// Open an untarred archive layout. The directory must exist; a missing
    /// `nix-cache-info` is only a warning so minimal layouts (tests, hand-built
    /// fixtures) still open.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        ensure!(
            dir.is_dir(),
            "drv archive directory {} does not exist",
            dir.display()
        );
        if !dir.join("nix-cache-info").exists() {
            tracing::warn!(
                dir = %dir.display(),
                "drv archive has no nix-cache-info; treating it as a bare narinfo layout"
            );
        }
        Ok(Self { dir })
    }

    /// Read and parse `<hash-part>.narinfo` for `store_path`. `Ok(None)` when
    /// the path has no narinfo in the archive.
    fn narinfo_for(&self, store_path: &str) -> Result<Option<NarInfo>> {
        let parsed = StorePath::parse(store_path)
            .map_err(|e| anyhow::anyhow!("not a store path: {store_path}: {e}"))?;
        let file = self.dir.join(format!("{}.narinfo", parsed.hash_part()));
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
        };
        let info =
            NarInfo::parse(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", file.display()))?;
        Ok(Some(info))
    }

    /// Walk the in-archive reference closure of `roots` and return it in
    /// reference order: every path appears after all of its in-archive
    /// references (post-order), deduplicated across roots.
    ///
    /// References with no narinfo in the archive are skipped with a debug
    /// log — the archive only carries derivation texts, so anything else is
    /// either already on the target or not importable from here (the same
    /// set `nix copy` from this layout could offer). A ROOT with no narinfo
    /// is an error naming the path: batches are planned from this eval set,
    /// so a missing root means the archive is incomplete or corrupted.
    pub fn closure(&self, roots: &[String]) -> Result<Vec<String>> {
        /// Explicit DFS frame: visit reads the narinfo and queues references,
        /// emit appends the path once everything it references is out.
        enum Frame {
            Visit(String),
            Emit(String),
        }
        let root_set: HashSet<&str> = roots.iter().map(String::as_str).collect();
        // `entered` marks paths whose narinfo has been read (or found absent)
        // so each archive file is read at most once and reference cycles in a
        // corrupted archive cannot loop the walk.
        let mut entered: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();
        // Roots are seeded in reverse so the LIFO stack walks them (and each
        // narinfo's reference list) in their given order.
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
                    let Some(info) = self.narinfo_for(&path)? else {
                        if root_set.contains(path.as_str()) {
                            bail!(
                                "root {path} has no narinfo in the drv archive at {} \
                                 (incomplete or corrupted eval set)",
                                self.dir.display()
                            );
                        }
                        tracing::debug!(
                            %path,
                            "reference not in the drv archive; skipping (not importable from here)"
                        );
                        continue;
                    };
                    stack.push(Frame::Emit(path));
                    for reference in info.references.iter().rev() {
                        let full = full_store_path(reference);
                        if !entered.contains(&full) {
                            stack.push(Frame::Visit(full));
                        }
                    }
                }
                Frame::Emit(path) => order.push(path),
            }
        }
        Ok(order)
    }

    /// Materialize one archive path as a worker-protocol upload entry: its
    /// wire path-info plus the NAR bytes held in memory (derivation texts are
    /// tiny, so per-entry payloads stay small; callers control how many
    /// entries they hold at once).
    ///
    /// The archive is packed with `compression=none`, so the `nar/…` payload
    /// bytes ARE the NAR serialization; any other `Compression:` value is an
    /// error rather than something to transparently decompress. The payload
    /// is verified against the narinfo's `NarSize`/`NarHash` before being
    /// offered for upload — a corrupted archive must not be uploaded.
    pub fn entry(&self, store_path: &str) -> Result<StoreEntry> {
        let info = self.narinfo_for(store_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no narinfo for {store_path} in the drv archive at {}",
                self.dir.display()
            )
        })?;
        ensure!(
            info.store_path == store_path,
            "narinfo for {store_path} names a different store path ({}) — corrupted archive",
            info.store_path
        );
        ensure!(
            info.compression == "none",
            "narinfo for {store_path} declares Compression: {} (the drv archive is packed \
             uncompressed); refusing to upload it",
            info.compression
        );
        // The URL must stay inside the layout: the archive is produced by the
        // eval-set builder, but it travels through S3 and shared volumes, so
        // never follow it out of the directory.
        ensure!(
            !info.url.starts_with('/')
                && !Path::new(&info.url)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
            "narinfo for {store_path} has a non-relative NAR URL {:?}",
            info.url
        );
        let nar_file = self.dir.join(&info.url);
        let bytes = std::fs::read(&nar_file)
            .with_context(|| format!("read NAR payload {} for {store_path}", nar_file.display()))?;
        ensure!(
            bytes.len() as u64 == info.nar_size,
            "NAR payload for {store_path} is {} bytes but its narinfo declares NarSize: {} — \
             corrupted archive",
            bytes.len(),
            info.nar_size
        );
        let nar_hash = decode_nar_hash(&info.nar_hash)
            .with_context(|| format!("narinfo NarHash for {store_path}"))?;
        ensure!(
            Sha256::digest(&bytes).as_slice() == nar_hash.as_slice(),
            "NAR payload for {store_path} does not match its narinfo NarHash — corrupted archive"
        );
        let references = info.references.iter().map(|r| full_store_path(r)).collect();
        Ok(StoreEntry {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                deriver: info.deriver.as_deref().map(full_store_path),
                nar_hash,
                references,
                registration_time: 0,
                nar_size: info.nar_size,
                ultimate: false,
                signatures: info.sigs,
                content_address: info.ca,
            },
            nar: NarPayload::Bytes(bytes),
        })
    }
}

/// Expand a narinfo basename (`<hash>-<name>`) to a full store path. The
/// narinfo text format stores references and the deriver as basenames, but
/// the wire `ValidPathInfo` and the closure walk use full paths; this is the
/// single place the prefix is added (already-full paths pass through).
fn full_store_path(name: &str) -> String {
    if name.starts_with("/nix/store/") {
        name.to_string()
    } else {
        format!("/nix/store/{name}")
    }
}

/// Decode a narinfo `NarHash:` value to its raw 32 SHA-256 bytes, via the
/// crate's existing NarHash normalizer (accepts the nixbase32 form `nix copy`
/// writes and the hex form rio-store records).
fn decode_nar_hash(nar_hash: &str) -> Result<Vec<u8>> {
    let hex_form = crate::nixcache::narhash_to_hex(nar_hash)?;
    hex::decode(&hex_form).with_context(|| format!("decode NarHash {nar_hash}"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rio_nix::protocol::client::NarPayload;
    use rio_nix::store_path::{StorePath, nixbase32};
    use sha2::{Digest as _, Sha256};

    use super::*;

    /// Fixed test derivation paths: `b.drv` references `a.drv`.
    const A_DRV: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv";
    const B_DRV: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv";

    /// NAR-encode `text` as a single-file store path and add it to the
    /// binary-cache layout at `layout`: `nar/<hash-part>.nar` plus
    /// `<hash-part>.narinfo` (`Compression: none`, references as basenames,
    /// optional `CA:`). Returns the NAR bytes for later assertions.
    fn add_layout_path(
        work: &Path,
        layout: &Path,
        store_path: &str,
        text: &str,
        references: &[&str],
        ca: Option<&str>,
    ) -> Vec<u8> {
        let hash_part = StorePath::parse(store_path).unwrap().hash_part();
        let src = work.join(format!("{hash_part}.aterm"));
        std::fs::write(&src, text).unwrap();
        let mut nar = Vec::new();
        rio_nix::nar::dump_path_streaming(&src, &mut nar).unwrap();

        std::fs::create_dir_all(layout.join("nar")).unwrap();
        std::fs::write(layout.join("nar").join(format!("{hash_part}.nar")), &nar).unwrap();

        let nar_hash_b32 = nixbase32::encode(&Sha256::digest(&nar));
        let refs_line = references
            .iter()
            .map(|r| r.strip_prefix("/nix/store/").unwrap_or(r))
            .collect::<Vec<_>>()
            .join(" ");
        let mut narinfo = format!(
            "StorePath: {store_path}\nURL: nar/{hash_part}.nar\nCompression: none\n\
             NarHash: sha256:{nar_hash_b32}\nNarSize: {}\nReferences: {refs_line}\n",
            nar.len()
        );
        if let Some(ca) = ca {
            narinfo.push_str(&format!("CA: {ca}\n"));
        }
        std::fs::write(layout.join(format!("{hash_part}.narinfo")), narinfo).unwrap();
        nar
    }

    /// Content address of a derivation text the way `nix copy --derivation`
    /// records it (`text:sha256:<nixbase32 of the text hash>`).
    fn text_ca(text: &str) -> String {
        format!(
            "text:sha256:{}",
            nixbase32::encode(&Sha256::digest(text.as_bytes()))
        )
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

    /// Build the minimal two-path layout (`b.drv` references `a.drv`) and
    /// return the tempdir guard, the layout dir, and both NAR payloads.
    fn two_path_layout() -> (tempfile::TempDir, PathBuf, Vec<u8>, Vec<u8>) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = tmp.path().join("drv-archive");
        std::fs::create_dir_all(&layout).unwrap();
        std::fs::write(layout.join("nix-cache-info"), "StoreDir: /nix/store\n").unwrap();
        let a_nar = add_layout_path(tmp.path(), &layout, A_DRV, "Derive(a)", &[], None);
        let b_text = "Derive(b)";
        let b_nar = add_layout_path(
            tmp.path(),
            &layout,
            B_DRV,
            b_text,
            &[A_DRV],
            Some(&text_ca(b_text)),
        );
        (tmp, layout, a_nar, b_nar)
    }

    #[test]
    fn closure_walks_references_before_referrers_and_entries_round_trip() -> anyhow::Result<()> {
        let (_tmp, layout, a_nar, b_nar) = two_path_layout();
        let a_drv = A_DRV.to_string();
        let b_drv = B_DRV.to_string();

        let archive = DrvArchive::open(&layout)?;
        let closure = archive.closure(std::slice::from_ref(&b_drv))?;
        assert_eq!(
            closure,
            vec![a_drv.clone(), b_drv.clone()],
            "references before referrers"
        );
        // Multiple roots and already-reached roots dedup.
        let closure_both = archive.closure(&[b_drv.clone(), a_drv.clone()])?;
        assert_eq!(closure_both, vec![a_drv.clone(), b_drv.clone()]);

        let entry = archive.entry(&a_drv)?;
        assert_eq!(entry.store_path, a_drv);
        assert_eq!(entry.info.nar_size, a_nar.len() as u64);
        assert_eq!(entry.info.nar_hash, Sha256::digest(&a_nar).to_vec());
        assert_eq!(entry.info.references, Vec::<String>::new());
        assert_eq!(entry.info.deriver, None);
        assert_eq!(entry.info.registration_time, 0);
        assert!(!entry.info.ultimate);
        assert_eq!(entry.info.content_address, None, "absent CA is tolerated");
        match entry.nar {
            NarPayload::Bytes(bytes) => assert_eq!(bytes, a_nar),
            _ => panic!("small entries are in-memory"),
        }

        // The referrer's references come back as full store paths and its
        // CA is passed through verbatim.
        let entry_b = archive.entry(&b_drv)?;
        assert_eq!(entry_b.info.references, vec![a_drv.clone()]);
        assert_eq!(entry_b.info.content_address, Some(text_ca("Derive(b)")));
        assert_eq!(entry_b.info.nar_size, b_nar.len() as u64);

        // A root absent from the archive is an error naming the path.
        let missing =
            archive.closure(&["/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-none.drv".into()]);
        let err = format!("{:#}", missing.unwrap_err());
        assert!(
            err.contains("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-none.drv"),
            "error must name the missing root, got: {err}"
        );
        Ok(())
    }

    #[test]
    fn closure_skips_non_root_references_missing_from_the_archive() -> anyhow::Result<()> {
        let (_tmp, layout, _a_nar, _b_nar) = two_path_layout();
        // c.drv references both b.drv (in the archive) and an input source
        // that is not part of the drv archive; the walk imports what the
        // archive can offer and skips the rest.
        let c_drv = "/nix/store/cccccccccccccccccccccccccccccccc-c.drv";
        let absent_src = "/nix/store/dddddddddddddddddddddddddddddddd-src.tar.gz";
        add_layout_path(
            layout.parent().unwrap(),
            &layout,
            c_drv,
            "Derive(c)",
            &[B_DRV, absent_src],
            None,
        );

        let archive = DrvArchive::open(&layout)?;
        let closure = archive.closure(&[c_drv.to_string()])?;
        assert_eq!(
            closure,
            vec![A_DRV.to_string(), B_DRV.to_string(), c_drv.to_string()]
        );
        Ok(())
    }

    #[test]
    fn entry_rejects_compressed_and_corrupted_payloads() -> anyhow::Result<()> {
        let (_tmp, layout, a_nar, _b_nar) = two_path_layout();
        let archive = DrvArchive::open(&layout)?;
        let a_hash_part = StorePath::parse(A_DRV).unwrap().hash_part();

        // Truncated payload: the byte count no longer matches NarSize.
        let nar_file = layout.join("nar").join(format!("{a_hash_part}.nar"));
        std::fs::write(&nar_file, &a_nar[..a_nar.len() - 8]).unwrap();
        let err = entry_err(&archive, A_DRV);
        assert!(err.contains(A_DRV) && err.contains("NarSize"), "got: {err}");

        // Same length but different content: the NarHash no longer matches.
        let mut corrupted = a_nar.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xff;
        std::fs::write(&nar_file, &corrupted).unwrap();
        let err = entry_err(&archive, A_DRV);
        assert!(err.contains(A_DRV) && err.contains("NarHash"), "got: {err}");

        // A compression scheme other than `none` is refused outright.
        let narinfo_file = layout.join(format!("{a_hash_part}.narinfo"));
        let rewritten = std::fs::read_to_string(&narinfo_file)
            .unwrap()
            .replace("Compression: none", "Compression: zstd");
        std::fs::write(&narinfo_file, rewritten).unwrap();
        let err = entry_err(&archive, A_DRV);
        assert!(
            err.contains("Compression") && err.contains("zstd"),
            "got: {err}"
        );
        Ok(())
    }

    #[test]
    fn open_requires_the_layout_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let err = format!(
            "{:#}",
            DrvArchive::open(tmp.path().join("nope")).unwrap_err()
        );
        assert!(err.contains("nope"), "got: {err}");

        // A minimal layout without nix-cache-info opens fine (warning only),
        // and entry() on a path with no narinfo names the path.
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        let archive = DrvArchive::open(&bare).unwrap();
        let err = entry_err(&archive, A_DRV);
        assert!(err.contains(A_DRV), "got: {err}");
    }
}
