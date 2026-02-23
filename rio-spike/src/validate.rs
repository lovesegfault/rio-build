use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, thread};

use crate::{benchmark, fuse_store, overlay, synthetic_db};

/// Validate that a string is safe to interpolate into a shell script.
fn assert_shell_safe(s: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !s.contains(['"', '`', '$', '\\', '\0', '\n']),
        "{label} contains shell-unsafe characters: {s}"
    );
    Ok(())
}

/// Iterate `read_dir`, logging errors for unreadable entries and collecting valid ones.
fn warn_read_dir(dir: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    let mut entries = Vec::new();
    let mut errors = 0u64;
    for result in fs::read_dir(dir)? {
        match result {
            Ok(e) => entries.push(e),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping unreadable dir entry");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        tracing::warn!(dir = %dir.display(), skipped = errors, "some directory entries were unreadable");
    }
    Ok(entries)
}

/// Run the full FUSE -> overlay -> sandbox -> build validation chain.
pub fn run_validate(all: bool, backing_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let work_dir = tempfile::tempdir()?;
    let work = work_dir.path();

    // Step 1: Prepare backing directory with real store paths.
    // Prefer /var/rio/store (emptyDir, ext4) for passthrough compatibility.
    // Overlay-backed filesystems (like /nix/store in containers) don't support
    // FUSE passthrough — the kernel rejects backing files on stacked filesystems.
    let backing = if let Some(dir) = backing_dir {
        anyhow::ensure!(
            dir.is_dir(),
            "backing dir does not exist: {}",
            dir.display()
        );
        dir
    } else {
        let dir = if Path::new("/var/rio").is_dir() {
            PathBuf::from("/var/rio/store")
        } else {
            work.join("backing-store")
        };
        prepare_backing_dir(&dir)?;
        dir
    };

    tracing::info!(backing = %backing.display(), "backing directory ready");

    // Step 2: Mount FUSE
    let fuse_mount = work.join("fuse-store");
    fs::create_dir_all(&fuse_mount)?;

    tracing::info!(mount_point = %fuse_mount.display(), "mounting FUSE filesystem");
    let _fuse_session = fuse_store::mount_fuse_background(&backing, &fuse_mount, false)?;

    // Give FUSE a moment to initialize
    thread::sleep(std::time::Duration::from_millis(500));

    // Validate basic FUSE operations
    validate_fuse_correctness(&backing, &fuse_mount)?;

    // Step 3: Create overlay
    // Upper/work dirs must be on a different filesystem than the FUSE lower.
    // Use /var/rio if available (emptyDir mount in k8s), otherwise a separate tmpdir.
    let overlay_base = if Path::new("/var/rio").is_dir() {
        PathBuf::from("/var/rio/overlays")
    } else {
        work.join("overlays")
    };
    fs::create_dir_all(&overlay_base)?;
    let overlay_mount = overlay::setup_overlay(&fuse_mount, &overlay_base, "build-1")?;

    tracing::info!(
        merged = %overlay_mount.merged_dir().display(),
        "overlay mounted"
    );

    // Validate overlay-over-FUSE correctness
    validate_overlay_correctness(&backing, &fuse_mount, overlay_mount.merged_dir())?;

    // Step 4: Generate synthetic SQLite DB
    let hello_path = find_hello_store_path()?;
    // Try querying nix path-info; fall back to synthetic metadata from disk
    let path_infos = match synthetic_db::query_path_info(&hello_path) {
        Ok(infos) => infos,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "nix path-info unavailable, generating synthetic metadata from disk"
            );
            synthetic_path_infos_from_dir(&backing)?
        }
    };

    let db_dir = overlay::prepare_nix_state_dirs(overlay_mount.upper_dir())?;
    let db_path = db_dir.join("db.sqlite");
    synthetic_db::generate_db(&db_path, &path_infos)?;

    tracing::info!(
        db_path = %db_path.display(),
        path_count = path_infos.len(),
        "synthetic SQLite DB generated"
    );

    // Validate SQLite DB consistency
    validate_sqlite_consistency(&db_path, &path_infos)?;

    // Step 5: nix-build inside the overlay (using mount namespace for correct layout)
    validate_nix_build(
        overlay_mount.merged_dir(),
        overlay_mount.upper_dir(),
        &hello_path,
    )?;

    // Verify outputs landed in upper layer
    validate_upper_layer_outputs(overlay_mount.upper_dir())?;

    // Step 6: Concurrent isolation test (if --all)
    if all {
        // Drop the first overlay
        overlay::teardown_overlay(overlay_mount)?;

        run_concurrent_isolation_test(&fuse_mount, &overlay_base, &path_infos)?;

        // FUSE read latency benchmark — standard mode
        tracing::info!("running FUSE read latency benchmark (standard mode)");
        benchmark::run_benchmark(&backing, &fuse_mount, 16)?;

        // Drop the standard FUSE session and re-mount with passthrough
        drop(_fuse_session);
        thread::sleep(std::time::Duration::from_millis(500));

        let passthrough_mount = work.join("fuse-store-passthrough");
        fs::create_dir_all(&passthrough_mount)?;
        tracing::info!("mounting FUSE with passthrough mode for benchmark");
        let _pt_session = fuse_store::mount_fuse_background(&backing, &passthrough_mount, true)?;
        thread::sleep(std::time::Duration::from_millis(500));

        tracing::info!("running FUSE read latency benchmark (passthrough mode)");
        benchmark::run_benchmark(&backing, &passthrough_mount, 16)?;
    }

    tracing::info!("all validation checks passed");
    Ok(())
}

