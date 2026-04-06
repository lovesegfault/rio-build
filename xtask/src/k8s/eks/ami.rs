//! Build + register the NixOS node AMI (ADR-021).
//!
//!   1. nix build .#node-ami-<arch>  → result/ with disk image +
//!      nix-support/image-info.json
//!   2. coldsnap upload <image>      → EBS snapshot (EBS Direct API,
//!      no S3 / VM-Import round-trip)
//!   3. aws ec2 register-image       → AMI ID
//!   4. aws ec2 create-tags          → rio.build/ami=<tag>,
//!      rio.build/git-sha=<sha>, kubernetes.io/arch=<arch>,
//!      karpenter.sh/discovery=<cluster>
//!
//! Idempotent (I-182): the `rio.build/ami` tag value is the first 12
//! hex of `sha256(drvPath_x86_64 ++ drvPath_aarch64)` — content-
//! addressed, so it only changes when the NixOS module config or its
//! transitive nixpkgs closure does. A no-op `up` re-evaluates the
//! same drvPaths, finds both arches already tagged, and skips the
//! ~2×4 GB coldsnap uploads. The git SHA stays as a secondary
//! `rio.build/git-sha` tag for traceability (it changes every commit;
//! the content tag does not). Deploy resolves the tag back from EC2
//! via `rio.build/ami-latest=true` (`resolve_latest_tag`) — no
//! per-worktree handoff file.

use std::path::Path;

use anyhow::{Context, Result};
use aws_sdk_ec2::types::{
    ArchitectureValues, BlockDeviceMapping, EbsBlockDevice, Filter, Image, Tag, VolumeType,
};
use clap::ValueEnum;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::info;

use super::TF_DIR;
use crate::sh::{cmd, run_read, shell};
use crate::{git, tofu, ui};

