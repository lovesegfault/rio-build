//! Rook-Ceph operator + cluster bring-up.
//!
//! Two-phase: operator CRDs must establish before CephCluster/
//! CephObjectStore CRs apply. Idempotent (helm upgrade --install).

use std::collections::BTreeMap;
use std::time::Duration;

use ::kube::api::Api;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Secret;
use tracing::info;

use crate::sh::{cmd, shell};
use crate::{helm, kube, ui};

pub async fn install() -> Result<()> {
    let sh = shell()?;
    let client = kube::client().await?;

    let op = cmd!(sh, "nix build --no-link --print-out-paths .#helm-rook-ceph").read()?;
    let cl = cmd!(
        sh,
        "nix build --no-link --print-out-paths .#helm-rook-ceph-cluster"
    )
    .read()?;

    info!("rook operator");
    helm::Helm::upgrade_install("rook-ceph", &op)
        .namespace("rook-ceph")
        .create_namespace()
        .set("csi.enableRbdDriver", "false")
        .set("csi.enableCephfsDriver", "false")
        .run()?;
    kube::wait_rollout(
        &client,
        "rook-ceph",
        "rook-ceph-operator",
        Duration::from_secs(120),
    )
    .await?;

    info!("rook cluster");
    helm::Helm::upgrade_install("rook-ceph-cluster", &cl)
        .namespace("rook-ceph")
        .values("infra/helm/rook-dev-values.yaml")
        .run()?;

    // CephObjectStoreUser reconcile generates AccessKey/SecretKey.
    // Takes a while — mons+OSDs+RGW all have to be up first.
    info!("waiting for ObjectStoreUser secret (~2-5min)");
    kube::wait_secret_key(
        &client,
        "rook-ceph",
        "rook-ceph-object-user-rio-rio",
        "AccessKey",
        Duration::from_secs(600),
    )
    .await
}

/// Copy Rook's ObjectStoreUser secret → rio-system as rio-s3-creds.
/// Cross-namespace secretKeyRef doesn't work; store reads from rio-s3-creds.
/// Also creates the bucket (RGW doesn't auto-create).
pub async fn s3_bridge() -> Result<()> {
    let client = kube::client().await?;
    kube::ensure_namespace(&client, "rio-system", false).await?;

    let src: Api<Secret> = Api::namespaced(client.clone(), "rook-ceph");
    let secret = src.get("rook-ceph-object-user-rio-rio").await.context(
        "rook ObjectStoreUser secret not found — run `cargo xtask k8s provision -p k3s` first",
    )?;
    let data = secret.data.context("secret has no data")?;
    let get = |k: &str| -> Result<String> {
        Ok(std::str::from_utf8(&data.get(k).context("missing key")?.0)?.to_string())
    };
    let ak = get("AccessKey")?;
    let sk = get("SecretKey")?;

    kube::apply_secret(
        &client,
        "rio-system",
        "rio-s3-creds",
        BTreeMap::from([
            ("AccessKey".into(), ak.clone()),
            ("SecretKey".into(), sk.clone()),
        ]),
    )
    .await?;

    // Bucket creation via aws-cli against RGW, port-forwarded.
    // aws-sdk-s3 could do this too but port-forwarding via kube-rs
    // returns a raw io stream — simpler to shell out here.
    ui::step("create rio-chunks bucket", || async {
        let sh = shell()?;
        let pf = std::process::Command::new("kubectl")
            .args([
                "-n",
                "rook-ceph",
                "port-forward",
                "svc/rook-ceph-rgw-rio",
                "18080:80",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let _guard = scopeguard::guard(pf, |mut c| {
            let _ = c.kill();
        });
        tokio::time::sleep(Duration::from_secs(2)).await;

        let _env1 = sh.push_env("AWS_ACCESS_KEY_ID", &ak);
        let _env2 = sh.push_env("AWS_SECRET_ACCESS_KEY", &sk);
        let _ = cmd!(
            sh,
            "aws --endpoint-url http://localhost:18080 s3 mb s3://rio-chunks"
        )
        .quiet()
        .run();
        Ok(())
    })
    .await
}
