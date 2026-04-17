//! Sandbox setup: synthetic SQLite DB, nix.conf.
//!
//! Side-effects on the overlay's upper layer only — runs after overlay
//! mount + input resolution, before daemon spawn.

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
/// 1. Generate synthetic DB from `synth_paths` (ValidPaths +
///    DerivationOutputs) so nix-daemon's isValidPath()/queryPartial
///    DerivationOutputMap() work without a real store.
/// 2. Write nix.conf (sandbox=true, substitute=false).
#[instrument(skip_all, fields(drv_path = %drv_path))]
pub(super) async fn prepare_sandbox(
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    synth_paths: Vec<ValidatedPathInfo>,
    effective_cores: u32,
    systems: &[String],
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