#[derive(Copy, Clone, Default, PartialEq, Eq, ValueEnum)]
pub enum AmiArch {
    X86_64,
    Aarch64,
    #[default]
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

/// Content-addressed AMI tag: 12 hex chars of `sha256(∑ drvPaths)`.
///
/// `nix eval .#node-ami-<arch>.drvPath` is fast (instantiation only,
/// no build) and deterministic — same flake.lock + module config
/// → same drvPath → same tag. Hashing BOTH arches means the tag
/// changes iff either AMI's content would, including arch-specific
/// closure changes (e.g. arm firmware) that the x86 drvPath alone
/// would miss. Called by `up --ami` to find/tag; deploy reads the
/// tag back from EC2 (`resolve_latest_tag`), not by recomputing.
///
/// I-198: was sync `sh::read` — `nix eval` of a NixOS module is
/// multi-second per arch (×2). `run_read` spawns via tokio::process
/// and yields. Per-phase `tokio::spawn` (run_up_phases) means a stray
/// blocking call here would no longer stall siblings, but it would
/// still tie up a runtime worker.
pub async fn ami_tag() -> Result<String> {
    let mut h = Sha256::new();
    for &(attr, _, _) in AmiArch::All.targets() {
        // Shell scoped tight so it isn't held across the await
        // (xshell::Shell is !Sync; keeping the future Send-clean).
        let fut = {
            let sh = shell()?;
            run_read(cmd!(sh, "nix eval --raw .#node-ami-{attr}.drvPath"))
        };
        let drv = fut
            .await
            .with_context(|| format!("evaluating .#node-ami-{attr}.drvPath"))?;
        h.update(drv.trim().as_bytes());
    }
    Ok(hex::encode(&h.finalize()[..6]))
}

/// `up --ami` phase entry. Computes the content-addressed tag,
/// short-circuits if every requested arch already has an AMI tagged
/// with it, otherwise builds + uploads + registers + tags the missing
/// ones. Deploy reads the tag from EC2 (`resolve_latest_tag`), not a
/// handoff file.
pub async fn run_phase(arch: AmiArch) -> Result<()> {
    let repo = git::open()?;
    let sha = git::short_sha(&repo)?;
    let ami_tag = ami_tag().await?;
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let cluster = tf.get("cluster_name")?;

    let conf = crate::aws::config(Some(&region)).await;
    let ec2 = aws_sdk_ec2::Client::new(conf);

    // I-182 fast path: every requested arch already registered for
    // this content tag → write the handoff file and stop. No build,
    // no coldsnap, no re-tag.
    let mut all_present = true;
    for &(_, _, k8s_arch) in arch.targets() {
        if find_existing(&ec2, &ami_tag, k8s_arch).await?.is_none() {
            all_present = false;
            break;
        }
    }
    if all_present {
        info!("AMIs for rio.build/ami={ami_tag} already registered — skipping build");
        return Ok(());
    }

    // join_all (not try_join_all): both arches run to completion even
    // if one fails — don't cancel a ~4 GB coldsnap upload mid-flight
    // because the other arch errored. Same "let in-flight work finish"
    // principle as run_up_phases. The nix build + upload are ~10–15 min
    // each and fully independent, so AmiArch::All halves wall time.
    // `nix build -L` stderr from both interleaves; ui::step is
    // concurrency-safe (f8db656d).
    let results = futures_util::future::join_all(
        arch.targets()
            .iter()
            .map(|t| build_and_register_one(&ec2, &ami_tag, &sha, &region, &cluster, t)),
    )
    .await;
    for r in results {
        r?;
    }
    Ok(())
}

/// One arch's build → coldsnap upload → register-image → tag pipeline.
/// Extracted from `run_phase` so `AmiArch::All` runs both concurrently.
async fn build_and_register_one(
    ec2: &aws_sdk_ec2::Client,
    ami_tag: &str,
    sha: &str,
    region: &str,
    cluster: &str,
    &(attr, ref ec2_arch, k8s_arch): &(&'static str, ArchitectureValues, &'static str),
) -> Result<()> {
    // Per-arch idempotency: a prior partial push (e.g. x86 done,
    // aarch64 interrupted) skips the done arch.
    if let Some(existing) = find_existing(ec2, ami_tag, k8s_arch).await? {
        info!(
            "AMI {existing} already tagged rio.build/ami={ami_tag} ({k8s_arch}) — skipping upload"
        );
        tag(ec2, &existing, ami_tag, sha, k8s_arch, cluster).await?;
        untag_prior_latest(ec2, &existing, k8s_arch).await?;
        return Ok(());
    }

    let build = {
        let sh = shell()?;
        run_read(cmd!(
            sh,
            "nix build -L --no-link --print-out-paths .#node-ami-{attr}"
        ))
    };
    let out = ui::step(&format!("nix build .#node-ami-{attr}"), || build).await?;
    let info = read_image_info(Path::new(out.trim()))?;

    let snap = ui::step(&format!("coldsnap upload ({k8s_arch})"), || async {
        // coldsnap's Rust SDK doesn't pick up SSO creds from
        // ~/.aws/sso/cache the way awscli does. Resolve via awscli
        // (which DOES) and pass the temp creds explicitly.
        // --wait polls until `completed` (register-image rejects
        // `pending`). stdout is the snapshot ID.
        let creds: serde_json::Value = serde_json::from_str(
            &{
                let sh = shell()?;
                run_read(cmd!(sh, "aws configure export-credentials"))
            }
            .await?,
        )?;
        let file = &info.file;
        let desc = format!("rio-nixos-node {ami_tag} {k8s_arch}");
        // I-198: was `sh.push_env()` RAII guards (hold `&Shell`, `!Sync`)
        // across the await — broke per-phase `tokio::spawn`. Per-command
        // `.env()` keeps the future `Send`.
        {
            let sh = shell()?;
            run_read(
                cmd!(
                    sh,
                    "coldsnap upload --wait --omit-zero-blocks --description {desc} {file}"
                )
                .env("AWS_REGION", region)
                .env(
                    "AWS_ACCESS_KEY_ID",
                    creds["AccessKeyId"].as_str().unwrap_or_default(),
                )
                .env(
                    "AWS_SECRET_ACCESS_KEY",
                    creds["SecretAccessKey"].as_str().unwrap_or_default(),
                )
                .env(
                    "AWS_SESSION_TOKEN",
                    creds["SessionToken"].as_str().unwrap_or_default(),
                ),
            )
        }
        .await
        .map(|s| s.trim().to_string())
    })
    .await?;

    let ami = ui::step(&format!("register-image ({k8s_arch})"), || {
        register(ec2, &info, &snap, ec2_arch.clone(), ami_tag, k8s_arch)
    })
    .await?;

    ui::step(&format!("tag {ami}"), || {
        tag(ec2, &ami, ami_tag, sha, k8s_arch, cluster)
    })
    .await?;
    untag_prior_latest(ec2, &ami, k8s_arch).await?;

    info!(
        "registered {ami} (snapshot {snap}) — rio.build/ami={ami_tag} kubernetes.io/arch={k8s_arch}"
    );
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

async fn find_existing(
    ec2: &aws_sdk_ec2::Client,
    ami_tag: &str,
    arch: &str,
) -> Result<Option<String>> {
    let resp = ec2
        .describe_images()
        .owners("self")
        .filters(tag_filter("rio.build/ami", ami_tag))
        .filters(tag_filter("kubernetes.io/arch", arch))
        .send()
        .await?;
    Ok(resp
        .images()
        .first()
        .and_then(|i| i.image_id().map(str::to_string)))
}

/// I-182 read side: resolve the deploy-time `rio.build/ami` tag value
/// from EC2, not the per-worktree `.rio-ami-tag` file. `tag()` below
/// stamps every newly-registered AMI with `rio.build/ami-latest=true`
/// and `untag_prior_latest()` strips it from older generations of the
/// same arch; query for that and read back the content-addressed
/// `rio.build/ami` value. If multiple generations still carry
/// `ami-latest` (interrupted untag, or pre-untag-step images), the
/// newest by `CreationDate` wins — ISO 8601 strings sort
/// lexically. A worktree that never ran `up --ami` now deploys the
/// same tag any other worktree would; previously it read a stale
/// gitignored file or recomputed a drvPath-hash that pointed at
/// nothing (the `assert_registered` guard caught the latter, but the
/// former silently deployed an old AMI).
pub async fn resolve_latest_tag(region: &str) -> Result<String> {
    let conf = crate::aws::config(Some(region)).await;
    let ec2 = aws_sdk_ec2::Client::new(conf);
    let resp = ec2
        .describe_images()
        .owners("self")
        .filters(tag_filter("rio.build/ami-latest", "true"))
        .send()
        .await?;
    latest_ami_tag_of(resp.images())
}

/// Pure half of `resolve_latest_tag` — newest image's `rio.build/ami`
/// tag. Split for unit testing without an EC2 client.
fn latest_ami_tag_of(images: &[Image]) -> Result<String> {
    let newest = images
        .iter()
        .max_by_key(|i| i.creation_date().unwrap_or_default())
        .with_context(|| {
            "no AMI tagged rio.build/ami-latest=true — \
             run `cargo xtask k8s -p eks up --ami` first"
        })?;
    newest
        .tags()
        .iter()
        .find(|t| t.key() == Some("rio.build/ami"))
        .and_then(|t| t.value())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "AMI {} is tagged rio.build/ami-latest=true but has no rio.build/ami tag — \
                 retag via `cargo xtask k8s -p eks up --ami`",
                newest.image_id().unwrap_or("?")
            )
        })
}

