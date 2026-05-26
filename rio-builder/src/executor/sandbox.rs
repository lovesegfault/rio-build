//! Sandbox setup: top-level `.drv` staging, synthetic SQLite DB, nix.conf.
//!
//! Side-effects on the overlay's upper layer only — runs after overlay
//! mount + input resolution, before daemon spawn.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use sha2::Digest as _;
use tracing::instrument;

use rio_nix::derivation::Derivation;
use rio_proto::validated::ValidatedPathInfo;

use crate::overlay;
use crate::synth_db::{self, SynthDrvOutput};

use super::ExecutorError;

/// Worker nix.conf content for sandbox builds.
///
/// The ConfigMap `rio-nix-conf` in the Helm chart's configmaps.yaml can
/// override this at `/etc/rio/nix-conf/nix.conf` — operators customize
/// without image rebuild. `setup_nix_conf` checks for the override
/// first; this is the fallback when the mount is absent (VM tests,
/// local dev).
///
/// `ca-derivations`: required for content-addressed outputs (Phase 2c
/// CA support). The ConfigMap also lists `nix-command` for pod
/// diagnostics (`nix store info` etc), but it's NOT needed for builds
/// — the daemon receives pre-evaluated .drv files via worker-protocol
/// opcodes, no `nix` CLI involvement. Dropped here to reduce attack
/// surface in the sandbox-spawned daemon.
///
/// This constant must stay in sync with infra/helm/rio-build/templates/
/// configmaps.yaml —
/// a mismatch means K8s deployments get different behavior than VM
/// tests (which use native NixOS modules, not this path).
// r[impl fetcher.nixconf.hashed-mirrors]
const WORKER_NIX_CONF: &str = "\
builders =
substitute = false
sandbox = true
sandbox-fallback = false
restrict-eval = true
experimental-features = ca-derivations
hashed-mirrors = http://tarballs.nixos.org/
";

/// Path where operators can mount a nix.conf override (via the
/// `rio-nix-conf` ConfigMap). If present, `setup_nix_conf` copies
/// THIS instead of using `WORKER_NIX_CONF`. Lets operators customize
/// experimental-features, sandbox paths, etc without image rebuild.
const NIX_CONF_OVERRIDE_PATH: &str = "/etc/rio/nix-conf/nix.conf";

