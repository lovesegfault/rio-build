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
use tracing::info;

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::provider::BuiltImages;
use crate::sh::{cmd, repo_root, shell};
use crate::{git, tofu, ui};

/// Nix system → OCI arch (what k8s nodes advertise via kubernetes.io/arch).
const ARCHES: &[(&str, &str)] = &[("x86_64-linux", "amd64"), ("aarch64-linux", "arm64")];

/// skopeo refuses to run without a policy. "insecureAcceptAnything" =
/// don't require signature verification. Source is docker-archive
/// (local nix store), dest is our own ECR — no signatures to verify.
const POLICY_JSON: &str = r#"{"default":[{"type":"insecureAcceptAnything"}]}"#;

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
    let ecr = tofu::output(TF_DIR, "ecr_registry")?;
    let region = tofu::output(TF_DIR, "region")?;
    let tag = &images.tag;
    let out_path = images.dir.path();

    // Shared authfile. skopeo login defaults to $XDG_RUNTIME_DIR/containers/auth.json
    // but manifest-tool reads ~/.docker/config.json — they miss each
    // other. Write to a known path and pass it to both explicitly.
    // manifest-tool's --docker-cfg wants the DIRECTORY containing config.json.
    let docker_cfg = out_path.join("docker");
    std::fs::create_dir_all(&docker_cfg)?;
    let authfile = docker_cfg.join("config.json");
    let authfile = authfile.to_str().unwrap().to_string();
    let docker_cfg = docker_cfg.to_str().unwrap().to_string();

    ui::step(&format!("ECR login ({ecr})"), || {
        ecr_login(&ecr, &region, &authfile)
    })
    .await?;

    // Policy file (skopeo --policy is a global flag, needs a file).
    let policy = out_path.join("policy.json");
    std::fs::write(&policy, POLICY_JSON)?;
    let policy = policy.to_str().unwrap().to_string();

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

            let (name, arch, tag, ecr, policy, authfile, src) = (
                name.to_string(),
                arch.to_string(),
                tag.clone(),
                ecr.clone(),
                policy.clone(),
                authfile.clone(),
                path.to_str().unwrap().to_string(),
            );
            joinset.spawn(ui::step_owned(
                format!("rio-{name}:{tag}-{arch}"),
                async move {
                    let out = tokio::process::Command::new("skopeo")
                        .args(["--policy", &policy, "copy", "--retry-times", "3"])
                        .args(["--authfile", &authfile])
                        .args(["--dest-compress-format", "zstd"])
                        .args(["--dest-compress-level", "6", "-f", "oci"])
                        .arg(format!("docker-archive:{src}"))
                        .arg(format!("docker://{ecr}/rio-{name}:{tag}-{arch}"))
                        .output()
                        .await?;
                    if out.status.success() {
                        Ok::<Option<(String, String)>, anyhow::Error>(None)
                    } else {
                        // skopeo stderr is UTF-8; display-path, not parse-path.
                        #[allow(clippy::disallowed_methods)]
                        let log = String::from_utf8_lossy(&out.stderr).into_owned();
                        Ok(Some((format!("{name}-{arch}"), log)))
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
            tracing_indicatif::indicatif_eprintln!("  {id} FAILED:\n{}", indent(&log, "    "));
            failed.push(id);
        }
    }
    if !failed.is_empty() {
        bail!("{} push(es) failed: {}", failed.len(), failed.join(" "));
    }

    // Manifest lists (OCI image index) per image. Sequential — small
    // metadata-only PUTs, ~1s each.
    for name in &names {
        ui::step(&format!("manifest rio-{name}:{tag}"), || async {
            let sh = shell()?;
            crate::sh::run(cmd!(
                sh,
                "manifest-tool --docker-cfg {docker_cfg} push from-args --platforms linux/amd64,linux/arm64 --template {ecr}/rio-{name}:{tag}-ARCH --target {ecr}/rio-{name}:{tag}"
            ))
            .await
        })
        .await?;
    }

    info!(
        "done — pushed {} images × 2 arches + manifest lists, tag: {tag}",
        names.len()
    );
    // Handoff to deploy is via .rio-image-tag. A raw println! here
    // would scroll the terminal past MultiProgress's tracked bottom,
    // freezing a copy of the active bars in scrollback.
    std::fs::write(repo_root().join(".rio-image-tag"), format!("{tag}\n"))?;
    Ok(())
}

async fn build_all(out: &std::path::Path, cfg: &XtaskConfig) -> Result<()> {
    let sh = shell()?;
    let attrs: Vec<String> = ARCHES
        .iter()
        .map(|(sys, _)| format!(".#packages.{sys}.dockerImages"))
        .collect();

    let store_args = match &cfg.remote_store {
        Some(remote) => {
            info!("building images on {remote} (both arches, single eval)");
            vec![
                "--eval-store".into(),
                "auto".into(),
                "--store".into(),
                remote.clone(),
            ]
        }
        None => {
            info!("building images locally (both arches; set RIO_REMOTE_STORE to offload)");
            vec![]
        }
    };
    // Single command: --print-out-paths emits one store path per attr
    // on stdout (in arg order), -L build log on stderr. A separate
    // `nix path-info` re-eval can disagree with the build's eval under
    // `--eval-store auto --store remote` — ask the build itself.
    let (sa, at) = (&store_args, &attrs);
    let out_paths = ui::step("nix build (multi-arch)", || {
        crate::sh::run_read(cmd!(
            sh,
            "nix build -L --no-link --print-out-paths {sa...} {at...}"
        ))
    })
    .await?;
    let paths: Vec<&str> = out_paths.lines().collect();
    anyhow::ensure!(
        paths.len() == ARCHES.len(),
        "nix build returned {} paths for {} attrs",
        paths.len(),
        ARCHES.len()
    );

    if let Some(remote) = &cfg.remote_store {
        let p = &paths;
        ui::step(&format!("nix copy from {remote}"), || {
            crate::sh::run(cmd!(sh, "nix copy --from {remote} --no-check-sigs {p...}"))
        })
        .await?;
    }

    for ((_, arch), path) in ARCHES.iter().zip(&paths) {
        std::os::unix::fs::symlink(path, out.join(format!("images-{arch}")))?;
    }
    Ok(())
}

async fn ecr_login(registry: &str, region: &str, authfile: &str) -> Result<()> {
    let conf = aws_config::from_env()
        .region(aws_config::Region::new(region.to_string()))
        .load()
        .await;
    let ecr = aws_sdk_ecr::Client::new(&conf);
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

    // Capture stdio — inherited stdout/stderr would write "Login
    // Succeeded!" directly to the terminal, bypassing indicatif's
    // MultiProgress and freezing the phase bar into scrollback.
    let mut child = std::process::Command::new("skopeo")
        .args(["login", "--authfile", authfile])
        .args(["--username", user, "--password-stdin", registry])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.as_mut().unwrap().write_all(pass.as_bytes())?;
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
