//! Sandbox setup: synthetic SQLite DB, nix.conf, FOD output whiteouts.
//!
//! Side-effects on the overlay's upper layer only — runs after overlay
//! mount + input resolution, before daemon spawn. Separated from
//! `mod.rs` so the FOD-whiteout commentary block (P0308 hang fix) lives
//! next to the mknod call instead of mid-orchestrator.

use std::path::Path;

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
const WORKER_NIX_CONF: &str = "\
builders =
substitute = false
sandbox = true
sandbox-fallback = false
restrict-eval = true
experimental-features = ca-derivations
";

/// Path where operators can mount a nix.conf override (via the
/// `rio-nix-conf` ConfigMap). If present, `setup_nix_conf` copies
/// THIS instead of using `WORKER_NIX_CONF`. Lets operators customize
/// experimental-features, sandbox paths, etc without image rebuild.
const NIX_CONF_OVERRIDE_PATH: &str = "/etc/rio/nix-conf/nix.conf";

/// Populate the sandbox: synthetic SQLite DB, nix.conf, FOD whiteouts.
///
/// Runs after overlay mount setup and input resolution. All side-effects
/// on the overlay's upper layer — no state returned. The overlay_mount
/// is held by the caller (execute_build) for later daemon spawn + upload
/// + teardown.
///
/// Steps:
/// 1. Generate synthetic DB from `synth_paths` (ValidPaths +
///    DerivationOutputs) so nix-daemon's isValidPath()/queryPartial
///    DerivationOutputMap() work without a real store.
/// 2. Write nix.conf (sandbox=true, substitute=false).
/// 3. For FODs: mknod whiteout char-devs for declared output paths so
///    daemon post-fail stat gets ENOENT from upper without probing FUSE.
#[instrument(skip_all, fields(drv_path = %drv_path, is_fod))]
pub(super) async fn prepare_sandbox(
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    synth_paths: Vec<ValidatedPathInfo>,
    is_fod: bool,
    effective_cores: u32,
) -> Result<(), ExecutorError> {
    // Generate synthetic DB from caller-supplied metadata (I-106:
    // captured during compute_input_closure's BFS, no second QPI pass).
    // CRITICAL: populate DerivationOutputs so nix-daemon's
    // queryPartialDerivationOutputMap(drvPath) returns our output paths.
    // Without it, initialOutputs[out].known is None → nix-daemon builds at
    // makeFallbackPath() (hash of "rewrite:<drvPath>:name:out" + zero hash),
    // but the builder's $out (from BasicDerivation env) is the REAL path →
    // output path mismatch → "builder failed to produce output path".
    //
    // CA floating outputs have an empty path (computed post-build from the
    // NAR hash). Inserting an empty-string path makes nix-daemon's
    // queryStaticPartialDerivationOutputMap call parseStorePath("") which
    // aborts the daemon (core dump). Real Nix never writes DerivationOutputs
    // rows for floating-CA — the output path is unknown until the build
    // finishes and the daemon writes a Realisations row instead. Filter them
    // here; nix-daemon computes scratchPath internally for CA outputs and
    // doesn't need the DerivationOutputs hint.
    let drv_outputs: Vec<SynthDrvOutput> = drv
        .outputs()
        .iter()
        .filter(|o| !o.path().is_empty())
        .map(|o| SynthDrvOutput {
            drv_path: drv_path.to_string(),
            output_name: o.name().to_string(),
            output_path: o.path().to_string(),
        })
        .collect();
    let db_dir = overlay::prepare_nix_state_dirs(&overlay_mount.upper_synth_db())?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths, &drv_outputs).await?;

    // Set up nix.conf in overlay
    setup_nix_conf(&overlay_mount.upper_nix_conf(), effective_cores)?;

    // Whiteout declared output paths in the overlay upper layer.
    //
    // r[impl builder.fod.verify-hash]
    //
    // P0308: FOD BuildResult propagation hang. When a builder exits
    // nonzero WITHOUT creating `$out` (wget 403 → exit 1, typical FOD
    // failure), nix-daemon's post-build cleanup — `deletePath(outputPath)`
    // in LocalDerivationGoal — stats `/nix/store/<output-basename>`.
    //
    // The overlay resolves this lookup layer by layer:
    //   upper ({upper}/nix/store/)  → ENOENT (builder never wrote it)
    //   lower (FUSE)                → lookup() → ensure_cached() → gRPC
    //
    // The FUSE gRPC should return ENOENT quickly, but empirically in the
    // k3s fixture it blocks — the daemon's stat syscall hangs, the daemon
    // never writes STDERR_LAST, and nix-build waits until `timeout 90`.
    //
    // Success path is unaffected: builder wrote `$out` → upper has it →
    // overlay resolves immediately, FUSE never probed.
    //
    // r[impl builder.fod.output-whiteout]
    // The whiteout fix: mknod a char device 0/0 for each output path
    // DIRECTLY IN THE UPPER DIR — bypassing overlay semantics entirely.
    //
    // Why not create-then-delete via the merged dir? Overlayfs only
    // writes a whiteout when `ovl_lower_positive()` returns true, i.e.
    // when at least one lower has the name. Here the FUSE lower ENOENTs
    // (output not yet in rio-store), so unlink via merged takes the
    // `ovl_remove_upper` path — plain unlink, no whiteout.
    // Empirically verified on Linux 6.12: create+rm via
    // merged for a name absent from all lowers leaves upper EMPTY.
    //
    // A char device 0/0 placed directly in the upperdir IS the
    // whiteout format (Documentation/filesystems/overlayfs.rst). The
    // kernel's merged-view lookup sees it and returns ENOENT without
    // consulting lowers — regardless of what lowers would say.
    // Post-whiteout:
    //
    //   - Daemon's pre-build deletePath(output): lstat → ENOENT (whiteout)
    //     → nothing to delete → returns. Whiteout survives.
    //   - Builder creates $out as FILE: open(O_CREAT)/link()/rename-file via
    //     merged → overlayfs replaces the whiteout with a real file in upper.
    //     Success path unchanged. **mkdir() onto a whiteout → EIO** (verified
    //     Linux 6.12; rename-dir → EXDEV) — the whiteout SURVIVES. Hence the
    //     is_fod guard below: fod-fetch.nix (the hang's repro) uses
    //     outputHashMode=flat → `wget -O $out` → file. Non-FOD `mkdir $out`
    //     callers (lifecycle.nix:171,218,227) skip the whiteout entirely.
    //     Recursive-mode FODs (NAR-hash dir outputs) are a known gap; not
    //     the plan's target, and the FUSE-spin hang is FOD-hash-verify-path
    //     specific — non-FOD failures don't probe $out the same way, so
    //     they never needed this whiteout.
    //   - Builder fails, $out never created: whiteout remains → daemon's
    //     post-fail stat gets ENOENT from upper → FUSE never probed →
    //     daemon proceeds → STDERR_LAST + BuildResult{PermanentFailure}.
    //
    // mknod(S_IFCHR) needs CAP_MKNOD. We hold it: this runs in the
    // worker's initial namespace before spawn_daemon_in_namespace, with
    // the same caps that mount the overlay (CAP_SYS_ADMIN ⊇ CAP_MKNOD
    // in practice; both granted to the worker pod). The syscall goes
    // straight to the upper's backing fs (local SSD) — the overlay and
    // FUSE are never consulted, so no spawn_blocking needed.
    //
    // `{upper}/nix/store/` is the overlayfs upperdir (overlay.rs:201).
    // It's a fresh empty dir per-build (mkdir_all at overlay setup),
    // so EEXIST is impossible here.
    if is_fod {
        let upper_store = overlay_mount.upper_store();
        // drv.is_fixed_output() ⇒ exactly one output named "out"
        // (derivation/mod.rs:211); loop body runs once.
        for out in drv.outputs() {
            // Output paths are always absolute store paths. An empty
            // path (CA derivations with unknown output paths) can't be
            // whitedout — skip and let the daemon's own logic handle it.
            let Some(basename) =
                rio_nix::store_path::basename(out.path()).filter(|b| !b.is_empty())
            else {
                continue;
            };
            let whiteout = upper_store.join(basename);
            nix::sys::stat::mknod(
                &whiteout,
                nix::sys::stat::SFlag::S_IFCHR,
                nix::sys::stat::Mode::empty(),
                0, // dev_t 0 (major 0, minor 0) — the overlayfs whiteout signature
            )
            .map_err(|errno| {
                // EPERM → missing CAP_MKNOD (pod securityContext regression).
                // EROFS → upper not writable (overlay misconfigured).
                // Either way the daemon spawn would fail; fail early with context.
                ExecutorError::DaemonSetup(format!(
                    "mknod whiteout for output {basename:?} at {}: {errno}",
                    whiteout.display()
                ))
            })?;
            // Diagnostic: verify the whiteout is visible as ENOENT through
            // the MERGED view. The mknod above writes directly to the
            // upperdir backing fs; this stat goes through overlayfs. If it
            // doesn't return ENOENT, the char-dev-0/0 whiteout approach
            // isn't being honored in this environment (nested overlay,
            // unusual mount options, kernel quirk) — the build will still
            // proceed but the FOD-failure hang fix won't take effect.
            // See TODO(P0311-T10) in netpol.nix (pre-ADR-019).
            let merged_check = overlay_mount.merged_dir().join(basename);
            match std::fs::symlink_metadata(&merged_check) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        output = basename,
                        upper = %whiteout.display(),
                        "FOD output whiteout created and visible as ENOENT via merged"
                    );
                }
                other => {
                    tracing::warn!(
                        output = basename,
                        upper = %whiteout.display(),
                        merged = %merged_check.display(),
                        result = ?other,
                        "FOD output whiteout NOT visible as ENOENT via merged — \
                         daemon post-fail stat may fall through to FUSE and hang"
                    );
                }
            }
        }
    }

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
fn setup_nix_conf(upper_nix_conf: &Path, effective_cores: u32) -> Result<(), ExecutorError> {
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

    std::fs::write(upper_nix_conf.join("nix.conf"), content).map_err(ExecutorError::NixConf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_nix_conf_content() {
        assert!(WORKER_NIX_CONF.contains("sandbox = true"));
        assert!(WORKER_NIX_CONF.contains("substitute = false"));
        assert!(WORKER_NIX_CONF.contains("builders ="));
        assert!(WORKER_NIX_CONF.contains("sandbox-fallback = false"));
    }

    // r[verify builder.cores.cgroup-clamp+2]
    #[test]
    fn test_setup_nix_conf() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conf_dir = dir.path().join("etc/nix");
        setup_nix_conf(&conf_dir, 2)?;

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
        // Never `cores = 0` (daemon resolves 0 → nproc, the I-196 bug).
        setup_nix_conf(&conf_dir, 0)?;
        let content = std::fs::read_to_string(&conf_path)?;
        assert!(content.contains("cores = 1\n"), "0 clamped to 1");
        assert!(!content.contains("cores = 0"));
        Ok(())
    }
}