/// Populate the sandbox: top-level `.drv` file, synthetic SQLite DB,
/// nix.conf.
///
/// Runs after overlay mount setup and input resolution. All side-effects
/// on the overlay's upper layer — no state returned. The overlay_mount
/// is held by the caller (execute_build) for later daemon spawn + upload
/// + teardown.
///
/// Steps:
/// 1. Stage the assignment's `.drv` into the per-build store (overlay
///    upper) so nix-daemon can read it back from disk — see
///    [`stage_drv`] for why `wopBuildDerivation` alone is not enough.
/// 2. Generate synthetic DB from `synth_paths` (ValidPaths +
///    DerivationOutputs) so nix-daemon's isValidPath()/queryPartial
///    DerivationOutputMap() work without a real store. The staged
///    `.drv` is registered too (its narHash/narSize are recomputed
///    locally from the bytes just written).
/// 3. Write nix.conf (sandbox=true, substitute=false).
#[instrument(skip_all, fields(drv_path = %drv_path))]
pub(super) async fn prepare_sandbox(
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    drv_text: &[u8],
    synth_paths: Vec<ValidatedPathInfo>,
    effective_cores: u32,
    systems: &[String],
) -> Result<(), ExecutorError> {
    // Stage the .drv FILE first, then make sure it is registered in the
    // synth DB. The store's QueryPathInfo normally already returned the
    // .drv (compute_input_closure seeds the BFS with drv_path), in which
    // case the store-reported metadata (with references) wins and the
    // locally-computed row is skipped; the push below covers the case
    // where the store had no record of the .drv — registration must not
    // depend on that, since the bytes being written are the ground truth
    // the daemon will read.
    let drv_info = stage_drv(&overlay_mount.upper_store(), drv_path, drv_text)?;
    let mut synth_paths = synth_paths;
    if !synth_paths
        .iter()
        .any(|p| p.store_path.as_str() == drv_path)
    {
        synth_paths.push(drv_info);
    }

    // Generate synthetic DB from caller-supplied metadata (I-106:
    // captured during compute_input_closure's BFS, no second QPI pass).
    // CRITICAL: populate DerivationOutputs so nix-daemon's
    // queryPartialDerivationOutputMap(drvPath) returns our output paths.
    // Without it, initialOutputs[out].known is None → nix-daemon builds at
    // makeFallbackPath() (hash of "rewrite:<drvPath>:name:out" + zero hash),
    // but the builder's $out (from BasicDerivation env) is the REAL path →
    // output path mismatch → "builder failed to produce output path".
    //
    // Filter floating-CA via static_outputs(): nix-daemon computes
    // scratchPath internally for CA outputs and doesn't need the
    // DerivationOutputs hint.
    use rio_nix::derivation::DerivationLike as _;
    let drv_outputs: Vec<SynthDrvOutput> = drv
        .static_outputs()
        .map(|o| SynthDrvOutput {
            drv_path: drv_path.to_string(),
            output_name: o.name().to_string(),
            output_path: o.path().to_string(),
        })
        .collect();
    let db_dir = overlay_mount.upper_synth_db();
    overlay::mkdir_all(&db_dir)?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths, &drv_outputs).await?;

    // Set up nix.conf in overlay
    setup_nix_conf(&overlay_mount.upper_nix_conf(), effective_cores, systems)?;

    Ok(())
}