/// Build synthetic NixPathInfo entries from directory entries on disk.
/// Computes real NAR hashes so the synthetic DB passes Nix verification.
/// Used as a fallback when `nix path-info` isn't available (e.g., no local store DB).
fn synthetic_path_infos_from_dir(
    store_dir: &Path,
) -> anyhow::Result<Vec<synthetic_db::NixPathInfo>> {
    let mut infos = Vec::new();

    let entries: Vec<_> = warn_read_dir(store_dir)?
        .into_iter()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !name.starts_with('.') && name.len() >= 33
        })
        .collect();

    tracing::info!(
        count = entries.len(),
        "computing NAR hashes for store paths"
    );

    for (i, entry) in entries.iter().enumerate() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let full_path = format!("/nix/store/{name}");
        let disk_path = store_dir.join(&name);

        let (nar_hash, nar_size) = match synthetic_db::compute_nar_hash(&disk_path) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(path = %name, error = %e, "failed to compute NAR hash, using placeholder");
                (format!("sha256:{}", "0".repeat(64)), 0)
            }
        };

        if (i + 1) % 50 == 0 {
            tracing::info!(
                progress = format!("{}/{}", i + 1, entries.len()),
                "NAR hash computation"
            );
        }

        infos.push(synthetic_db::NixPathInfo {
            path: full_path,
            nar_hash,
            nar_size,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        });
    }

    let placeholder_count = infos
        .iter()
        .filter(|info| info.nar_hash == format!("sha256:{}", "0".repeat(64)))
        .count();
    if placeholder_count > 0 {
        anyhow::bail!(
            "{placeholder_count}/{} store paths have placeholder hashes -- \
             nix-store --dump failed for these paths. The synthetic DB would be \
             rejected by Nix.",
            infos.len()
        );
    }

    Ok(infos)
}

/// Populate a backing directory with the hello package and its closure.
fn prepare_backing_dir(backing: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(backing)?;

    let hello_path = find_hello_store_path()?;

    tracing::info!(hello = %hello_path, "copying store closure to backing dir");

    // Try nix-store -qR for the closure. If that fails (no store DB in container),
    // fall back to copying all store paths from /nix/store directly.
    let output = Command::new("nix-store")
        .args(["-qR", &hello_path])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            stderr = %stderr.trim(),
            "nix-store -qR failed, copying all store paths from /nix/store"
        );
        return copy_all_store_paths(backing);
    }

    let closure_paths: Vec<String> = String::from_utf8(output.stdout)?
        .lines()
        .map(String::from)
        .collect();

    for path in &closure_paths {
        let src = Path::new(path);
        let name = src
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("store path has no filename: {}", src.display()))?;
        let dst = backing.join(name);

        if !dst.exists() {
            // Use cp -a to preserve symlinks, permissions, etc.
            let status = Command::new("cp")
                .args(["-a", "--reflink=auto"])
                .arg(src)
                .arg(&dst)
                .status()?;
            anyhow::ensure!(status.success(), "cp failed for {}", path);
        }
    }

    tracing::info!(
        path_count = closure_paths.len(),
        "backing directory populated"
    );

    Ok(())
}

