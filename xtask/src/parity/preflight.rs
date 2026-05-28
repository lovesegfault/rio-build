//! Launch pre-flight checks for the nixpkgs-parity campaign.
//!
//! Verifies the deployed rio-gateway/rio-scheduler image tags, the
//! gateway build-policy entries for the campaign tenants, the per-mode
//! tenant upstream sets, and probes the scheduler AdminService for the
//! `QueryDerivationStatuses` RPC the engine prefers for narinfo-presence
//! checks. Pure helpers are split from the I/O so they unit-test
//! without a cluster; the cluster-reading halves run at launch time
//! (operator step), never in CI.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use k8s_openapi::api::apps::v1::Deployment;
use kube::api::Api;

use crate::k8s::client as kclient;

// No non-test users yet: `parity eval` and `parity launch` (landing
// next) drive these checks at launch time. Each `allow(dead_code)`
// comes off with its first user.

/// Upstream URL every substituting campaign tenant must have — and the
/// only one (parity-leaf / parity-warm substitute from exactly
/// cache.nixos.org; parity-selfhosted substitutes from nothing).
#[allow(dead_code)]
pub const CACHE_NIXOS_ORG: &str = crate::k8s::eks::smoke::UPSTREAM_URL;

/// Tag suffix of an image ref, ignoring a registry port
/// (`host:5000/repo:tag` → `tag`, `repo:tag` → `tag`, `repo` → None).
pub fn tag_of_image_ref(image: &str) -> Option<&str> {
    let basename_start = image.rfind('/').map_or(0, |i| i + 1);
    image[basename_start..].rsplit_once(':').map(|(_, t)| t)
}

/// Deployed image tags of the components the campaign depends on.
/// Keys are Deployment names ("rio-gateway", "rio-scheduler").
#[allow(dead_code)]
pub async fn deployed_image_tags(client: &kclient::Client) -> Result<BTreeMap<String, String>> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), crate::k8s::NS);
    let mut out = BTreeMap::new();
    for name in ["rio-gateway", "rio-scheduler"] {
        let dep = api
            .get(name)
            .await
            .with_context(|| format!("read Deployment rio-system/{name}"))?;
        let image = dep
            .spec
            .and_then(|s| s.template.spec)
            .and_then(|p| p.containers.first().and_then(|c| c.image.clone()))
            .with_context(|| format!("Deployment {name} has no container image"))?;
        let tag = tag_of_image_ref(&image)
            .with_context(|| format!("image ref {image:?} has no tag"))?
            .to_owned();
        out.insert(name.to_owned(), tag);
    }
    Ok(out)
}