/// `up --deploy` guard: bail if the resolved amiTag has no registered
/// AMI for either arch. Without this, deploy renders the tag into the
/// EC2NodeClass amiSelectorTerms, Karpenter's AMINotFound makes EVERY
/// NodePool NotReady, and the cluster stops provisioning until someone
/// patches the EC2NodeClass back. Still useful post-I-182: catches a
/// half-registered tag (only one arch uploaded before interrupt).
pub async fn assert_registered(ami_tag: &str, region: &str) -> Result<()> {
    let conf = crate::aws::config(Some(region)).await;
    let ec2 = aws_sdk_ec2::Client::new(conf);
    for &(_, _, k8s_arch) in AmiArch::All.targets() {
        if find_existing(&ec2, ami_tag, k8s_arch).await?.is_none() {
            anyhow::bail!(
                "no AMI tagged rio.build/ami={ami_tag} ({k8s_arch}) — \
                 run `cargo xtask k8s -p eks up --ami` first \
                 (deploying a non-existent tag wedges Karpenter)"
            );
        }
    }
    Ok(())
}

/// AMI Name + Description for register-image. Split out so the unit
/// test can assert ASCII without an EC2 client.
fn image_identity(info: &ImageInfo, ami_tag: &str, k8s_arch: &str) -> (String, String) {
    // Name must be unique-per-account-per-region. label is the NixOS
    // system.nixos.label (release + git rev of nixpkgs); the
    // content-addressed tag + arch disambiguates.
    let name = format!("rio-nixos-node-{}-{ami_tag}-{k8s_arch}", info.label);
    // EC2 rejects non-ASCII (em-dash etc.) in Description with
    // "Character sets beyond ASCII are not supported."
    let desc = format!("rio-build NixOS EKS node (ADR-021) - {ami_tag}");
    debug_assert!(name.is_ascii() && desc.is_ascii());
    (name, desc)
}

