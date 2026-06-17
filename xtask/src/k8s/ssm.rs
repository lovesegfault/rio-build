//! SSM remote-host port-forward: the EKS tunnel transport.
//!
//! `kubectl port-forward` drops under sustained throughput (apiserver
//! SPDY in the data path) — a long `rsb`/`k8s build` cuts at ~13min
//! with the gateway logging client-side `early eof`. SSM goes
//! websocket→agent-on-node→`host:port`; the same build completes.
//!
//! In-cluster targets use [`tunnel_pod`]: the relay is the pod's OWN
//! node, dialling the pod IP — host netns reaches local overlay IPs
//! regardless of `bpf-lb-sock`/Service type, so one path covers
//! gateway, scheduler-leader and store. VPC-routable hosts (RDS) use
//! [`tunnel_host`] via a cached any-node relay.
//!
//! [`relay()`] is `None` on k3s / no SSM-online node → callers fall
//! back to kubectl.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::OnceCell;

use super::client as kube;
use super::shared::ProcessGuard;

static RELAY: OnceCell<Option<String>> = OnceCell::const_new();

/// Instance-id of an SSM-online cluster node, or `None` (k3s, or EKS
/// before one has registered). Cached — discovery is ~1s and QA opens
/// ~50 tunnels per run. The context check keeps the `aws` CLI off the
/// k3s path.
pub async fn relay() -> Option<&'static str> {
    RELAY
        .get_or_init(|| async {
            if !kube::current_context()
                .ok()
                .is_some_and(|c| c.starts_with("arn:aws:eks:"))
            {
                return None;
            }
            match discover_relay().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("ssm relay discovery failed; falling back to kubectl: {e:#}");
                    None
                }
            }
        })
        .await
        .as_deref()
}

async fn discover_relay() -> Result<Option<String>> {
    let nodes = cluster_instances().await?;
    let online = ssm_online().await?;
    // Prefer managed-nodegroup nodes — no Karpenter/spot churn.
    let pick = nodes
        .iter()
        .find(|(id, managed)| *managed && online.contains(id))
        .or_else(|| nodes.iter().find(|(id, _)| online.contains(id)));
    if let Some((id, managed)) = pick {
        tracing::debug!(relay = %id, managed, "ssm relay selected");
        Ok(Some(id.clone()))
    } else {
        tracing::warn!(
            "no SSM-online cluster node found ({} nodes, {} ssm-online in account); \
             falling back to kubectl port-forward. Apply infra/eks (system nodegroup \
             AmazonSSMManagedInstanceCore) or wait for a Karpenter node.",
            nodes.len(),
            online.len()
        );
        Ok(None)
    }
}

/// `(instance-id, is-managed-nodegroup)` for every cluster Node.
async fn cluster_instances() -> Result<Vec<(String, bool)>> {
    use ::kube::api::{Api, ListParams};
    use k8s_openapi::api::core::v1::Node;
    let client = kube::client().await?;
    let api: Api<Node> = Api::all(client);
    let mut out = Vec::new();
    for n in api.list(&ListParams::default()).await?.items {
        let managed = n
            .metadata
            .labels
            .as_ref()
            .is_some_and(|l| l.contains_key("eks.amazonaws.com/nodegroup"));
        // providerID = aws:///<zone>/<instance-id>
        if let Some(id) = n
            .spec
            .and_then(|s| s.provider_id)
            .and_then(|p| p.rsplit('/').next().map(str::to_owned))
        {
            out.push((id, managed));
        }
    }
    Ok(out)
}

