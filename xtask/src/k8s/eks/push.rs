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
use crate::sh::{cmd, repo_root, shell};
use crate::{git, tofu, ui};

/// Nix system → OCI arch (what k8s nodes advertise via kubernetes.io/arch).
const ARCHES: &[(&str, &str)] = &[("x86_64-linux", "amd64"), ("aarch64-linux", "arm64")];

/// skopeo refuses to run without a policy. "insecureAcceptAnything" =
/// don't require signature verification. Source is docker-archive
/// (local nix store), dest is our own ECR — no signatures to verify.
const POLICY_JSON: &str = r#"{"default":[{"type":"insecureAcceptAnything"}]}"#;

pub async fn run(cfg: &XtaskConfig) -> Result<()> {
    let ecr = tofu::output(TF_DIR, "ecr_registry")?;
    let region = tofu::output(TF_DIR, "region")?;

    let repo = git::open()?;
    let tag = git::image_tag(&repo)?;
    if tag.contains("-dirty-") {
        info!("dirty tree — tagging {tag}");
    }

    let out = tempfile::tempdir()?;
    let out_path = out.path();

    // Build both arch linkFarms.
    for (sys, arch) in ARCHES {
        build_arch(sys, arch, out_path, cfg).await?;
    }

    // ECR auth via aws-sdk-ecr → skopeo login.
    info!("ECR login ({ecr}, {region})");
    ecr_login(&ecr, &region).await?;

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

            let (name, arch, tag, ecr, policy, src) = (
                name.to_string(),
                arch.to_string(),
                tag.clone(),
                ecr.clone(),
                policy.clone(),
                path.to_str().unwrap().to_string(),
            );
            joinset.spawn(ui::step_owned(
                format!("rio-{name}:{tag}-{arch}"),
                async move {
                    let out = tokio::process::Command::new("skopeo")
                        .args(["--policy", &policy, "copy", "--retry-times", "3"])
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
    info!("creating multi-arch manifest lists");
    let sh = shell()?;
    for name in &names {
        info!("  rio-{name}:{tag} → {{amd64,arm64}}");
        crate::sh::run(cmd!(
            sh,
            "manifest-tool push from-args --platforms linux/amd64,linux/arm64 --template {ecr}/rio-{name}:{tag}-ARCH --target {ecr}/rio-{name}:{tag}"
        ))
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

async fn build_arch(sys: &str, arch: &str, out: &std::path::Path, cfg: &XtaskConfig) -> Result<()> {
    let sh = shell()?;
    let link = out.join(format!("images-{arch}"));
    let link_s = link.to_str().unwrap();
    let attr = format!(".#packages.{sys}.dockerImages");

    if let Some(remote) = &cfg.remote_store {
        info!("building {arch} images on {remote}");
        // Two-step: run() captures stderr (nix's -L build log) into the
        // spinner tail; then read() the resulting store path.
        crate::sh::run(cmd!(
            sh,
            "nix build {attr} -L --no-link --eval-store auto --store {remote}"
        ))
        .await?;
        let outpath = crate::sh::read(cmd!(
            sh,
            "nix path-info {attr} --eval-store auto --store {remote}"
        ))?;
        info!("copying {outpath} from {remote}");
        crate::sh::run(cmd!(
            sh,
            "nix copy --from {remote} --no-check-sigs {outpath}"
        ))
        .await?;
        std::os::unix::fs::symlink(&outpath, &link)?;
    } else {
        info!("building {arch} images locally (set RIO_REMOTE_STORE to offload)");
        crate::sh::run(cmd!(sh, "nix build {attr} -L --out-link {link_s}")).await?;
    }
    Ok(())
}

async fn ecr_login(registry: &str, region: &str) -> Result<()> {
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

    let mut child = std::process::Command::new("skopeo")
        .args(["login", "--username", user, "--password-stdin", registry])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.as_mut().unwrap().write_all(pass.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("skopeo login failed");
    }
    Ok(())
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
