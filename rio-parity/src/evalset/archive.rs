//! drvs.tar.zst — the eval set's derivation-closure archive.
//!
//! `nix copy --derivation` exports the .drv closure of every manifest
//! target into an uncompressed `file://` binary-cache layout
//! (`nix-cache-info`, one `.narinfo` per path, `nar/` payloads); that
//! layout is then tarred and zstd-compressed into a single
//! `drvs.tar.zst`, which keeps the eval set usable independent of any
//! Nix store's garbage collection. The campaign runner later untars
//! the archive once and imports derivations per batch from the local
//! layout instead of re-evaluating nixpkgs. Derivations are tiny text
//! files, so the export costs well under a millisecond and roughly
//! half a kilobyte of archive per closure path.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use anyhow::Context as _;

/// Export the drv closures of `drv_paths` into an uncompressed
/// `file://` binary-cache layout at `layout_dir`. Requires a `nix`
/// binary and the drvs (plus their closures) already in the local
/// store — i.e. run after the evaluation phase — so the offline unit
/// suite never calls it; it is exercised end-to-end when an eval set
/// is actually built.
///
/// `layout_dir` is created if missing and canonicalized to an absolute
/// path before being embedded in the `file://` destination URL (which
/// has no working directory to resolve a relative path against), so
/// relative paths are safe to pass.
///
/// TODO: the drv paths are passed as argv, which caps the practical
/// list size at the kernel argument-size limit (roughly 30k paths);
/// large enough scopes need batched invocations or `--stdin` once a
/// full-evaluation scope is built.
pub async fn export_drv_closure(
    nix_bin: &str,
    drv_paths: &[String],
    layout_dir: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(!drv_paths.is_empty(), "no drv paths to export");
    std::fs::create_dir_all(layout_dir)
        .with_context(|| format!("create {}", layout_dir.display()))?;
    let layout_dir = layout_dir
        .canonicalize()
        .with_context(|| format!("canonicalize {}", layout_dir.display()))?;
    // compression=none keeps the per-path nar payloads uncompressed so
    // the whole layout compresses as one zstd stream in
    // `pack_layout_to_tar_zst` instead of double-compressing each nar.
    let dest = format!("file://{}?compression=none", layout_dir.display());
    tracing::info!(n = drv_paths.len(), dest = %dest, "exporting drv closure");
    let out = tokio::process::Command::new(nix_bin)
        .args([
            "--extra-experimental-features",
            "nix-command",
            "copy",
            "--derivation",
            "--to",
            &dest,
        ])
        .args(drv_paths)
        .stdout(Stdio::null())
        // Cancelling the caller (e.g. a failure elsewhere in the build)
        // drops this future; kill_on_drop keeps that from orphaning a
        // still-running nix copy.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn {nix_bin} copy --derivation"))?;
    anyhow::ensure!(
        out.status.success(),
        "nix copy --derivation to {dest} failed ({}): {}",
        out.status,
        crate::body_snippet(std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")),
    );
    Ok(())
}

/// Tar the layout directory (member paths relative to it, `./…`) and
/// zstd-compress in-process (level 3). Returns the compressed byte
/// count. The archive is written to a `.tmp` sibling of `out_file` and
/// renamed into place on success, so an existing file at `out_file` is
/// only ever replaced by a complete archive — a tar or encode failure
/// cannot leave a truncated-but-valid-looking `drvs.tar.zst` behind.
/// Blocking; call via `spawn_blocking` from async contexts.
pub fn pack_layout_to_tar_zst(layout_dir: &Path, out_file: &Path) -> anyhow::Result<u64> {
    anyhow::ensure!(
        layout_dir.is_dir(),
        "drv layout dir {} does not exist",
        layout_dir.display()
    );
    let tmp_file = {
        let mut name = out_file.as_os_str().to_os_string();
        name.push(".tmp");
        std::path::PathBuf::from(name)
    };
    // `tar -C <layout> .` keeps member names relative (`./nix-cache-info`,
    // `./nar/…`) so the consumer can untar into any directory.
    let mut tar = std::process::Command::new("tar")
        .args(["-cf", "-", "-C"])
        .arg(layout_dir)
        .arg(".")
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn tar")?;
    let tar_stdout = tar.stdout.take().context("tar stdout missing")?;
    if let Err(err) = zstd_encode_to_file(tar_stdout, &tmp_file) {
        // The encoder failed mid-stream: kill and reap tar so it is not
        // left behind as a zombie writing into a closed pipe, and drop
        // the partial .tmp file.
        let _ = tar.kill();
        let _ = tar.wait();
        let _ = std::fs::remove_file(&tmp_file);
        return Err(err);
    }
    let status = tar.wait().context("wait for tar")?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp_file);
        anyhow::bail!(
            "tar exited with {status} while packing {}",
            layout_dir.display()
        );
    }
    std::fs::rename(&tmp_file, out_file)
        .with_context(|| format!("rename {} -> {}", tmp_file.display(), out_file.display()))?;
    let bytes = std::fs::metadata(out_file)
        .with_context(|| format!("stat {}", out_file.display()))?
        .len();
    Ok(bytes)
}