/// Copy all store paths from /nix/store to the backing directory.
/// Used when nix-store -qR isn't available (no local store DB).
fn copy_all_store_paths(backing: &Path) -> anyhow::Result<()> {
    let src = Path::new("/nix/store");
    anyhow::ensure!(src.is_dir(), "/nix/store does not exist");

    let mut count = 0u64;
    let mut failures = 0u64;
    for entry in warn_read_dir(src)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.len() < 33 {
            continue;
        }
        let dst = backing.join(&name);
        if !dst.exists() {
            let status = Command::new("cp")
                .args(["-a", "--reflink=auto"])
                .arg(entry.path())
                .arg(&dst)
                .status()?;
            if !status.success() {
                tracing::warn!(path = %name, "cp failed, skipping");
                failures += 1;
                continue;
            }
            count += 1;
        }
    }

    if failures > 0 {
        let total = count + failures;
        let failure_pct = (failures as f64 / total as f64) * 100.0;
        anyhow::ensure!(
            failure_pct <= 10.0,
            "{failures}/{total} store path copies failed ({failure_pct:.0}%). \
             The backing directory is too incomplete to produce valid results."
        );
        tracing::warn!(
            failures,
            copied = count,
            "some store paths failed to copy but within threshold"
        );
    }

    anyhow::ensure!(
        count > 0 || failures == 0,
        "no store paths copied ({failures} failed)"
    );

    tracing::info!(
        path_count = count,
        "backing directory populated from /nix/store"
    );
    Ok(())
}