/// Instance-ids with `PingStatus=Online`. Shells out: `start-session`
/// already needs the CLI on PATH; pulling `aws-sdk-ssm` for one query
/// isn't worth the build time.
async fn ssm_online() -> Result<Vec<String>> {
    let out = tokio::process::Command::new("aws")
        .args([
            "ssm",
            "describe-instance-information",
            "--query",
            "InstanceInformationList[?PingStatus=='Online'].InstanceId",
            "--output",
            "json",
        ])
        .output()
        .await
        .context("spawn aws ssm describe-instance-information")?;
    if !out.status.success() {
        anyhow::bail!(
            "aws ssm describe-instance-information failed: {}",
            std::str::from_utf8(&out.stderr)
                .unwrap_or("<non-utf8>")
                .trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse ssm describe-instance-information output")
}

/// SSM-tunnel to a VPC-routable host (RDS endpoint) via the cached
/// any-node [`relay()`]. Gate on `relay()` first for kubectl fallback.
pub async fn tunnel_host(host: &str, remote: u16, local: u16) -> Result<(u16, ProcessGuard)> {
    let relay = relay().await.context(
        "no SSM relay node available (need an SSM-online EKS node; \
         system nodegroup gets AmazonSSMManagedInstanceCore via infra/eks)",
    )?;
    tunnel_via(relay, host, remote, local).await
}

/// SSM-tunnel to `pod`'s container `port` via the pod's own node —
/// host netns reaches local overlay IPs but not cross-node ones, so
/// the relay must be co-located. `port` is the CONTAINER port, not the
/// Service port.
pub async fn tunnel_pod(
    client: &::kube::Client,
    ns: &str,
    pod: &str,
    port: u16,
    local: u16,
) -> Result<(u16, ProcessGuard)> {
    let (ip, node) = kube::pod_addr(client, ns, pod).await?;
    tunnel_via(&node, &ip, port, local).await
}

/// `AWS-StartPortForwardingSessionToRemoteHost`: `local → host:remote`
/// from `relay`'s netns. `local = 0` binds ephemeral; the bound port
/// is parsed from the plugin's `Port NNNNN opened …` line.
async fn tunnel_via(
    relay: &str,
    host: &str,
    remote: u16,
    local: u16,
) -> Result<(u16, ProcessGuard)> {
    let params = serde_json::json!({
        "host": [host],
        "portNumber": [remote.to_string()],
        "localPortNumber": [local.to_string()],
    })
    .to_string();
    let mut cmd = tokio::process::Command::new("aws");
    cmd.args([
        "ssm",
        "start-session",
        "--target",
        relay,
        "--document-name",
        "AWS-StartPortForwardingSessionToRemoteHost",
        "--parameters",
        &params,
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut guard = ProcessGuard::spawn(cmd)?;
    let stdout = guard.child.stdout.take().expect("piped above");
    let stderr = guard.child.stderr.take().expect("piped above");
    let mut lines = BufReader::new(stdout).lines();
    let bind = async {
        while let Some(line) = lines.next_line().await? {
            // `Port NNNNN opened for sessionId …` (after `Starting
            // session`; before per-conn `Connection accepted`).
            if let Some(rest) = line.strip_prefix("Port ") {
                return rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .with_context(|| format!("unparseable ssm bind line: {line}"));
            }
        }
        Err(anyhow!(
            "session-manager-plugin exited before binding (relay={relay} host={host}:{remote})"
        ))
    };
    let bound = tokio::time::timeout(Duration::from_secs(30), bind)
        .await
        .map_err(|_| anyhow!("ssm start-session did not bind within 30s (relay={relay})"))??;
    // Drain so the plugin never blocks on a full pipe.
    tokio::spawn(async move { while lines.next_line().await.ok().flatten().is_some() {} });
    tokio::spawn(async move {
        let mut errs = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = errs.next_line().await {
            tracing::debug!(target: "ssm", "{l}");
        }
    });
    tracing::debug!(relay, host, remote, local = bound, "ssm tunnel up");
    Ok((bound, guard))
}

#[cfg(test)]
mod tests {
    /// The one plugin stdout shape we depend on.
    #[test]
    fn parse_bind_line() {
        let line = "Port 43425 opened for sessionId jorg-abc";
        let got: u16 = line
            .strip_prefix("Port ")
            .and_then(|r| r.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap();
        assert_eq!(got, 43425);
    }
}