/// Drain `reader` through a level-3 zstd encoder into a freshly created
/// file at `path`. Split out of [`pack_layout_to_tar_zst`] so its error
/// path can kill and reap the tar child before returning.
fn zstd_encode_to_file(reader: impl std::io::Read, path: &Path) -> anyhow::Result<()> {
    let out = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(out);
    zstd::stream::copy_encode(reader, &mut writer, 3).context("zstd-encode tar stream")?;
    writer.flush().context("flush archive")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_a_layout_into_tar_zst() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = tmp.path().join("layout");
        std::fs::create_dir_all(layout.join("nar")).unwrap();
        std::fs::write(layout.join("nix-cache-info"), "StoreDir: /nix/store\n").unwrap();
        std::fs::write(layout.join("abc.narinfo"), "StorePath: /nix/store/abc-x\n").unwrap();
        std::fs::write(layout.join("nar/abc.nar"), vec![0u8; 4096]).unwrap();

        let out = tmp.path().join("drvs.tar.zst");
        let bytes = pack_layout_to_tar_zst(&layout, &out).unwrap();
        assert!(bytes > 0);
        // The intermediate .tmp sibling is renamed away on success.
        assert!(!tmp.path().join("drvs.tar.zst.tmp").exists());

        // zstd magic 0x28 B5 2F FD.
        let head = std::fs::read(&out).unwrap();
        assert_eq!(&head[..4], &[0x28, 0xB5, 0x2F, 0xFD], "not a zstd stream");

        // Decompress in-process, list with `tar -tf`, assert the layout
        // files round-tripped.
        let tar_path = tmp.path().join("drvs.tar");
        let mut src = std::fs::File::open(&out).unwrap();
        let mut dst = std::fs::File::create(&tar_path).unwrap();
        zstd::stream::copy_decode(&mut src, &mut dst).unwrap();
        let listing = std::process::Command::new("tar")
            .args(["-tf", tar_path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(listing.status.success());
        let names = String::from_utf8(listing.stdout).unwrap();
        for expected in ["./nix-cache-info", "./abc.narinfo", "./nar/abc.nar"] {
            assert!(names.contains(expected), "missing {expected} in:\n{names}");
        }
    }

    #[test]
    fn pack_fails_on_a_missing_layout_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = pack_layout_to_tar_zst(&tmp.path().join("nope"), &tmp.path().join("o.tar.zst"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("nope"));
    }

    #[test]
    fn pack_failure_leaves_no_partial_archive() {
        // Point the output at a non-existent directory so the encode
        // side fails after tar has been spawned: the error path must
        // reap the tar child and leave neither the target nor its .tmp
        // sibling behind.
        let tmp = tempfile::tempdir().unwrap();
        let layout = tmp.path().join("layout");
        std::fs::create_dir_all(&layout).unwrap();
        std::fs::write(layout.join("nix-cache-info"), "StoreDir: /nix/store\n").unwrap();

        let out = tmp.path().join("missing-dir").join("drvs.tar.zst");
        let err = pack_layout_to_tar_zst(&layout, &out).unwrap_err();
        assert!(
            format!("{err:#}").contains("drvs.tar.zst.tmp"),
            "got: {err:#}"
        );
        assert!(!out.exists());
        assert!(!out.parent().unwrap().exists());
    }
}
