//! Build + register the NixOS node AMI (ADR-021).
//!
//!   1. nix build .#node-ami-<arch>  → result/ with disk image +
//!      nix-support/image-info.json
//!   2. coldsnap upload <image>      → EBS snapshot (EBS Direct API,
//!      no S3 / VM-Import round-trip)
//!   3. aws ec2 register-image       → AMI ID
//!   4. aws ec2 create-tags          → rio.build/ami=<git-sha>,
//!      kubernetes.io/arch=<arch>, karpenter.sh/discovery=<cluster>
//!
//! Idempotent: if an AMI tagged with the current SHA + arch already
//! exists, skip upload and re-tag (cheap, makes "latest" move).
//!
//! P2 wires this into `xtask k8s eks up` between `push` and `deploy`;
//! P1 exposes it as a manual subcommand for the spike.

use std::path::Path;

use anyhow::{Context, Result};
use aws_sdk_ec2::types::{
    ArchitectureValues, BlockDeviceMapping, EbsBlockDevice, Filter, Tag, VolumeType,
};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use tracing::info;

use super::TF_DIR;
use crate::sh::{cmd, run_read, shell};
use crate::{git, tofu, ui};

#[derive(Args)]
pub struct AmiArgs {
    #[command(subcommand)]
    cmd: AmiCmd,
}

#[derive(clap::Subcommand)]
enum AmiCmd {
    /// nix build → coldsnap upload → register-image → tag.
    Push {
        /// Target architecture (or `all` for both).
        #[arg(long, value_enum, default_value_t = AmiArch::All)]
        arch: AmiArch,
        /// Skip the build and use an existing result/ dir (debugging).
        #[arg(long)]
        skip_build: bool,
    },
    // TODO(P2): Gc { keep: usize } — deregister AMIs + delete snapshots
    // where rio.build/ami ∉ {last N SHAs, "latest", any NodeClaim's
    // resolved AMI}.
}

#[derive(Copy, Clone, ValueEnum)]
enum AmiArch {
    X86_64,
    Aarch64,
    All,
}

impl AmiArch {
    /// (flake-attr suffix, EC2 architecture, kubernetes.io/arch tag value).
    fn targets(self) -> &'static [(&'static str, ArchitectureValues, &'static str)] {
        const X86: (&str, ArchitectureValues, &str) =
            ("x86_64", ArchitectureValues::X8664, "amd64");
        const ARM: (&str, ArchitectureValues, &str) =
            ("aarch64", ArchitectureValues::Arm64, "arm64");
        match self {
            AmiArch::X86_64 => &[X86],
            AmiArch::Aarch64 => &[ARM],
            AmiArch::All => &[X86, ARM],
        }
    }
}

/// nixpkgs amazon-image.nix writes this at `nix-support/image-info.json`.
/// Only the fields `register-image` needs are deserialized.
#[derive(Deserialize)]
struct ImageInfo {
    label: String,
    file: String,
    boot_mode: String,
}

pub async fn run(args: AmiArgs) -> Result<()> {
    match args.cmd {
        AmiCmd::Push { arch, skip_build } => push(arch, skip_build).await,
    }
}