/// Stage the build's own `.drv` file into the per-build store (overlay
/// upper) and return the `ValidatedPathInfo` to register it with.
///
/// P0560 cutover gap: the castore-FUSE lower serves exactly
/// `WorkAssignment.input_roots` (the input closure). Unlike the old
/// whole-path FUSE it does NOT materialize arbitrary store paths on
/// lookup, so nothing puts the `.drv` itself on disk. The daemon still
/// needs it even though `wopBuildDerivation` carries the full
/// `BasicDerivation` inline: the `.drv` is registered in the synth DB
/// (the input-closure BFS includes it), and once `isValidPath(drvPath)`
/// is true, Nix's `Store::queryPartialDerivationOutputMap` — called
/// from the building goal with `ca-derivations` enabled (it is, in
/// `WORKER_NIX_CONF` and the `rio-nix-conf` ConfigMap) — reads the
/// `.drv` back from disk via `readInvalidDerivation`. A missing file
/// fails the build with `store path '<drv>' does not exist`
/// (MiscFailure) before the builder ever runs.
///
/// Only the TOP-LEVEL `.drv` is needed. The `BasicDerivation` the
/// executor sends (`client_send_build_derivation`, opcode 36) has
/// `inputDrvs` already collapsed into concrete `inputSrcs` by
/// `resolve_inputs`, so every daemon-side path that walks input
/// derivations (`DerivationResolutionGoal`, `hashDerivationModulo`,
/// the input-closure walk in the building goal) iterates an EMPTY
/// `inputDrvs` map and never opens an input `.drv` — those stay
/// un-materialized, no drv-closure staging is required.
///
/// The file is written into the overlay UPPER (`{upper}/nix/store/`),
/// mode 0444, via temp-file + rename (same staging discipline as the
/// castore fill path). Writing the upper directly while the overlay is
/// mounted is safe here: nothing has looked this name up through the
/// merged view yet (the daemon spawns later in the flow), so no overlay
/// dentry can be stale. The upload scan excludes exactly this basename
/// (`upload_all_outputs`), so the staged file is never re-uploaded nor
/// reported as a phantom output.
///
/// Idempotent: an existing file with identical content is reused
/// (daemon-transient retry on a leaked upper dir, warm node); an
/// existing file with DIFFERENT content is an error — never silently
/// keep either copy.
///
/// narHash/narSize for the registration are recomputed locally from the
/// bytes being written (NAR encoding of a single non-executable regular
/// file via `rio_nix::nar::serialize`) — strictly stronger than trusting
/// a remote record, since this is exactly what the daemon will see on
/// disk. References are left empty for the locally-built row: nothing in
/// the `wopBuildDerivation` flow walks the `.drv`'s own references, and
/// when the store DID return metadata for the `.drv` the store row (with
/// references) is used instead (see `prepare_sandbox`).
fn stage_drv(
    upper_store: &Path,
    drv_path: &str,
    drv_text: &[u8],
) -> Result<ValidatedPathInfo, ExecutorError> {
    let store_path = rio_nix::store_path::StorePath::parse(drv_path).map_err(|e| {
        ExecutorError::InvalidDerivation(format!(
            "assignment drv_path {drv_path:?} is not a valid store path: {e}"
        ))
    })?;
    let basename = store_path.basename().to_owned();

    // NAR-encode the single regular file to get narHash/narSize for the
    // synth-db ValidPaths row — the same encoding the store computes for
    // a .drv at upload time, so the row matches what a real store would
    // record for these bytes.
    let mut nar = Vec::with_capacity(drv_text.len() + 128);
    rio_nix::nar::serialize(
        &mut nar,
        &rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: drv_text.to_vec(),
        },
    )
    .map_err(|e| ExecutorError::DrvStage(format!("NAR-encoding {drv_path} failed: {e}")))?;
    let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
    let nar_size = nar.len() as u64;

    let dest = upper_store.join(&basename);
    match std::fs::read(&dest) {
        Ok(existing) if existing == drv_text => {
            tracing::debug!(
                drv_path = %drv_path,
                bytes = drv_text.len(),
                "drv already staged in per-build store with identical content; reusing"
            );
        }
        Ok(existing) => {
            return Err(ExecutorError::DrvStage(format!(
                "{} already exists in the per-build store with DIFFERENT content \
                 ({} bytes on disk vs {} bytes in the assignment); refusing to overwrite — \
                 wipe the overlay upper for this build (stale state from a previous attempt?)",
                dest.display(),
                existing.len(),
                drv_text.len(),
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Atomic write+rename: a hidden temp name so a crash between
            // write and rename can never be mistaken for a store path
            // (scan_new_outputs skips dotfiles too).
            let tmp = upper_store.join(format!(".{basename}.stage-tmp"));
            let write = (|| -> std::io::Result<()> {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(drv_text)?;
                // 0444 like any real store file; set on the temp so the
                // rename publishes name and mode together.
                f.set_permissions(std::fs::Permissions::from_mode(0o444))?;
                drop(f);
                std::fs::rename(&tmp, &dest)
            })();
            if let Err(e) = write {
                let _ = std::fs::remove_file(&tmp);
                return Err(ExecutorError::DrvStage(format!(
                    "writing {} ({} bytes): {e}",
                    dest.display(),
                    drv_text.len(),
                )));
            }
            tracing::info!(
                drv_path = %drv_path,
                bytes = drv_text.len(),
                nar_size,
                "staged .drv into per-build store"
            );
        }
        Err(e) => {
            return Err(ExecutorError::DrvStage(format!(
                "checking for existing {}: {e}",
                dest.display(),
            )));
        }
    }

    Ok(ValidatedPathInfo {
        store_path,
        store_path_hash: vec![],
        deriver: None,
        nar_hash,
        nar_size,
        references: vec![],
        registration_time: 0,
        ultimate: false,
        signatures: vec![],
        content_address: None,
    })
}

/// Write nix.conf to the overlay upper layer.
///
/// Checks for an operator override at [`NIX_CONF_OVERRIDE_PATH`]
/// first (mounted from the `rio-nix-conf` ConfigMap in K8s). If
/// present, copies it; else uses [`WORKER_NIX_CONF`]. In BOTH cases,
/// `cores = <effective_cores>` and `max-jobs = 1` are appended last
/// (later lines win in nix.conf, so the operator override is
/// preserved for everything else but cannot un-clamp cores).
///
/// Override use case: operator wants to add e.g. `extra-sandbox-
/// paths = /some/secret` or tweak `sandbox-build-dir`. ConfigMap
/// edit + pod restart, no image rebuild.
///
/// I-197 defense-in-depth: `effective_cores` is ALSO sent via
/// `wopSetOptions.build_cores`, which is the primary path. Writing
/// it to nix.conf catches an upstream `wopSetOptions` regression
/// (the daemon would otherwise fall back to nix.conf → host nproc).
///
/// `systems` is the resolved `RIO_SYSTEMS` list. Non-`builtin`
/// entries become the daemon's `extra-platforms` so a drv routed for
/// any advertised system is accepted (e.g. `i686-linux` on an x86_64
/// host). The host system being in the list is a no-op.
// r[impl builder.platform.i686]
fn setup_nix_conf(
    upper_nix_conf: &Path,
    effective_cores: u32,
    systems: &[String],
) -> Result<(), ExecutorError> {
    std::fs::create_dir_all(upper_nix_conf).map_err(ExecutorError::NixConf)?;

    // Try the override first. `read` (not `read_to_string`) —
    // nix.conf is ASCII but we're just copying bytes, no reason
    // to UTF-8-validate. ENOENT OR empty = not mounted → fallback.
    // Any OTHER error (permission denied, I/O) → bubble up
    // (something's wrong with the mount).
    //
    // The mount is a DIRECTORY (no subPath): `optional: true`
    // ConfigMap + missing ConfigMap → K8s mounts an empty dir →
    // read("dir/nix.conf") → clean ENOENT → fallback. Directory
    // mount (no subPath): subPath creates an empty file/dir when
    // the ConfigMap is missing → empty nix.conf → Nix defaults →
    // substitute=true → cache.nixos.org lookup → airgap DNS
    // timeout (600s+ hang).
    let mut content = match std::fs::read(NIX_CONF_OVERRIDE_PATH) {
        Ok(bytes) if !bytes.is_empty() => {
            tracing::debug!(
                path = NIX_CONF_OVERRIDE_PATH,
                "using nix.conf override from ConfigMap mount"
            );
            bytes
        }
        // Empty OR NotFound: ConfigMap not applied, or key missing.
        // Either way, compiled-in fallback.
        Ok(_) => WORKER_NIX_CONF.as_bytes().to_vec(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => WORKER_NIX_CONF.as_bytes().to_vec(),
        Err(e) => return Err(ExecutorError::NixConf(e)),
    };

    // r[impl builder.cores.cgroup-clamp+2]
    // Append AFTER the base/override (later lines win). max-jobs=1:
    // single-slot builder (P0537) — the daemon should never start a
    // second build even if the wopBuildDerivation flow somehow
    // requests it. cores: same value sent via wopSetOptions; nix.conf
    // is the fallback if that opcode is dropped/ignored. .max(1):
    // never write `cores = 0` (daemon resolves 0 → nproc, the I-196
    // failure mode).
    if !content.ends_with(b"\n") {
        content.push(b'\n');
    }
    content.extend_from_slice(
        format!("max-jobs = 1\ncores = {}\n", effective_cores.max(1)).as_bytes(),
    );
    let extra: Vec<&str> = systems
        .iter()
        .map(String::as_str)
        .filter(|s| *s != "builtin")
        .collect();
    if !extra.is_empty() {
        content.extend_from_slice(format!("extra-platforms = {}\n", extra.join(" ")).as_bytes());
    }

    std::fs::write(upper_nix_conf.join("nix.conf"), content).map_err(ExecutorError::NixConf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rio_test_support::fixtures::{make_nar, test_drv_path, test_store_path};

    /// Minimal one-output ATerm for the staging tests. The output path
    /// must be a parseable store path (prepare_sandbox feeds it into
    /// the DerivationOutputs table).
    fn stage_test_drv() -> (String, Vec<u8>, Derivation) {
        let drv_path = test_drv_path("stage-me");
        let out = test_store_path("stage-me-out");
        let aterm = format!(
            r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","{out}")])"#
        );
        let drv = Derivation::parse(&aterm).expect("test ATerm is valid");
        (drv_path, aterm.into_bytes(), drv)
    }

    /// File lands at the exact store basename in the upper, mode 0444,
    /// byte-identical content; the returned registration row carries the
    /// NAR hash/size of exactly those bytes (cross-checked against the
    /// shared `make_nar` oracle, which is what the store itself would
    /// have recorded for this .drv).
    #[test]
    fn test_stage_drv_writes_file_and_registration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let upper_store = dir.path().join("upper/nix/store");
        std::fs::create_dir_all(&upper_store)?;
        let (drv_path, drv_text, _) = stage_test_drv();

        let info = stage_drv(&upper_store, &drv_path, &drv_text)
            .map_err(|e| anyhow::anyhow!("stage_drv failed: {e}"))?;

        let basename = rio_nix::store_path::basename(&drv_path).unwrap();
        let dest = upper_store.join(basename);
        assert_eq!(std::fs::read(&dest)?, drv_text, "content must be verbatim");
        let mode = std::fs::metadata(&dest)?.permissions().mode() & 0o7777;
        assert_eq!(mode, 0o444, "store files are read-only (got {mode:o})");

        // Registration row matches the NAR encoding of the staged bytes.
        let (nar, expected_hash) = make_nar(&drv_text);
        assert_eq!(info.store_path.as_str(), drv_path);
        assert_eq!(info.nar_hash, expected_hash);
        assert_eq!(info.nar_size, nar.len() as u64);

        // No temp file left behind.
        assert!(
            !upper_store.join(format!(".{basename}.stage-tmp")).exists(),
            "temp file must be renamed away"
        );
        Ok(())
    }

    /// Re-running with identical content (daemon-transient retry on a
    /// leaked upper) is fine; different content is a loud error and the
    /// original file is left untouched.
    #[test]
    fn test_stage_drv_idempotent_and_mismatch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let upper_store = dir.path().join("upper/nix/store");
        std::fs::create_dir_all(&upper_store)?;
        let (drv_path, drv_text, _) = stage_test_drv();

        let first = stage_drv(&upper_store, &drv_path, &drv_text)
            .map_err(|e| anyhow::anyhow!("first stage failed: {e}"))?;
        let second = stage_drv(&upper_store, &drv_path, &drv_text)
            .map_err(|e| anyhow::anyhow!("idempotent re-run must succeed: {e}"))?;
        assert_eq!(first.nar_hash, second.nar_hash);
        assert_eq!(first.nar_size, second.nar_size);

        // Same path, DIFFERENT bytes → typed DrvStage error naming the
        // file; the staged content is not clobbered.
        let tampered = b"Derive(<something else entirely>)".to_vec();
        let err =
            stage_drv(&upper_store, &drv_path, &tampered).expect_err("content mismatch must error");
        assert!(matches!(err, ExecutorError::DrvStage(_)), "got {err:?}");
        let msg = err.to_string();
        let basename = rio_nix::store_path::basename(&drv_path).unwrap();
        assert!(
            msg.contains(basename) && msg.contains("DIFFERENT content"),
            "error must name the path and the mismatch: {msg}"
        );
        assert_eq!(
            std::fs::read(upper_store.join(basename))?,
            drv_text,
            "original staged content must survive the rejected overwrite"
        );

        // Mismatch is infrastructure (node-local state), not permanent,
        // not daemon-transient — the scheduler may retry elsewhere.
        assert!(!err.is_permanent());
        assert!(!err.is_daemon_transient());
        Ok(())
    }

    /// A drv_path that is not a store path is a deterministic
    /// assignment defect → InvalidDerivation (permanent), not an infra
    /// error; a write failure (unwritable upper) is DrvStage (infra).
    #[test]
    fn test_stage_drv_error_classification() {
        let dir = tempfile::tempdir().unwrap();
        let upper_store = dir.path().join("upper/nix/store");
        std::fs::create_dir_all(&upper_store).unwrap();
        let (_, drv_text, _) = stage_test_drv();

        let err = stage_drv(&upper_store, "not-a-store-path.drv", &drv_text)
            .expect_err("invalid drv_path must error");
        assert!(
            matches!(err, ExecutorError::InvalidDerivation(_)),
            "got {err:?}"
        );
        assert!(err.is_permanent());

        // Writing into a missing upper dir → DrvStage with the target
        // path in the message (actionable: the overlay layout is wrong).
        let missing = dir.path().join("does-not-exist/nix/store");
        let (drv_path, drv_text, _) = stage_test_drv();
        let err =
            stage_drv(&missing, &drv_path, &drv_text).expect_err("unwritable upper must error");
        assert!(matches!(err, ExecutorError::DrvStage(_)), "got {err:?}");
        assert!(
            err.to_string().contains("does-not-exist"),
            "error must carry the destination path: {err}"
        );
    }

    /// prepare_sandbox end-to-end (sans real mounts): the .drv lands in
    /// the upper store AND is registered in the synth DB, so the
    /// DerivationOutputs FK resolves even when the store-side metadata
    /// did not include the .drv.
    #[tokio::test]
    async fn test_prepare_sandbox_registers_staged_drv() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let build_dir = dir.path().join("build");
        let mount = overlay::OverlayMount::for_test(build_dir.clone());
        std::fs::create_dir_all(mount.upper_store())?;
        let (drv_path, drv_text, drv) = stage_test_drv();

        // synth_paths deliberately does NOT include the .drv — the
        // staged registration must cover it.
        prepare_sandbox(&mount, &drv, &drv_path, &drv_text, vec![], 2, &[])
            .await
            .map_err(|e| anyhow::anyhow!("prepare_sandbox failed: {e}"))?;

        // File staged.
        let basename = rio_nix::store_path::basename(&drv_path).unwrap();
        assert_eq!(std::fs::read(mount.upper_store().join(basename))?, drv_text);

        // Registered in the synth DB with the locally-computed NAR hash,
        // and the DerivationOutputs row resolved against it.
        let (_, expected_hash) = make_nar(&drv_text);
        let db_path = mount.upper_synth_db().join("db.sqlite");
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
        let mut conn = <sqlx::SqliteConnection as sqlx::Connection>::connect_with(&opts).await?;
        let hash: String = sqlx::query_scalar("SELECT hash FROM ValidPaths WHERE path = ?1")
            .bind(&drv_path)
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(hash, format!("sha256:{}", hex::encode(expected_hash)));
        let n_outputs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM DerivationOutputs d \
             JOIN ValidPaths vp ON d.drv = vp.id WHERE vp.path = ?1",
        )
        .bind(&drv_path)
        .fetch_one(&mut conn)
        .await?;
        assert_eq!(
            n_outputs, 1,
            "DerivationOutputs FK must resolve against the staged .drv row"
        );
        Ok(())
    }

    /// When the store-side closure metadata ALREADY carries the .drv
    /// (the normal case — the BFS seeds with drv_path), the
    /// store-reported row wins and no duplicate is inserted.
    #[tokio::test]
    async fn test_prepare_sandbox_keeps_store_provided_drv_row() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mount = overlay::OverlayMount::for_test(dir.path().join("build"));
        std::fs::create_dir_all(mount.upper_store())?;
        let (drv_path, drv_text, drv) = stage_test_drv();

        // Store-provided metadata for the .drv with a sentinel hash and
        // a reference — distinguishable from the locally-computed row.
        let (nar, _) = make_nar(&drv_text);
        let mut store_row = rio_test_support::fixtures::make_path_info(&drv_path, &nar, [0xEE; 32]);
        store_row.references =
            vec![rio_nix::store_path::StorePath::parse(&test_store_path("some-src")).unwrap()];

        prepare_sandbox(&mount, &drv, &drv_path, &drv_text, vec![store_row], 2, &[])
            .await
            .map_err(|e| anyhow::anyhow!("prepare_sandbox failed: {e}"))?;

        let db_path = mount.upper_synth_db().join("db.sqlite");
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
        let mut conn = <sqlx::SqliteConnection as sqlx::Connection>::connect_with(&opts).await?;
        let rows: Vec<String> = sqlx::query_scalar("SELECT hash FROM ValidPaths WHERE path = ?1")
            .bind(&drv_path)
            .fetch_all(&mut conn)
            .await?;
        assert_eq!(rows.len(), 1, "exactly one ValidPaths row for the .drv");
        assert_eq!(
            rows[0],
            format!("sha256:{}", hex::encode([0xEE; 32])),
            "store-provided metadata wins when present"
        );
        // The file is still staged regardless of which row won.
        let basename = rio_nix::store_path::basename(&drv_path).unwrap();
        assert_eq!(std::fs::read(mount.upper_store().join(basename))?, drv_text);
        Ok(())
    }

    #[test]
    fn test_worker_nix_conf_content() {
        assert!(WORKER_NIX_CONF.contains("sandbox = true"));
        assert!(WORKER_NIX_CONF.contains("substitute = false"));
        assert!(WORKER_NIX_CONF.contains("builders ="));
        assert!(WORKER_NIX_CONF.contains("sandbox-fallback = false"));
        assert!(WORKER_NIX_CONF.contains("hashed-mirrors = http://tarballs.nixos.org/"));
    }

    // r[verify builder.cores.cgroup-clamp+2]
    // r[verify builder.platform.i686]
    #[test]
    fn test_setup_nix_conf() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conf_dir = dir.path().join("etc/nix");
        let sys = ["x86_64-linux".into(), "i686-linux".into(), "builtin".into()];
        setup_nix_conf(&conf_dir, 2, &sys)?;

        let conf_path = conf_dir.join("nix.conf");
        assert!(conf_path.exists());
        let content = std::fs::read_to_string(&conf_path)?;
        assert!(content.contains("sandbox = true"));
        // I-197 defense-in-depth: cores/max-jobs appended AFTER the
        // base content (later lines win in nix.conf).
        assert!(content.contains("max-jobs = 1\n"));
        assert!(content.contains("cores = 2\n"));
        let sandbox_pos = content.find("sandbox = true").unwrap();
        let cores_pos = content.find("cores = 2").unwrap();
        assert!(
            cores_pos > sandbox_pos,
            "cores= appended after base content so it wins over any \
             override; got:\n{content}"
        );
        // r[builder.platform.i686]: advertised systems → extra-platforms.
        assert!(
            content.contains("extra-platforms = x86_64-linux i686-linux\n"),
            "non-builtin systems become extra-platforms; got:\n{content}"
        );
        assert!(
            !content.contains("builtin"),
            "`builtin` is a routing pseudo-system, not a nix platform"
        );
        // Never `cores = 0` (daemon resolves 0 → nproc, the I-196 bug).
        setup_nix_conf(&conf_dir, 0, &[])?;
        let content = std::fs::read_to_string(&conf_path)?;
        assert!(content.contains("cores = 1\n"), "0 clamped to 1");
        assert!(!content.contains("cores = 0"));
        assert!(
            !content.contains("extra-platforms"),
            "empty systems → no extra-platforms line"
        );
        Ok(())
    }
}
