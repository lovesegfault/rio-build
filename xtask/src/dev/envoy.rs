//! Envoy Gateway operator (dashboard gRPC-Web → gRPC+mTLS translation).

use std::time::Duration;

use anyhow::Result;
use tracing::info;

use crate::sh::{cmd, shell};
use crate::{helm, kube};

pub async fn install() -> Result<()> {
    let sh = shell()?;
    let client = kube::client().await?;

    let eg = cmd!(
        sh,
        "nix build --no-link --print-out-paths .#helm-envoy-gateway"
    )
    .read()?;

    info!("envoy-gateway operator + CRDs");
    helm::Helm::upgrade_install("envoy-gateway", &eg)
        .namespace("envoy-gateway-system")
        .create_namespace()
        .run()?;
    kube::wait_rollout(
        &client,
        "envoy-gateway-system",
        "envoy-gateway",
        Duration::from_secs(120),
    )
    .await
}