/// `rio-cli --json upstream list --tenant <t>` output (a JSON array of
/// upstream objects) → the set of upstream URLs.
#[allow(dead_code)]
pub fn upstream_urls(json_out: &str) -> Result<BTreeSet<String>> {
    let v: serde_json::Value =
        serde_json::from_str(json_out.trim()).context("parse `upstream list --json` output")?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| u.get("url").and_then(|s| s.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Assert a tenant's upstream set is exactly `expected`.
#[allow(dead_code)]
pub fn check_upstreams(tenant: &str, got: &BTreeSet<String>, expected: &[&str]) -> Result<()> {
    let want: BTreeSet<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    if *got != want {
        bail!(
            "tenant '{tenant}' upstream set mismatch: got {got:?}, expected {want:?} \
             — fix with `rio-cli upstream add/remove --tenant {tenant} …` (never `k8s grant`)"
        );
    }
    Ok(())
}

/// Assert the deployed gateway build-policy TOML (the `gateway.toml`
/// rendered by the chart's `rio.gatewayToml` named template,
/// infra/helm/rio-build/templates/gateway.yaml) has the expected
/// `[build_policy."<tenant>"]` entry: `keep_going` is always true for
/// campaign tenants; `force_build_roots` is per campaign mode.
/// `policy_toml` is the ConfigMap's `gateway.toml` content.
#[allow(dead_code)]
pub fn check_build_policy(policy_toml: &str, tenant: &str, force_build_roots: bool) -> Result<()> {
    let v: toml::Table = policy_toml.parse().context("parse gateway.toml")?;
    let entry = v
        .get("build_policy")
        .and_then(|bp| bp.get(tenant))
        .with_context(|| {
            format!(
                "deployed gateway.toml has no [build_policy.\"{tenant}\"] entry — \
                 redeploy with parity.enabled=true (`cargo xtask k8s -p eks up --deploy --deploy-parity`)"
            )
        })?;
    let kg = entry.get("keep_going").and_then(toml::Value::as_bool);
    let fbr = entry
        .get("force_build_roots")
        .and_then(toml::Value::as_bool);
    if kg != Some(true) || fbr != Some(force_build_roots) {
        bail!(
            "deployed build-policy for '{tenant}' is {entry} — expected \
             keep_going = true, force_build_roots = {force_build_roots}"
        );
    }
    Ok(())
}

/// Read the deployed gateway.toml from the rio-gateway-config ConfigMap
/// (rendered by the chart's `rio.gatewayToml` named template; present
/// whenever parity.enabled=true or any explicit gateway.buildPolicy
/// entry is set). `None` ⇒ the chart was deployed without any of it.
#[allow(dead_code)]
pub async fn read_build_policy(client: &kclient::Client) -> Result<Option<String>> {
    kclient::get_configmap_key(client, crate::k8s::NS, "rio-gateway-config", "gateway.toml").await
}

/// Whether a gRPC status proves the method exists on the server.
/// `Unimplemented` is the only "definitely absent" answer; anything else
/// (InvalidArgument, Unauthenticated, PermissionDenied, Internal, …)
/// means the method was routed.
pub fn method_present_from(code: tonic::Code) -> bool {
    code != tonic::Code::Unimplemented
}

/// Probe the scheduler AdminService for the `QueryDerivationStatuses`
/// RPC without depending on its (not-yet-generated) message types: send
/// an empty unary request to the full method path and classify the
/// status. `sched_addr` is `host:port` of a port-forwarded scheduler.
#[allow(dead_code)]
pub async fn probe_query_derivation_statuses(sched_addr: &str) -> Result<bool> {
    let channel = tonic::transport::Channel::from_shared(format!("http://{sched_addr}"))
        .context("scheduler address")?
        .connect()
        .await
        .with_context(|| format!("connect to scheduler at {sched_addr}"))?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.context("grpc ready")?;
    // () is a prost::Message that encodes to zero bytes — good enough to
    // make the server route (and then reject) the call.
    let codec: tonic_prost::ProstCodec<(), ()> = tonic_prost::ProstCodec::default();
    let path =
        http::uri::PathAndQuery::from_static("/rio.admin.AdminService/QueryDerivationStatuses");
    match grpc.unary(tonic::Request::new(()), path, codec).await {
        Ok(_) => Ok(true),
        Err(status) => Ok(method_present_from(status.code())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_of_image_ref_handles_registry_ports() {
        assert_eq!(
            tag_of_image_ref("123.dkr.ecr.us-east-2.amazonaws.com/rio-gateway:abc123"),
            Some("abc123")
        );
        assert_eq!(tag_of_image_ref("rio-gateway:dev"), Some("dev"));
        assert_eq!(tag_of_image_ref("localhost:5000/rio-gateway"), None);
        assert_eq!(tag_of_image_ref("rio-gateway"), None);
    }

    #[test]
    fn upstream_urls_and_set_check() {
        let json = r#"[{"url":"https://cache.nixos.org","priority":50,"sig_mode":"keep","trusted_keys":["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="]}]"#;
        let got = upstream_urls(json).unwrap();
        check_upstreams("parity-leaf", &got, &["https://cache.nixos.org"]).unwrap();
        check_upstreams("parity-selfhosted", &got, &[]).unwrap_err();
        let none = upstream_urls("[]").unwrap();
        check_upstreams("parity-selfhosted", &none, &[]).unwrap();
        let err = check_upstreams("parity-warm", &none, &["https://cache.nixos.org"]).unwrap_err();
        assert!(err.to_string().contains("parity-warm"), "{err}");
    }

    #[test]
    fn build_policy_check_matches_helm_defaults() {
        // Exactly what the rio.gatewayToml named template
        // (infra/helm/rio-build/templates/gateway.yaml) renders with
        // parity.enabled=true and no operator overrides.
        let policy = r#"
[build_policy."parity-leaf"]
keep_going = true
force_build_roots = true

[build_policy."parity-selfhosted"]
keep_going = true
force_build_roots = false

[build_policy."parity-warm"]
keep_going = true
force_build_roots = false
"#;
        check_build_policy(policy, "parity-leaf", true).unwrap();
        check_build_policy(policy, "parity-selfhosted", false).unwrap();
        check_build_policy(policy, "parity-warm", false).unwrap();
        // Wrong flag and missing tenant both refuse.
        check_build_policy(policy, "parity-leaf", false).unwrap_err();
        let err = check_build_policy("", "parity-leaf", true).unwrap_err();
        assert!(err.to_string().contains("--deploy-parity"), "{err}");
    }

    #[test]
    fn unimplemented_means_absent_everything_else_means_present() {
        assert!(!method_present_from(tonic::Code::Unimplemented));
        for present in [
            tonic::Code::InvalidArgument,
            tonic::Code::Unauthenticated,
            tonic::Code::PermissionDenied,
            tonic::Code::Internal,
            tonic::Code::Ok,
        ] {
            assert!(method_present_from(present), "{present:?}");
        }
    }
}