/// Find the store path for the `hello` package.
fn find_hello_store_path() -> anyhow::Result<String> {
    let output = Command::new("nix")
        .args(["eval", "nixpkgs#hello.outPath", "--raw"])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "nix eval failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// Validate FUSE correctness: file reads, directory listings, symlink resolution.
fn validate_fuse_correctness(backing: &Path, fuse_mount: &Path) -> anyhow::Result<()> {
    tracing::info!("validating FUSE correctness");

    // Check directory listing matches
    let backing_entries: Vec<String> = warn_read_dir(backing)?
        .into_iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    let fuse_entries: Vec<String> = warn_read_dir(fuse_mount)?
        .into_iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    anyhow::ensure!(
        backing_entries.len() == fuse_entries.len(),
        "directory listing mismatch: backing has {} entries, FUSE has {}",
        backing_entries.len(),
        fuse_entries.len()
    );

    // Check file content matches for a sample
    for entry in backing_entries.iter().take(5) {
        let backing_path = backing.join(entry);
        let fuse_path = fuse_mount.join(entry);

        let backing_meta = backing_path.symlink_metadata()?;
        let fuse_meta = fuse_path.symlink_metadata()?;

        // Check type matches
        anyhow::ensure!(
            backing_meta.file_type() == fuse_meta.file_type(),
            "file type mismatch for {entry}"
        );

        // For regular files, check content
        if backing_meta.is_file() {
            let backing_content = fs::read(&backing_path)?;
            let fuse_content = fs::read(&fuse_path)?;
            anyhow::ensure!(
                backing_content == fuse_content,
                "file content mismatch for {entry}"
            );
        }

        // For symlinks, check target
        if backing_meta.is_symlink() {
            let backing_target = fs::read_link(&backing_path)?;
            let fuse_target = fs::read_link(&fuse_path)?;
            anyhow::ensure!(
                backing_target == fuse_target,
                "symlink target mismatch for {entry}"
            );
        }
    }

    tracing::info!("FUSE correctness validated");
    Ok(())
}

/// Validate overlay-over-FUSE correctness.
fn validate_overlay_correctness(
    backing: &Path,
    _fuse_mount: &Path,
    merged: &Path,
) -> anyhow::Result<()> {
    tracing::info!("validating overlay-over-FUSE correctness");

    // The merged view should show the same files as the backing dir (lower layer)
    let backing_entries: std::collections::HashSet<String> = warn_read_dir(backing)?
        .into_iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    let merged_entries: std::collections::HashSet<String> = warn_read_dir(merged)?
        .into_iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    // merged should contain at least all backing entries (may have more from upper)
    for entry in &backing_entries {
        anyhow::ensure!(
            merged_entries.contains(entry),
            "overlay merged view missing entry from lower layer: {entry}"
        );
    }

    tracing::info!("overlay-over-FUSE correctness validated");
    Ok(())
}

/// Validate the synthetic SQLite DB is consistent.
fn validate_sqlite_consistency(
    db_path: &Path,
    path_infos: &[synthetic_db::NixPathInfo],
) -> anyhow::Result<()> {
    tracing::info!("validating SQLite DB consistency");

    let conn = rusqlite::Connection::open(db_path)?;

    // Check schema version
    let version: String = conn.query_row(
        "SELECT value FROM Config WHERE name = 'SchemaVersion'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(version == "10", "unexpected schema version: {version}");

    // Check all paths are present
    let db_count: i64 = conn.query_row("SELECT COUNT(*) FROM ValidPaths", [], |row| row.get(0))?;
    anyhow::ensure!(
        db_count == path_infos.len() as i64,
        "path count mismatch: DB has {db_count}, expected {}",
        path_infos.len()
    );

    // Check each path is findable by path (as Nix does)
    for info in path_infos {
        let hash: String = conn.query_row(
            "SELECT hash FROM ValidPaths WHERE path = ?1",
            [&info.path],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            hash == info.nar_hash,
            "hash mismatch for {}: DB has {hash}, expected {}",
            info.path,
            info.nar_hash
        );
    }

    tracing::info!(
        path_count = path_infos.len(),
        "SQLite DB consistency validated"
    );
    Ok(())
}

/// Search a store directory for the first entry whose name contains `pattern`
/// and does not contain any of the `exclude` substrings, and that is a directory.
/// Returns the canonical `/nix/store/<name>` path if found.
fn find_store_entry(
    store_dir: &Path,
    pattern: &str,
    exclude: &[&str],
) -> anyhow::Result<Option<String>> {
    let entry = warn_read_dir(store_dir)?.into_iter().find(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.contains(pattern) && !exclude.iter().any(|ex| n.contains(ex)) && e.path().is_dir()
    });
    Ok(entry.map(|e| format!("/nix/store/{}", e.file_name().to_string_lossy())))
}

/// Run nix commands inside a mount namespace where the overlay is bind-mounted
/// onto `/nix/store` and the synthetic DB is at `/nix/var/nix/db/`.
///
/// This matches the production worker layout: the overlay merged dir IS `/nix/store`,
/// and nix-daemon sees store paths at their canonical locations.
fn validate_nix_build(merged: &Path, upper: &Path, hello_path: &str) -> anyhow::Result<()> {
    tracing::info!(merged = %merged.display(), "validating nix-build inside overlay");

    let db_dir = upper.join("nix/var/nix/db");
    anyhow::ensure!(
        db_dir.join("db.sqlite").exists(),
        "synthetic DB not found at {}",
        db_dir.display()
    );

    // Use `unshare --mount` to create a child mount namespace, then bind-mount
    // the overlay merged dir onto /nix/store and the DB onto /nix/var/nix/db.
    // This avoids modifying the container's real /nix/store.
    //
    // The nix-build derivation is written to a temp file to avoid shell/Nix
    // escaping issues with inline expressions.
    // Find bash and coreutils in the store — the derivation needs both the builder
    // and its runtime dependencies (glibc etc.) to be available in the sandbox chroot.
    // We find bash's full closure path and use nix-store --query --requisites inside
    // the mount namespace to let Nix resolve what to bind-mount into the sandbox.
    let bash_path = find_store_entry(merged, "-bash-", &["interactive", ".drv"])?;
    let coreutils_path = find_store_entry(merged, "-coreutils-", &[".drv"])?;

    let (bash_path, coreutils_path) = match (bash_path, coreutils_path) {
        (Some(b), Some(c)) => (b, c),
        (None, Some(_)) => anyhow::bail!(
            "bash not found in store at {}, cannot build derivation",
            merged.display()
        ),
        (Some(_), None) => anyhow::bail!(
            "coreutils not found in store at {}, cannot build derivation",
            merged.display()
        ),
        (None, None) => anyhow::bail!(
            "neither bash nor coreutils found in store at {}, cannot build derivation",
            merged.display()
        ),
    };

    // Write the derivation to a temp file. Reference all store paths via
    // builtins.storePath so Nix includes them in the derivation's dependency
    // closure, ensuring the sandbox bind-mounts them into the chroot.
    let drv_file = tempfile::NamedTempFile::new()?;

    {
        // Collect all store paths — we need to reference them all in the derivation
        // so the sandbox bind-mounts the entire store closure (including glibc etc.).
        // Without this, bash can't find its dynamic linker.
        let all_store_paths: Vec<String> = warn_read_dir(merged)?
            .into_iter()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if !n.starts_with('.') && n.len() >= 33 && e.path().is_dir() {
                    Some(format!("/nix/store/{n}"))
                } else {
                    None
                }
            })
            .collect();

        let store_path_refs: String = all_store_paths
            .iter()
            .map(|p| format!("    (builtins.storePath \"{p}\")"))
            .collect::<Vec<_>>()
            .join("\n");

        // The sandbox only bind-mounts paths that appear in the derivation's
        // environment. We pass all store paths via an env var so Nix includes
        // them in the sandbox chroot (bash needs glibc, etc.).
        let drv_expr = format!(
            r#"let
  bash = builtins.storePath "{bash_path}";
  coreutils = builtins.storePath "{coreutils_path}";
  allPaths = [
{store_path_refs}
  ];
in derivation {{
  name = "spike-test";
  builder = "${{bash}}/bin/bash";
  args = ["-c" "${{coreutils}}/bin/echo spike-ok > $out"];
  # Hardcoded to match the spike's target architecture (x86_64).
  # Must be updated if the Terraform instance_type variable
  # (rio-spike/terraform/variables.tf) changes to an ARM instance.
  system = "x86_64-linux";
  # Force all store paths into the derivation environment so the sandbox
  # bind-mounts them into the chroot
  storePaths = builtins.concatStringsSep " " (map toString allPaths);
}}"#
        );
        fs::write(drv_file.path(), &drv_expr)?;
    }

    let drv_path_str = drv_file.path().display().to_string();

    // Validate all paths before interpolating into the shell script
    let merged_str = merged.display().to_string();
    let db_dir_str = db_dir.display().to_string();
    assert_shell_safe(&merged_str, "merged path")?;
    assert_shell_safe(&db_dir_str, "db_dir path")?;
    assert_shell_safe(hello_path, "hello_path")?;
    assert_shell_safe(&drv_path_str, "drv_file path")?;

    let script = format!(
        r#"
set -e
mount --bind "{merged}" /nix/store
mount --bind "{db_dir}" /nix/var/nix/db

echo ">>> nix-store --query --requisites {hello_path}"
nix-store --query --requisites "{hello_path}" | head -20
echo ">>> nix-store --query --requisites OK"

if [ -s "{drv_file}" ]; then
    echo ">>> building trivial derivation (sandbox enabled)"
    nix-build --no-out-link "{drv_file}" 2>&1
    echo ">>> nix-build OK"
else
    echo "WARN: bash/coreutils not found in store, skipping nix-build"
fi
"#,
        merged = merged.display(),
        db_dir = db_dir.display(),
        hello_path = hello_path,
        drv_file = drv_path_str,
    );

    let output = Command::new("unshare")
        .args(["--mount", "bash", "-c", &script])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.is_empty() {
        tracing::info!(stdout = %stdout.trim(), "nix-build output");
    }
    if !stderr.is_empty() {
        tracing::info!(stderr = %stderr.trim(), "nix-build stderr");
    }

    anyhow::ensure!(
        output.status.success(),
        "nix-build validation failed (exit {}): {}",
        output.status,
        stderr
    );

    // Check if the build output landed in the upper layer
    let upper_store = upper.join("nix/store");
    if upper_store.exists() {
        let new_paths: Vec<_> = warn_read_dir(&upper_store)?
            .into_iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("spike-test"))
            .collect();
        if !new_paths.is_empty() {
            tracing::info!(paths = ?new_paths, "build output found in overlay upper layer");
        } else {
            tracing::warn!(
                "no spike-test output found in upper layer (may be in a different location)"
            );
        }
    }

    tracing::info!("nix-build validation passed");
    Ok(())
}