async fn register(
    ec2: &aws_sdk_ec2::Client,
    info: &ImageInfo,
    snapshot_id: &str,
    arch: ArchitectureValues,
    ami_tag: &str,
    k8s_arch: &str,
) -> Result<String> {
    let (name, desc) = image_identity(info, ami_tag, k8s_arch);
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
    ami_tag: &str,
    git_sha: &str,
    k8s_arch: &str,
    cluster: &str,
) -> Result<()> {
    // rio.build/ami=<tag> is what the EC2NodeClass amiSelectorTerms
    // match — content-addressed (I-182), pin to a value for
    // reproducible rollback. rio.build/git-sha is traceability only
    // (changes every commit; the content tag does not).
    // karpenter.sh/discovery scopes the AMI to this cluster's selector
    // (same key as subnets/SGs).
    ec2.create_tags()
        .resources(ami)
        .tags(mk_tag("rio.build/ami", ami_tag))
        .tags(mk_tag("rio.build/git-sha", git_sha))
        .tags(mk_tag("rio.build/ami-latest", "true"))
        .tags(mk_tag("kubernetes.io/arch", k8s_arch))
        .tags(mk_tag("karpenter.sh/discovery", cluster))
        .tags(mk_tag(
            "Name",
            &format!("rio-nixos-node-{ami_tag}-{k8s_arch}"),
        ))
        .send()
        .await?;
    Ok(())
}