async fn push(arch: AmiArch, skip_build: bool) -> Result<()> {
    let repo = git::open()?;
    let sha = git::short_sha(&repo)?;
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let cluster = tf.get("cluster_name")?;

    let conf = aws_config::from_env()
        .region(aws_config::Region::new(region.clone()))
        .load()
        .await;
    let ec2 = aws_sdk_ec2::Client::new(&conf);

    for &(attr, ref ec2_arch, k8s_arch) in arch.targets() {
        // Idempotency: an owned AMI already tagged with this SHA + arch
        // means a prior push for this commit succeeded — skip the
        // ~2 min coldsnap upload.
        if let Some(existing) = find_existing(&ec2, &sha, k8s_arch).await? {
            info!(
                "AMI {existing} already tagged rio.build/ami={sha} ({k8s_arch}) — skipping upload"
            );
            tag(&ec2, &existing, &sha, k8s_arch, &cluster).await?;
            continue;
        }

        let out = if skip_build {
            format!("result-node-ami-{attr}")
        } else {
            ui::step(&format!("nix build .#node-ami-{attr}"), || async {
                let sh = shell()?;
                run_read(cmd!(
                    sh,
                    "nix build -L --no-link --print-out-paths .#node-ami-{attr}"
                ))
                .await
            })
            .await?
        };
        let info = read_image_info(Path::new(out.trim()))?;

        let snap = ui::step(&format!("coldsnap upload ({k8s_arch})"), || async {
            // coldsnap's Rust SDK doesn't pick up SSO creds from
            // ~/.aws/sso/cache the way awscli does. Resolve via awscli
            // (which DOES) and pass the temp creds explicitly.
            // --wait polls until `completed` (register-image rejects
            // `pending`). stdout is the snapshot ID.
            let sh = shell()?;
            let creds: serde_json::Value = serde_json::from_str(
                &run_read(cmd!(sh, "aws configure export-credentials")).await?,
            )?;
            let _r = sh.push_env("AWS_REGION", &region);
            let _a = sh.push_env(
                "AWS_ACCESS_KEY_ID",
                creds["AccessKeyId"].as_str().unwrap_or_default(),
            );
            let _s = sh.push_env(
                "AWS_SECRET_ACCESS_KEY",
                creds["SecretAccessKey"].as_str().unwrap_or_default(),
            );
            let _t = sh.push_env(
                "AWS_SESSION_TOKEN",
                creds["SessionToken"].as_str().unwrap_or_default(),
            );
            let file = &info.file;
            let desc = format!("rio-nixos-node {sha} {k8s_arch}");
            run_read(cmd!(
                sh,
                "coldsnap upload --wait --description {desc} {file}"
            ))
            .await
            .map(|s| s.trim().to_string())
        })
        .await?;

        let ami = ui::step(&format!("register-image ({k8s_arch})"), || {
            register(&ec2, &info, &snap, ec2_arch.clone(), &sha, k8s_arch)
        })
        .await?;

        ui::step(&format!("tag {ami}"), || {
            tag(&ec2, &ami, &sha, k8s_arch, &cluster)
        })
        .await?;

        info!(
            "registered {ami} (snapshot {snap}) — rio.build/ami={sha} kubernetes.io/arch={k8s_arch}"
        );
    }
    // Handoff to deploy: same shape as .rio-image-tag (push.rs).
    std::fs::write(
        crate::sh::repo_root().join(".rio-ami-tag"),
        format!("{sha}\n"),
    )?;
    Ok(())
}

fn read_image_info(out: &Path) -> Result<ImageInfo> {
    let p = out.join("nix-support/image-info.json");
    let raw = std::fs::read_to_string(&p).with_context(|| {
        format!(
            "reading {} — did `nix build .#node-ami-*` run?",
            p.display()
        )
    })?;
    let info: ImageInfo = serde_json::from_str(&raw)?;
    Ok(info)
}

async fn find_existing(ec2: &aws_sdk_ec2::Client, sha: &str, arch: &str) -> Result<Option<String>> {
    let resp = ec2
        .describe_images()
        .owners("self")
        .filters(tag_filter("rio.build/ami", sha))
        .filters(tag_filter("kubernetes.io/arch", arch))
        .send()
        .await?;
    Ok(resp
        .images()
        .first()
        .and_then(|i| i.image_id().map(str::to_string)))
}

/// AMI Name + Description for register-image. Split out so the unit
/// test can assert ASCII without an EC2 client.
fn image_identity(info: &ImageInfo, sha: &str, k8s_arch: &str) -> (String, String) {
    // Name must be unique-per-account-per-region. label is the NixOS
    // system.nixos.label (release + git rev of nixpkgs); sha + arch
    // disambiguates rebuilds against the same nixpkgs.
    let name = format!("rio-nixos-node-{}-{sha}-{k8s_arch}", info.label);
    // EC2 rejects non-ASCII (em-dash etc.) in Description with
    // "Character sets beyond ASCII are not supported."
    let desc = format!("rio-build NixOS EKS node (ADR-021) - {sha}");
    debug_assert!(name.is_ascii() && desc.is_ascii());
    (name, desc)
}

