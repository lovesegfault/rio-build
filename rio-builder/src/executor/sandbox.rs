//! Sandbox setup: synthetic SQLite DB, nix.conf.
//!
//! Side-effects on the overlay's upper layer only — runs after overlay
//! mount + input resolution, before daemon spawn.
//!
//! Derivation files are deliberately NOT part of the per-build store:
//! neither materialized on disk nor registered in the synthetic DB (see
//! [`prepare_sandbox`]). The daemon gets the derivation over the wire
//! (`wopBuildDerivation` carries the full `BasicDerivation`) and, with
//! `isValidPath(drvPath)` false, derives the output map from it
//! in-memory instead of reading `.drv` files back from the store.

use std::path::Path;

use tracing::instrument;

use rio_proto::validated::ValidatedPathInfo;

use crate::overlay;
use crate::synth_db;

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

/// Populate the sandbox: synthetic SQLite DB, nix.conf.
///
/// Runs after overlay mount setup and input resolution. All side-effects
/// on the overlay's upper layer — no state returned. The overlay_mount
/// is held by the caller (execute_build) for later daemon spawn + upload
/// + teardown.
///
/// Steps:
/// 1. Generate the synthetic DB from `synth_paths` (ValidPaths + Refs)
///    so nix-daemon's isValidPath()/computeFSClosure() work for the
///    INPUT closure without a real store. `.drv` paths are filtered out
///    — see below.
/// 2. Write nix.conf (sandbox=true, substitute=false).
///
/// # Why `.drv` paths are excluded from the synthetic DB
///
/// The daemon already holds the derivation: `wopBuildDerivation` carries
/// the full `BasicDerivation` (inputDrvs resolved into inputSrcs by
/// `resolve_inputs`). With the drv path NOT valid in the per-build
/// store, nix-daemon's building goal takes its documented
/// in-memory-derivation fallback and derives the output map from that
/// wire-supplied derivation — no `.drv` file is read at any point.
///
/// Registering the drv path (the pre-castore behavior, harmless when the
/// whole-path FUSE could materialize ANY path on access) is actively
/// harmful now: once `isValidPath(drvPath)` is true, the daemon — with
/// `ca-derivations` enabled — reads the `.drv` back from disk
/// (`Store::queryPartialDerivationOutputMap` → `readInvalidDerivation`),
/// and `hashDerivationModulo` then recurses into the file's `inputDrvs`,
/// requiring the TRANSITIVE drv closure on disk. The castore lower
/// serves only `WorkAssignment.input_roots`, so a registered-but-absent
/// drv fails the build with `store path '<drv>' does not exist`
/// (the P0560 canary failure), and materializing the closure would
/// reintroduce exactly the per-path JIT machinery the cutover deleted.
/// Keeping every `.drv` out of the per-build store sidesteps all of it,
/// for leaves and parents alike.
// r[impl builder.synth-db.derivation-outputs+2]
#[instrument(skip_all, fields(drv_path = %drv_path))]
pub(super) async fn prepare_sandbox(
    overlay_mount: &overlay::OverlayMount,
    drv_path: &str,
    synth_paths: Vec<ValidatedPathInfo>,
    effective_cores: u32,
    systems: &[String],
) -> Result<(), ExecutorError> {
    // Generate synthetic DB from caller-supplied metadata (I-106:
    // captured during compute_input_closure's BFS, no second QPI pass).
    // The closure metadata legitimately CONTAINS .drv paths (the BFS
    // seeds with the top-level drv and its inputDrvs to reach their
    // references); they are dropped here, at the store-DB boundary, so
    // the per-build store never claims a derivation it cannot back with
    // a file.
    let n_total = synth_paths.len();
    let synth_paths: Vec<ValidatedPathInfo> = synth_paths
        .into_iter()
        .filter(|p| !p.store_path.basename().ends_with(".drv"))
        .collect();
    tracing::debug!(
        registered = synth_paths.len(),
        drv_paths_excluded = n_total - synth_paths.len(),
        "synth DB registration set (drv paths excluded)"
    );

    let db_dir = overlay_mount.upper_synth_db();
    overlay::mkdir_all(&db_dir)?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths).await?;

    // Set up nix.conf in overlay
    setup_nix_conf(&overlay_mount.upper_nix_conf(), effective_cores, systems)?;

    Ok(())
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

    use rio_test_support::fixtures::{make_nar, make_path_info, test_drv_path, test_store_path};

    /// `.drv` paths are NEVER registered in the per-build store: not in
    /// `ValidPaths`, not in `DerivationOutputs`, and no file is staged
    /// into the overlay upper. The shape mirrors a parent ("root")
    /// build whose closure metadata carries its own `.drv`, an input
    /// `.drv`, that input's output, and a plain source — only the two
    /// non-drv paths may land in the DB, so nix-daemon's
    /// `isValidPath(drvPath)` stays false and it derives the output map
    /// from the wire-supplied BasicDerivation (in-memory fallback)
    /// instead of reading `.drv` files (which the castore lower cannot
    /// provide).
    // r[verify builder.synth-db.derivation-outputs+2]
    #[tokio::test]
    async fn test_prepare_sandbox_never_registers_or_stages_drv_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mount = overlay::OverlayMount::for_test(dir.path().join("build"));
        std::fs::create_dir_all(mount.upper_store())?;

        let root_drv = test_drv_path("rio-root");
        let leaf_drv = test_drv_path("rio-leaf-1");
        let leaf_out = test_store_path("rio-leaf-1-out");
        let busybox = test_store_path("busybox-static");

        // Closure metadata as compute_input_closure would return it: the
        // BFS legitimately includes the .drv files (it seeds with them to
        // reach their references).
        let synth_paths: Vec<ValidatedPathInfo> = [&root_drv, &leaf_drv, &leaf_out, &busybox]
            .iter()
            .map(|p| {
                let (nar, hash) = make_nar(p.as_bytes());
                make_path_info(p, &nar, hash)
            })
            .collect();

        prepare_sandbox(&mount, &root_drv, synth_paths, 2, &[])
            .await
            .map_err(|e| anyhow::anyhow!("prepare_sandbox failed: {e}"))?;

        // Nothing staged into the upper store — the daemon must not find
        // any .drv file there.
        assert_eq!(
            std::fs::read_dir(mount.upper_store())?.count(),
            0,
            "no files (in particular no .drv) are staged into the overlay upper"
        );

        // Only the non-drv paths are valid; DerivationOutputs stays empty.
        let db_path = mount.upper_synth_db().join("db.sqlite");
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
        let mut conn = <sqlx::SqliteConnection as sqlx::Connection>::connect_with(&opts).await?;
        let valid: Vec<String> = sqlx::query_scalar("SELECT path FROM ValidPaths ORDER BY path")
            .fetch_all(&mut conn)
            .await?;
        assert!(
            valid.iter().all(|p| !p.ends_with(".drv")),
            "no .drv path may be registered as valid; got {valid:?}"
        );
        assert!(
            valid.contains(&leaf_out) && valid.contains(&busybox),
            "non-drv input paths are still registered; got {valid:?}"
        );
        let drv_outputs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM DerivationOutputs")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(
            drv_outputs, 0,
            "DerivationOutputs stays empty — the output map comes from the in-memory derivation"
        );
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
