//! Build multi-arch docker images + push to ECR.
//!
//! Replaces `infra/eks/push-images.sh`:
//!   1. nix build .#packages.{x86_64,aarch64}-linux.dockerImages
//!   2. skopeo copy → rio-foo:$tag-{amd64,arm64} (parallel, zstd, OCI)
//!   3. manifest-tool push from-args → rio-foo:$tag (OCI image index)
//!
//! Tag is git short-SHA plus `-dirty-${hash}` if the tree has changes.
//! ECR tags are immutable so the tag must uniquely identify content.

use std::collections::BTreeSet;
use std::io::Write;

use anyhow::{Context, Result, bail};
use base64::Engine;
use tokio::task::JoinSet;
use tracing::{error, info};

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::provider::BuiltImages;
use crate::sh::{cmd, shell};
use crate::{git, tofu, ui};

/// Nix system → OCI arch (what k8s nodes advertise via kubernetes.io/arch).
const ARCHES: &[(&str, &str)] = &[("x86_64-linux", "amd64"), ("aarch64-linux", "arm64")];

/// `manifest-tool --platforms` value derived from [`ARCHES`]. Every
/// other arch step in this file (`build_all`, the per-arch tag suffix,
/// `assert_in_ecr`) iterates `ARCHES`; the manifest list MUST cover the
/// same set or new-arch nodes silently ImagePullBackOff on the
/// manifest-list tag while the per-arch tag exists.
fn manifest_platforms() -> String {
    ARCHES
        .iter()
        .map(|(_, a)| format!("linux/{a}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// skopeo refuses to run without a policy. "insecureAcceptAnything" =
/// don't require signature verification. Source is docker-archive
/// (local nix store), dest is our own ECR — no signatures to verify.
const POLICY_JSON: &str = r#"{"default":[{"type":"insecureAcceptAnything"}]}"#;

/// `skopeo copy` flags for the docker-archive → OCI transcode.
///
/// **MUST match `ociSkopeoCopyArgs` in `nix/docker.nix`.** The NixOS
/// node AMI prebakes builder/fetcher layer blobs into containerd's
/// content store (r[infra.node.prebake-layer-warm]); containerd skips a
/// pull layer iff its digest is already present. A compress-level
/// mismatch between this push and the AMI seed yields different
/// compressed bytes → different digest → silent full re-fetch on every
/// fresh node. The `executor-seed-layer-parity` flake check guards the
/// Nix side; this comment is the Rust↔Nix tripwire.
const SKOPEO_OCI_ZSTD_ARGS: &[&str] = &[
    "--dest-compress-format",
    "zstd",
    "--dest-compress-level",
    "6",
    "-f",
    "oci",
];

/// One ECR push session: registry, tag, the shared authfile (skopeo and
/// manifest-tool both read it — their defaults miss each other), and
/// the skopeo policy file. Built by [`EcrSession::open`], which writes
/// the files under a caller-owned staging dir and performs the ECR
/// login. Plain strings so the parallel per-image pushes can each clone
/// a copy into their task.
#[derive(Clone)]
struct EcrSession {
    ecr: String,
    tag: String,
    /// skopeo --policy file path.
    policy: String,
    /// skopeo --authfile path (the config.json inside `docker_cfg`).
    authfile: String,
    /// Directory containing config.json — what manifest-tool's
    /// --docker-cfg wants (the directory, not the file).
    docker_cfg: String,
}

impl EcrSession {
    /// Write the authfile + policy under `dir` and log in to ECR. `dir`
    /// is owned by the caller (the BuiltImages temp dir for the full
    /// push, a fresh temp dir for [`push_single`]) and must outlive the
    /// session.
    async fn open(ecr: &str, region: &str, tag: &str, dir: &std::path::Path) -> Result<Self> {
        // skopeo login defaults to $XDG_RUNTIME_DIR/containers/auth.json
        // but manifest-tool reads ~/.docker/config.json — they miss each
        // other. Write to a known path and pass it to both explicitly.
        let docker_cfg = dir.join("docker");
        std::fs::create_dir_all(&docker_cfg)?;
        let authfile = docker_cfg.join("config.json");
        let authfile = authfile.to_str().unwrap().to_string();
        let docker_cfg = docker_cfg.to_str().unwrap().to_string();

        // Policy file (skopeo --policy is a global flag, needs a file).
        let policy = dir.join("policy.json");
        std::fs::write(&policy, POLICY_JSON)?;
        let policy = policy.to_str().unwrap().to_string();

        ui::step(&format!("ECR login ({ecr})"), || {
            ecr_login(ecr, region, &authfile)
        })
        .await?;

        Ok(Self {
            ecr: ecr.to_string(),
            tag: tag.to_string(),
            policy,
            authfile,
            docker_cfg,
        })
    }

    /// `skopeo copy` one docker-archive to `rio-{name}:{tag}-{arch}`.
    /// Failure carries skopeo's stderr in the error message.
    async fn skopeo_copy(&self, src: &str, name: &str, arch: &str) -> Result<()> {
        let out = tokio::process::Command::new("skopeo")
            .args(["--policy", &self.policy, "copy", "--retry-times", "3"])
            .args(["--authfile", &self.authfile])
            .args(SKOPEO_OCI_ZSTD_ARGS)
            .arg(format!("docker-archive:{src}"))
            .arg(format!(
                "docker://{}/rio-{name}:{}-{arch}",
                self.ecr, self.tag
            ))
            .output()
            .await?;
        if out.status.success() {
            return Ok(());
        }
        // skopeo stderr is UTF-8; display-path, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let log = String::from_utf8_lossy(&out.stderr).into_owned();
        bail!("skopeo copy rio-{name}:{}-{arch} failed:\n{log}", self.tag)
    }

    /// `manifest-tool push from-args` — the OCI image index tying the
    /// per-arch tags into the single `rio-{name}:{tag}` the chart pulls.
    async fn push_manifest(&self, name: &str) -> Result<()> {
        let (ecr, tag, docker_cfg) = (&self.ecr, &self.tag, &self.docker_cfg);
        let platforms = manifest_platforms();
        // Shell scoped tight (xshell::Shell is !Sync) so the returned
        // future stays Send.
        let fut = {
            let sh = shell()?;
            crate::sh::run(cmd!(
                sh,
                "manifest-tool --docker-cfg {docker_cfg} push from-args --platforms {platforms} --template {ecr}/rio-{name}:{tag}-ARCH --target {ecr}/rio-{name}:{tag}"
            ))
        };
        fut.await.with_context(|| format!("manifest rio-{name}"))
    }
}

/// nix build both arch linkFarms. Independent of provision outputs —
/// `up` joins this with provision concurrently.
pub async fn build(cfg: &XtaskConfig) -> Result<BuiltImages> {
    let repo = git::open()?;
    let tag = git::image_tag(&repo)?;
    if tag.contains("-dirty-") {
        info!("dirty tree — tagging {tag}");
    }

    let dir = tempfile::tempdir()?;
    build_all(dir.path(), cfg).await?;
    Ok(BuiltImages { dir, tag })
}

/// ECR login + skopeo copy + manifest lists. Needs tofu outputs
/// (ecr_registry, region) so cannot run before provision.
pub async fn push(images: &BuiltImages, _cfg: &XtaskConfig) -> Result<()> {
    let tf = tofu::outputs(TF_DIR)?;
    let ecr = tf.get("ecr_registry")?;
    let region = tf.get("region")?;
    let tag = &images.tag;
    let out_path = images.dir.path();

    let session = EcrSession::open(&ecr, &region, tag, out_path).await?;

    // Parallel push: one skopeo per image per arch.
    let mut names = BTreeSet::new();
    let mut joinset = JoinSet::new();

    for (_, arch) in ARCHES {
        let images_dir = out_path.join(format!("images-{arch}"));
        let mut found = 0;
        for entry in std::fs::read_dir(&images_dir)? {
            let path = entry?.path();
            let Some(fname) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            let Some(name) = fname.strip_suffix(".tar.zst") else {
                continue;
            };
            found += 1;
            names.insert(name.to_string());

            let (session, name, arch, src) = (
                session.clone(),
                name.to_string(),
                arch.to_string(),
                path.to_str().unwrap().to_string(),
            );
            joinset.spawn(ui::step_owned(
                format!("rio-{name}:{tag}-{arch}"),
                async move {
                    // Collect-all-errors: a failed copy is reported as a
                    // (id, log) pair, not bailed, so every failing image
                    // surfaces in one run.
                    match session.skopeo_copy(&src, &name, &arch).await {
                        Ok(()) => Ok::<Option<(String, String)>, anyhow::Error>(None),
                        Err(e) => Ok(Some((format!("{name}-{arch}"), format!("{e:#}")))),
                    }
                },
            ));
        }
        if found == 0 {
            bail!("no {arch} images in linkFarm — nix build produced nothing?");
        }
    }

    // Wait for ALL pushes (not just first failure) so every error surfaces.
    let mut failed = vec![];
    while let Some(res) = joinset.join_next().await {
        if let Some((id, log)) = res?? {
            error!("  {id} FAILED:\n{}", indent(&log, "    "));
            failed.push(id);
        }
    }
    if !failed.is_empty() {
        bail!("{} push(es) failed: {}", failed.len(), failed.join(" "));
    }

    // Manifest lists (OCI image index) per image. Parallel — each is
    // an independent metadata-only PUT (~1s); ~6 images well under
    // ECR's ~10 req/s PutImage limit so no concurrency cap. Same
    // collect-all-errors discipline as the skopeo JoinSet above.
    let mut joinset = JoinSet::new();
    for name in &names {
        let (session, name) = (session.clone(), name.clone());
        joinset.spawn(ui::step_owned(
            format!("manifest rio-{name}:{tag}"),
            async move { session.push_manifest(&name).await },
        ));
    }
    let mut failed = vec![];
    while let Some(res) = joinset.join_next().await {
        if let Err(e) = res? {
            error!("  {e:#}");
            failed.push(e);
        }
    }
    if !failed.is_empty() {
        bail!("{} manifest push(es) failed", failed.len());
    }

    info!(
        "done — pushed {} images × {} arches + manifest lists, tag: {tag}",
        names.len(),
        ARCHES.len()
    );
    Ok(())
}

/// Build + push ONE image to ECR (both arches + the manifest list):
/// `push_single(cfg, "replay")` lands `rio-replay:<tag>` where `<tag>`
/// is the current tree's content-addressed image tag.
///
/// Used by `replay setup` when the campaign-engine image is missing
/// from ECR: the chart is already deployed (its service images were
/// already pushed), so rebuilding and re-pushing every image just to
/// add the one missing repo would waste tens of minutes. Reuses the
/// same nix-build / skopeo / manifest-tool pipeline as the full push so
/// compression args (and therefore layer digests) cannot drift between
/// the two paths. `image` is the nix attr name (`.#dockerImages.<image>`
/// — the same derivation the full linkFarm links as `<image>.tar.zst`);
/// the ECR repo is `rio-<image>`.
pub async fn push_single(cfg: &XtaskConfig, image: &str) -> Result<()> {
    let tag = git::image_tag(&git::open()?)?;
    let tf = tofu::outputs(TF_DIR)?;
    let ecr = tf.get("ecr_registry")?;
    let region = tf.get("region")?;

    let attrs: Vec<String> = ARCHES
        .iter()
        .map(|(sys, _)| format!(".#packages.{sys}.dockerImages.{image}"))
        .collect();
    let paths = nix_build_attrs(&attrs, cfg).await?;

    // Staging dir for the auth/policy files (the full push stages them
    // in the BuiltImages dir; here there is no linkFarm to stage).
    let staging = tempfile::tempdir()?;
    let session = EcrSession::open(&ecr, &region, &tag, staging.path()).await?;

    for ((_, arch), src) in ARCHES.iter().zip(&paths) {
        ui::step(&format!("rio-{image}:{tag}-{arch}"), || {
            session.skopeo_copy(src, image, arch)
        })
        .await?;
    }
    ui::step(&format!("manifest rio-{image}:{tag}"), || {
        session.push_manifest(image)
    })
    .await?;
    info!(
        "pushed rio-{image}:{tag} ({} arches + manifest list)",
        ARCHES.len()
    );
    Ok(())
}

/// `nix build --print-out-paths` for `attrs`, honoring RIO_REMOTE_STORE
/// (build on the remote store, then `nix copy` the results back).
/// Returns one local store path per attr, in attr order.
async fn nix_build_attrs(attrs: &[String], cfg: &XtaskConfig) -> Result<Vec<String>> {
    let store_args = match &cfg.remote_store {
        Some(remote) => {
            info!("building images on {remote} (single eval)");
            vec![
                "--eval-store".into(),
                "auto".into(),
                "--store".into(),
                remote.clone(),
            ]
        }
        None => {
            info!("building images locally (set RIO_REMOTE_STORE to offload)");
            vec![]
        }
    };
    // Single command: --print-out-paths emits one store path per attr
    // on stdout (in arg order), -L build log on stderr. A separate
    // `nix path-info` re-eval can disagree with the build's eval under
    // `--eval-store auto --store remote` — ask the build itself.
    let (sa, at) = (&store_args, attrs);
    // Shell scoped so `&Shell` (`!Sync`) drops before the await — keeps
    // this future `Send` for the per-phase `tokio::spawn` (I-198).
    let build = {
        let sh = shell()?;
        crate::sh::run_read(cmd!(
            sh,
            "nix build -L --no-link --print-out-paths {sa...} {at...}"
        ))
    };
    let out_paths = ui::step("nix build (multi-arch)", || build).await?;
    let paths: Vec<String> = out_paths.lines().map(str::to_owned).collect();
    anyhow::ensure!(
        paths.len() == attrs.len(),
        "nix build returned {} paths for {} attrs",
        paths.len(),
        attrs.len()
    );

    if let Some(remote) = &cfg.remote_store {
        let p = &paths;
        let copy = {
            let sh = shell()?;
            crate::sh::run(cmd!(sh, "nix copy --from {remote} --no-check-sigs {p...}"))
        };
        ui::step(&format!("nix copy from {remote}"), || copy).await?;
    }
    Ok(paths)
}

async fn build_all(out: &std::path::Path, cfg: &XtaskConfig) -> Result<()> {
    let attrs: Vec<String> = ARCHES
        .iter()
        .map(|(sys, _)| format!(".#packages.{sys}.dockerImages"))
        .collect();
    let paths = nix_build_attrs(&attrs, cfg).await?;
    for ((_, arch), path) in ARCHES.iter().zip(&paths) {
        std::os::unix::fs::symlink(path, out.join(format!("images-{arch}")))?;
    }
    Ok(())
}

/// Whether `<repo>:{tag}` exists in ECR. `Ok(false)` only on ECR's
/// image-not-found response; every other failure (auth, network,
/// repository missing entirely) is an error.
pub async fn in_ecr(repo: &str, tag: &str, region: &str) -> Result<bool> {
    let conf = crate::aws::config(Some(region)).await;
    let ecr = aws_sdk_ecr::Client::new(conf);
    let found = ecr
        .describe_images()
        .repository_name(repo)
        .image_ids(
            aws_sdk_ecr::types::ImageIdentifier::builder()
                .image_tag(tag)
                .build(),
        )
        .send()
        .await;
    match found {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.as_service_error(), Some(se) if se.is_image_not_found_exception()) => {
            Ok(false)
        }
        Err(e) => Err(e).context("ECR DescribeImages"),
    }
}

/// Deploy/launch-time guard: bail if `<repo>:{tag}` isn't in ECR. Mirrors
/// [`super::ami::assert_registered`] — `--deploy` recomputes the image
/// tag (content-addressed via [`crate::git::image_tag`]); if the tree
/// drifted since `--push`, the recomputed tag won't be in ECR and this
/// fails with a clear "run --push first". The deploy path checks
/// `rio-gateway`, the canary repo (always pushed; see `rio_images` in
/// `infra/eks/ecr.tf`). The manifest-list tag (no `-{arch}` suffix) is
/// what the chart pulls, so that's what's checked.
pub async fn assert_in_ecr(repo: &str, tag: &str, region: &str) -> Result<()> {
    if in_ecr(repo, tag, region).await? {
        return Ok(());
    }
    bail!(
        "no {repo}:{tag} in ECR — run `cargo xtask k8s -p eks up --push` first \
         (deploying a non-existent tag wedges pods in ImagePullBackOff)"
    )
}

async fn ecr_login(registry: &str, region: &str, authfile: &str) -> Result<()> {
    let conf = crate::aws::config(Some(region)).await;
    let ecr = aws_sdk_ecr::Client::new(conf);
    let resp = ecr.get_authorization_token().send().await?;
    let token = resp
        .authorization_data()
        .first()
        .and_then(|d| d.authorization_token())
        .context("no ECR authorization token")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(token)?;
    let decoded = std::str::from_utf8(&decoded)?;
    let (user, pass) = decoded
        .split_once(':')
        .context("malformed ECR token (expected user:pass)")?;

    // Raw Command (not sh::run): --password-stdin needs piped stdin,
    // which run_inner nulls. Capture stdio so "Login Succeeded!" doesn't
    // land on the spinner line.
    let mut child = std::process::Command::new("skopeo")
        .args(["login", "--authfile", authfile])
        .args(["--username", user, "--password-stdin", registry])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .expect("set via Stdio::piped() above")
        .write_all(pass.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        #[allow(clippy::disallowed_methods)]
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("skopeo login failed: {err}");
    }
    Ok(())
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_platforms_derives_from_arches() {
        let p = manifest_platforms();
        assert_eq!(p.split(',').count(), ARCHES.len());
        for (_, oci) in ARCHES {
            assert!(p.contains(&format!("linux/{oci}")), "{p} missing {oci}");
        }
    }

    #[test]
    fn single_image_attrs_name_the_docker_images_passthru() {
        // push_single builds `.#packages.<sys>.dockerImages.<image>` — the
        // per-image passthru attr of the linkFarm package (flake.nix). One
        // attr per arch, in ARCHES order, so nix_build_attrs' positional
        // path↔arch zip holds.
        let attrs: Vec<String> = ARCHES
            .iter()
            .map(|(sys, _)| format!(".#packages.{sys}.dockerImages.replay"))
            .collect();
        assert_eq!(
            attrs,
            vec![
                ".#packages.x86_64-linux.dockerImages.replay",
                ".#packages.aarch64-linux.dockerImages.replay",
            ]
        );
    }
}