async fn register(
    ec2: &aws_sdk_ec2::Client,
    info: &ImageInfo,
    snapshot_id: &str,
    arch: ArchitectureValues,
    sha: &str,
    k8s_arch: &str,
) -> Result<String> {
    let (name, desc) = image_identity(info, sha, k8s_arch);
    let resp = ec2
        .register_image()
        .name(&name)
        .description(desc)
        .architecture(arch)
        .virtualization_type("hvm")
        .root_device_name("/dev/xvda")
        .ena_support(true)
        .boot_mode(info.boot_mode.as_str().into())
        .block_device_mappings(
            BlockDeviceMapping::builder()
                .device_name("/dev/xvda")
                .ebs(
                    EbsBlockDevice::builder()
                        .snapshot_id(snapshot_id)
                        .delete_on_termination(true)
                        .volume_type(VolumeType::Gp3)
                        .build(),
                )
                .build(),
        )
        .send()
        .await?;
    resp.image_id()
        .map(str::to_string)
        .context("register-image returned no AMI ID")
}

async fn tag(
    ec2: &aws_sdk_ec2::Client,
    ami: &str,
    sha: &str,
    k8s_arch: &str,
    cluster: &str,
) -> Result<()> {
    // rio.build/ami=<sha> is what the EC2NodeClass amiSelectorTerms match.
    // "latest" is a moving tag for `values.yaml` default; pin to a SHA
    // for reproducible rollback. karpenter.sh/discovery scopes the AMI
    // to this cluster's selector (same key as subnets/SGs).
    ec2.create_tags()
        .resources(ami)
        .tags(mk_tag("rio.build/ami", sha))
        .tags(mk_tag("rio.build/ami-latest", "true"))
        .tags(mk_tag("kubernetes.io/arch", k8s_arch))
        .tags(mk_tag("karpenter.sh/discovery", cluster))
        .tags(mk_tag("Name", &format!("rio-nixos-node-{sha}-{k8s_arch}")))
        .send()
        .await?;
    Ok(())
}

fn mk_tag(k: &str, v: &str) -> Tag {
    Tag::builder().key(k).value(v).build()
}

fn tag_filter(k: &str, v: &str) -> Filter {
    Filter::builder().name(format!("tag:{k}")).values(v).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_targets_cover_both() {
        assert_eq!(AmiArch::All.targets().len(), 2);
        assert_eq!(AmiArch::X86_64.targets()[0].2, "amd64");
        assert_eq!(AmiArch::Aarch64.targets()[0].2, "arm64");
    }

    #[test]
    fn aws_strings_are_ascii() {
        // EC2 RegisterImage rejects Name/Description containing
        // non-ASCII (em-dash, smart quotes, etc.) with
        // InvalidParameterValue. Regression for the ADR-021 bringup
        // where an em-dash in Description failed register-image after
        // a successful 4GB coldsnap upload.
        let info = ImageInfo {
            label: "26.05.20260401.6201e20".into(),
            file: String::new(),
            boot_mode: "uefi".into(),
        };
        for &(_, _, k8s_arch) in AmiArch::All.targets() {
            let (name, desc) = image_identity(&info, "af8a6f093dcd", k8s_arch);
            assert!(name.is_ascii(), "non-ASCII in AMI name: {name:?}");
            assert!(desc.is_ascii(), "non-ASCII in AMI description: {desc:?}");
            // AMI Name: 3-128 chars, [A-Za-z0-9 ()./_-]. The label
            // and sha are alphanumeric+dot; k8s_arch is alphanumeric.
            assert!(name.len() <= 128);
        }
    }

    #[test]
    fn image_info_deserializes_nixpkgs_shape() {
        // Exact field names from nixpkgs maintainers/scripts/ec2/
        // amazon-image.nix postVM. Extra fields (system, logical_bytes,
        // disks) are ignored by serde — only assert the ones we read.
        let json = r#"{
            "label": "26.05.20260101.abcdef1",
            "boot_mode": "uefi",
            "system": "x86_64-linux",
            "file": "/nix/store/xxx-nixos-amazon-image/nixos.img",
            "logical_bytes": "8589934592"
        }"#;
        let info: ImageInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.boot_mode, "uefi");
        assert!(info.file.ends_with("nixos.img"));
        assert!(info.label.starts_with("26.05"));
    }
}