/// I-182 write side: strip `rio.build/ami-latest` from prior
/// generations of this arch. Runs AFTER `tag()` so a failure here
/// leaves the new AMI tagged (deploy still resolves it via
/// `max(CreationDate)`); idempotent (delete-tags on an absent tag is a
/// no-op). Without this, every generation accumulates `ami-latest=true`
/// and `resolve_latest_tag` walks an ever-growing describe-images
/// result. Only the `ami-latest` key is removed — `rio.build/ami`
/// (content tag) and `rio.build/git-sha` stay for rollback pinning.
async fn untag_prior_latest(
    ec2: &aws_sdk_ec2::Client,
    keep_ami: &str,
    k8s_arch: &str,
) -> Result<()> {
    let resp = ec2
        .describe_images()
        .owners("self")
        .filters(tag_filter("rio.build/ami-latest", "true"))
        .filters(tag_filter("kubernetes.io/arch", k8s_arch))
        .send()
        .await?;
    let prior: Vec<String> = resp
        .images()
        .iter()
        .filter_map(|i| i.image_id())
        .filter(|id| *id != keep_ami)
        .map(str::to_string)
        .collect();
    if prior.is_empty() {
        return Ok(());
    }
    info!(
        "untagging rio.build/ami-latest from {} prior {k8s_arch} AMI(s)",
        prior.len()
    );
    ec2.delete_tags()
        .set_resources(Some(prior))
        .tags(Tag::builder().key("rio.build/ami-latest").build())
        .send()
        .await?;
    Ok(())
}