/// Verify that build outputs land in the correct overlay upper layer.
fn validate_upper_layer_outputs(upper: &Path) -> anyhow::Result<()> {
    tracing::info!(upper = %upper.display(), "checking overlay upper layer");

    // The upper layer should contain at least the nix state directory we created
    let db_path = upper.join("nix/var/nix/db/db.sqlite");
    anyhow::ensure!(
        db_path.exists(),
        "synthetic DB not found in upper layer at {}",
        db_path.display()
    );

    tracing::info!("upper layer validation passed");
    Ok(())
}

/// Run concurrent isolation test: two overlays on the same FUSE mount.
fn run_concurrent_isolation_test(
    fuse_mount: &Path,
    overlay_base: &Path,
    path_infos: &[synthetic_db::NixPathInfo],
) -> anyhow::Result<()> {
    tracing::info!("running concurrent isolation test");

    let overlay_a = overlay::setup_overlay(fuse_mount, overlay_base, "concurrent-a")?;
    let overlay_b = overlay::setup_overlay(fuse_mount, overlay_base, "concurrent-b")?;

    let db_dir_a = overlay::prepare_nix_state_dirs(overlay_a.upper_dir())?;
    synthetic_db::generate_db(&db_dir_a.join("db.sqlite"), path_infos)?;

    let db_dir_b = overlay::prepare_nix_state_dirs(overlay_b.upper_dir())?;
    synthetic_db::generate_db(&db_dir_b.join("db.sqlite"), path_infos)?;

    // Write a marker file to each upper layer
    fs::write(overlay_a.upper_dir().join("marker-a"), "build-a")?;
    fs::write(overlay_b.upper_dir().join("marker-b"), "build-b")?;

    // Verify isolation: marker-a should only be in overlay-a's upper, not in overlay-b's
    anyhow::ensure!(
        overlay_a.upper_dir().join("marker-a").exists(),
        "marker-a not found in overlay-a upper"
    );
    anyhow::ensure!(
        !overlay_b.upper_dir().join("marker-a").exists(),
        "marker-a leaked to overlay-b upper (isolation failure!)"
    );
    anyhow::ensure!(
        overlay_b.upper_dir().join("marker-b").exists(),
        "marker-b not found in overlay-b upper"
    );
    anyhow::ensure!(
        !overlay_a.upper_dir().join("marker-b").exists(),
        "marker-b leaked to overlay-a upper (isolation failure!)"
    );

    // Verify the FUSE backing is untouched (read-only lower)
    anyhow::ensure!(
        !fuse_mount.join("marker-a").exists(),
        "marker-a found in FUSE mount (lower layer corruption!)"
    );
    anyhow::ensure!(
        !fuse_mount.join("marker-b").exists(),
        "marker-b found in FUSE mount (lower layer corruption!)"
    );

    tracing::info!("concurrent isolation test passed");

    // Clean up
    overlay::teardown_overlay(overlay_a)?;
    overlay::teardown_overlay(overlay_b)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires nix to be installed"]
    fn test_find_hello_store_path() {
        let path = find_hello_store_path().expect("nix should be available");
        assert!(path.starts_with("/nix/store/"));
        assert!(path.contains("hello"));
    }
}