/// `xtask k8s ami gc`: deregister stale rio AMIs + delete their backing
/// snapshots. "Stale" = tagged `rio.build/ami` (any value), NOT tagged
/// `rio.build/ami-latest=true`, and `CreationDate` older than
/// `older_than_days`. The latest tag is what `up --deploy` resolves
/// (see `resolve_latest_tag`), so anything carrying it is live by
/// definition and never collected. `dry_run` (the default) prints the
/// candidate set without touching AWS.
///
/// Each `up --ami` that changes the NixOS closure leaves the prior
/// generation's 2×~4 GB snapshots behind (`untag_prior_latest` only
/// strips the `ami-latest` tag — it keeps the AMI for rollback). Weekly
/// gc bounds the accumulation.
pub async fn gc(older_than_days: u64, dry_run: bool) -> Result<()> {
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let conf = crate::aws::config(Some(&region)).await;
    let ec2 = aws_sdk_ec2::Client::new(conf);

    // All self-owned AMIs that carry the rio.build/ami tag (any value).
    // The ami-latest exclusion + age filter happen client-side in
    // gc_candidates so the selection logic is unit-testable.
    let resp = ec2
        .describe_images()
        .owners("self")
        .filters(
            Filter::builder()
                .name("tag-key")
                .values("rio.build/ami")
                .build(),
        )
        .send()
        .await?;

    let cutoff =
        jiff::Timestamp::now() - jiff::SignedDuration::from_hours(older_than_days as i64 * 24);
    let victims = gc_candidates(resp.images(), cutoff);

    if victims.is_empty() {
        info!(
            "ami gc: nothing to collect (no rio.build/ami images older than {older_than_days}d \
             without rio.build/ami-latest)"
        );
        return Ok(());
    }

    for (id, created, snaps) in &victims {
        info!(
            "{} {id} (created {created}, {} snapshot(s): {})",
            if dry_run {
                "would deregister"
            } else {
                "deregister"
            },
            snaps.len(),
            snaps.join(",")
        );
    }

    if dry_run {
        info!(
            "ami gc: dry-run — {} AMI(s) would be deregistered; \
             pass --no-dry-run to actually delete",
            victims.len()
        );
        return Ok(());
    }

    for (id, _, snaps) in &victims {
        ui::step(&format!("deregister {id}"), || async {
            ec2.deregister_image().image_id(id).send().await?;
            for snap in snaps {
                ec2.delete_snapshot().snapshot_id(snap).send().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    }
    info!("ami gc: deregistered {} AMI(s)", victims.len());
    Ok(())
}

/// Pure half of [`gc`]: select `(image_id, creation_date, snapshot_ids)`
/// for every image that (a) is NOT tagged `rio.build/ami-latest=true`
/// and (b) has a `CreationDate` strictly before `cutoff`. Images
/// missing an ID or with an unparseable date are skipped (conservative
/// — never delete what we can't age). Split out for unit testing
/// without an EC2 client, mirroring `latest_ami_tag_of`.
fn gc_candidates(images: &[Image], cutoff: jiff::Timestamp) -> Vec<(String, String, Vec<String>)> {
    images
        .iter()
        .filter(|i| {
            !i.tags()
                .iter()
                .any(|t| t.key() == Some("rio.build/ami-latest") && t.value() == Some("true"))
        })
        .filter_map(|i| {
            let id = i.image_id()?.to_string();
            let created = i.creation_date()?;
            let ts: jiff::Timestamp = created.parse().ok()?;
            if ts >= cutoff {
                return None;
            }
            let snaps = i
                .block_device_mappings()
                .iter()
                .filter_map(|b| b.ebs().and_then(|e| e.snapshot_id()).map(str::to_string))
                .collect();
            Some((id, created.to_string(), snaps))
        })
        .collect()
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
    fn latest_ami_tag_picks_newest_and_reads_rio_tag() {
        // I-182 read side: two generations both tagged ami-latest=true
        // (interrupted untag or pre-untag-step images) — newest
        // CreationDate wins, and the value returned is the
        // rio.build/ami tag, not the image ID.
        let img = |id: &str, date: &str, ami: &str| {
            Image::builder()
                .image_id(id)
                .creation_date(date)
                .tags(mk_tag("rio.build/ami-latest", "true"))
                .tags(mk_tag("rio.build/ami", ami))
                .tags(mk_tag("kubernetes.io/arch", "amd64"))
                .build()
        };
        let images = vec![
            img("ami-old", "2026-03-01T00:00:00.000Z", "aaaaaaaaaaaa"),
            img("ami-new", "2026-04-01T00:00:00.000Z", "bbbbbbbbbbbb"),
        ];
        assert_eq!(latest_ami_tag_of(&images).unwrap(), "bbbbbbbbbbbb");

        // No images → actionable error naming the fix.
        let err = latest_ami_tag_of(&[]).unwrap_err().to_string();
        assert!(err.contains("up --ami"), "{err}");

        // ami-latest present but rio.build/ami missing → names the AMI.
        let broken = [Image::builder()
            .image_id("ami-broken")
            .creation_date("2026-04-01T00:00:00.000Z")
            .tags(mk_tag("rio.build/ami-latest", "true"))
            .build()];
        let err = latest_ami_tag_of(&broken).unwrap_err().to_string();
        assert!(err.contains("ami-broken"), "{err}");
    }

    #[test]
    fn gc_candidates_excludes_latest_and_recent() {
        let bdm = |snap: &str| {
            BlockDeviceMapping::builder()
                .device_name("/dev/xvda")
                .ebs(EbsBlockDevice::builder().snapshot_id(snap).build())
                .build()
        };
        let img = |id: &str, date: &str, latest: bool, snap: &str| {
            let mut b = Image::builder()
                .image_id(id)
                .creation_date(date)
                .tags(mk_tag("rio.build/ami", "deadbeef0000"))
                .block_device_mappings(bdm(snap));
            if latest {
                b = b.tags(mk_tag("rio.build/ami-latest", "true"));
            }
            b.build()
        };
        let images = vec![
            // old, not latest → collected
            img("ami-old", "2026-03-01T00:00:00.000Z", false, "snap-old"),
            // old but latest → kept (live)
            img("ami-live", "2026-03-01T00:00:00.000Z", true, "snap-live"),
            // recent, not latest → kept (within retention window)
            img("ami-new", "2026-04-02T00:00:00.000Z", false, "snap-new"),
            // unparseable date → skipped (conservative)
            img("ami-bad", "garbage", false, "snap-bad"),
        ];
        let cutoff: jiff::Timestamp = "2026-03-28T00:00:00Z".parse().unwrap();
        let v = gc_candidates(&images, cutoff);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].0, "ami-old");
        assert_eq!(v[0].2, vec!["snap-old"]);

        // Empty input → empty output.
        assert!(gc_candidates(&[], cutoff).is_empty());
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
