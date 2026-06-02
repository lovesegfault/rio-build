//! Launch pre-flight checks for the build-replay campaign.
//!
//! Verifies the deployed rio-gateway/rio-scheduler image tags, the
//! gateway build-policy entries for the campaign tenants, the per-mode
//! tenant upstream sets, and the CiliumNetworkPolicy admissions for the
//! campaign engine (the same artifact read-back `replay setup` runs
//! after enabling). Pure helpers are split from the I/O so they
//! unit-test without a cluster; the cluster-reading halves run at
//! setup/launch/repro time (operator step), never in CI.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use k8s_openapi::api::apps::v1::Deployment;
use kube::api::Api;

use super::NS_REPLAY;
use crate::k8s::client as kclient;

/// Upstream URL every substituting campaign tenant must have — and the
/// only one (replay-leaf / replay-warm substitute from exactly
/// cache.nixos.org; replay-selfhosted substitutes from nothing).
pub const CACHE_NIXOS_ORG: &str = crate::k8s::eks::smoke::UPSTREAM_URL;

/// Tag suffix of an image ref, ignoring a registry port
/// (`host:5000/repo:tag` → `tag`, `repo:tag` → `tag`, `repo` → None).
pub fn tag_of_image_ref(image: &str) -> Option<&str> {
    let basename_start = image.rfind('/').map_or(0, |i| i + 1);
    image[basename_start..].rsplit_once(':').map(|(_, t)| t)
}

/// Deployed image tags of the components the campaign depends on.
/// Keys are Deployment names ("rio-gateway", "rio-scheduler").
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
/// upstream objects) → the set of upstream URLs. Refuses output whose
/// top level is not an array or whose entries carry no `url` string: a
/// pre-flight parser must never silently degrade to an empty set (that
/// would pass the replay-selfhosted "no upstreams" check on garbage).
pub fn upstream_urls(json_out: &str) -> Result<BTreeSet<String>> {
    let v: serde_json::Value =
        serde_json::from_str(json_out.trim()).context("parse `upstream list --json` output")?;
    let entries = v
        .as_array()
        .with_context(|| format!("`upstream list --json` top level is not an array: {v}"))?;
    entries
        .iter()
        .map(|u| {
            u.get("url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .with_context(|| format!("`upstream list --json` entry has no `url` string: {u}"))
        })
        .collect()
}

/// Assert a tenant's upstream set is exactly `expected`.
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
pub fn check_build_policy(policy_toml: &str, tenant: &str, force_build_roots: bool) -> Result<()> {
    let v: toml::Table = policy_toml.parse().context(
        "parse gateway.toml from the rio-system/rio-gateway-config ConfigMap (key `gateway.toml`)",
    )?;
    let entry = v
        .get("build_policy")
        .and_then(|bp| bp.get(tenant))
        .with_context(|| {
            format!(
                "deployed gateway.toml has no [build_policy.\"{tenant}\"] entry — \
                 enable replay on the release (`cargo xtask replay setup`)"
            )
        })?;
    let kg = entry.get("keep_going").and_then(toml::Value::as_bool);
    let fbr = entry
        .get("force_build_roots")
        .and_then(toml::Value::as_bool);
    if kg != Some(true) || fbr != Some(force_build_roots) {
        bail!(
            "deployed build-policy for '{tenant}' is {entry} — expected \
             keep_going = true, force_build_roots = {force_build_roots}; \
             remove (or fix) any conflicting gateway.buildPolicy values override and \
             re-run `cargo xtask replay setup` (then `cargo xtask k8s -p eks up --deploy` \
             to re-render the chart if an override came from the service deploy)"
        );
    }
    Ok(())
}

/// Assert one deployed component's image tag matches `want` (the tag
/// xtask computes for the current tree — the same tag the rio-replay
/// engine image is pushed and pulled as). Pure so the message stays
/// unit-tested; the launch pre-flight calls it for rio-gateway and
/// rio-scheduler.
pub fn check_image_tag(component: &str, got: &str, want: &str) -> Result<()> {
    if got == want {
        return Ok(());
    }
    bail!(
        "deployed {component} image tag is '{got}' but the current tree builds '{want}' — \
         push + redeploy this tree (`cargo xtask k8s -p eks up --push --deploy`; replay \
         enablement is preserved across service deploys) or check out the deployed revision \
         before launching, so the campaign records the component versions it actually ran against"
    )
}

/// Read the deployed gateway.toml from the rio-gateway-config ConfigMap
/// (rendered by the chart's `rio.gatewayToml` named template; present
/// whenever replay.enabled=true or any explicit gateway.buildPolicy
/// entry is set). `None` ⇒ the chart was deployed without any of it.
pub async fn read_build_policy(client: &kclient::Client) -> Result<Option<String>> {
    kclient::get_configmap_key(client, crate::k8s::NS, "rio-gateway-config", "gateway.toml").await
}

/// Whether one CiliumNetworkPolicy admits the campaign engine ON THE
/// SERVICE'S gRPC PORT: some ingress rule whose `toPorts` contains
/// `grpc_port` carries a `fromEndpoints` entry matching the replay
/// namespace plus the `rio-replay` name label — exactly what the chart
/// renders under `replay.enabled=true`
/// (infra/helm/rio-build/templates/networkpolicy.yaml; helm/27 pins the
/// rendered pairing). Rules are selected by port content first: an
/// admission entry parked on some other rule (e.g. the metrics one)
/// admits nothing the engine dials, so it must NOT satisfy this
/// predicate — Cilium would still drop every gRPC call. Pure so the
/// predicate is unit-testable against chart-shaped JSON.
fn cnp_admits_replay(cnp: &serde_json::Value, grpc_port: &str) -> bool {
    cnp.pointer("/spec/ingress")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter(|rule| {
            rule.get("toPorts")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|tp| tp.get("ports").and_then(|v| v.as_array()))
                .flatten()
                .any(|p| p.get("port").and_then(|v| v.as_str()) == Some(grpc_port))
        })
        .filter_map(|rule| rule.get("fromEndpoints").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|ep| ep.get("matchLabels").and_then(|v| v.as_object()))
        .any(|labels| {
            labels
                .get("k8s:io.kubernetes.pod.namespace")
                .and_then(|v| v.as_str())
                == Some(NS_REPLAY)
                && labels
                    .get("k8s:app.kubernetes.io/name")
                    .and_then(|v| v.as_str())
                    == Some("rio-replay")
        })
}

/// Read back both component-ingress CiliumNetworkPolicies and assert each
/// admits the campaign engine on the port the engine dials
/// ([`crate::k8s::SCHEDULER_GRPC_PORT`] / [`crate::k8s::STORE_GRPC_PORT`]
/// — the same constants the campaign-spec addresses are built from). The
/// chart renders the admissions into scheduler-ingress (rio-system) and
/// store-ingress (rio-store); a miss on either means replay traffic is
/// silently dropped.
///
/// This verifies the deployed ARTIFACT, not the release values: the chart
/// templates the admission namespace from the `replay.namespace` value,
/// which has to equal the hardcoded namespace xtask creates the engine
/// Jobs in ([`NS_REPLAY`]) — a raw-helm override of that value (or any
/// out-of-band CNP edit, or a divergent older deployment on setup's
/// already-enabled no-upgrade path) renders policies that admit nothing
/// the engine runs as. Reading the CNPs back catches every one of those
/// before a campaign Job is created — `replay setup` (verify step),
/// `replay launch` (pre-flight), and `replay repro` all run it. Both
/// policies are checked before reporting so the operator gets the
/// complete miss list at once.
pub async fn verify_cnp_admissions(client: &kclient::Client) -> Result<()> {
    use kube::core::{ApiResource, DynamicObject, GroupVersionKind};

    use crate::k8s::{NS, NS_STORE, SCHEDULER_GRPC_PORT, STORE_GRPC_PORT};

    let gvk = GroupVersionKind::gvk("cilium.io", "v2", "CiliumNetworkPolicy");
    let ar = ApiResource::from_gvk(&gvk);
    let mut fetched: Vec<FetchedCnp<'_>> = Vec::new();
    for (ns, name, port) in [
        (NS, "scheduler-ingress", SCHEDULER_GRPC_PORT),
        (NS_STORE, "store-ingress", STORE_GRPC_PORT),
    ] {
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
        let cnp = api
            .get_opt(name)
            .await
            .with_context(|| format!("read CiliumNetworkPolicy {ns}/{name}"))?;
        fetched.push((ns, name, port, cnp.map(|c| c.data)));
    }
    check_cnp_admissions(&fetched)
}

/// One CNP read-back target as fetched: (policy namespace, policy name,
/// gRPC port the engine dials it on, the policy object — `None` ⇒ the
/// policy does not exist).
type FetchedCnp<'a> = (&'a str, &'a str, &'a str, Option<serde_json::Value>);

/// Decision half of [`verify_cnp_admissions`], split from the fetch so
/// the refusal contract stays unit-testable without a cluster: every
/// target policy is checked and ALL misses are reported in one error,
/// each naming the policy and the missing admission
/// (namespace + label + port), followed by the silent-drop consequence
/// and the remediation (`replay.namespace` must equal [`NS_REPLAY`];
/// re-run `replay setup`).
fn check_cnp_admissions(fetched: &[FetchedCnp<'_>]) -> Result<()> {
    let mut misses: Vec<String> = Vec::new();
    for (ns, name, port, cnp) in fetched {
        match cnp {
            None => misses.push(format!(
                "CiliumNetworkPolicy {ns}/{name} not found — the deployed chart did not render \
                 its network policies"
            )),
            Some(cnp) if !cnp_admits_replay(cnp, port) => misses.push(format!(
                "CiliumNetworkPolicy {ns}/{name} has no ingress admission for the campaign \
                 engine (namespace {NS_REPLAY}, label rio-replay) on gRPC port {port}"
            )),
            Some(_) => {}
        }
    }
    ensure!(
        misses.is_empty(),
        "{} — Cilium would silently drop the engine's gRPC traffic; check `helm get values {} \
         -n {}` for replay.enabled and for a replay.namespace override (it must equal \
         {NS_REPLAY:?}, the namespace the engine Jobs run in), then re-run \
         `cargo xtask replay setup`",
        misses.join("; "),
        super::setup::RELEASE,
        crate::k8s::NS,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
        check_upstreams("replay-leaf", &got, &["https://cache.nixos.org"]).unwrap();
        check_upstreams("replay-selfhosted", &got, &[]).unwrap_err();
        let none = upstream_urls("[]").unwrap();
        check_upstreams("replay-selfhosted", &none, &[]).unwrap();
        let err = check_upstreams("replay-warm", &none, &["https://cache.nixos.org"]).unwrap_err();
        assert!(err.to_string().contains("replay-warm"), "{err}");
    }

    #[test]
    fn upstream_urls_refuse_unexpected_shapes() {
        // A pre-flight parser must never silently degrade to an empty
        // set: a non-array top level (e.g. a future envelope object) and
        // an entry without a `url` string both refuse instead of
        // vanishing — an empty set would wrongly pass the
        // replay-selfhosted "no upstreams" check.
        let err = upstream_urls(r#"{"upstreams":[]}"#).unwrap_err();
        assert!(format!("{err:#}").contains("not an array"), "{err:#}");
        let err = upstream_urls(r#"[{"priority":50}]"#).unwrap_err();
        assert!(format!("{err:#}").contains("no `url`"), "{err:#}");
        assert!(upstream_urls("not json").is_err());
    }

    #[test]
    fn build_policy_check_matches_helm_defaults() {
        // Exactly what the rio.gatewayToml named template
        // (infra/helm/rio-build/templates/gateway.yaml) renders with
        // replay.enabled=true and no operator overrides.
        let policy = r#"
[build_policy."replay-leaf"]
keep_going = true
force_build_roots = true

[build_policy."replay-selfhosted"]
keep_going = true
force_build_roots = false

[build_policy."replay-warm"]
keep_going = true
force_build_roots = false
"#;
        // Expectations come from the shared tenant matrix — the same table
        // `replay launch` provisions from and pre-flights against — so the
        // chart fixture above and the launch path cannot drift apart
        // silently.
        for (tenant, _, force_build_roots) in super::super::TENANT_MATRIX {
            check_build_policy(policy, tenant, force_build_roots).unwrap();
        }
        // Wrong flag and missing tenant both refuse, and both name the
        // fix (values override / `replay setup`).
        let err = check_build_policy(policy, "replay-leaf", false).unwrap_err();
        for needle in ["gateway.buildPolicy", "cargo xtask replay setup"] {
            assert!(err.to_string().contains(needle), "{err}");
        }
        let err = check_build_policy("", "replay-leaf", true).unwrap_err();
        assert!(
            err.to_string().contains("cargo xtask replay setup"),
            "{err}"
        );
        // A parse failure names where the TOML came from.
        let err = check_build_policy("not = [valid", "replay-leaf", true).unwrap_err();
        assert!(format!("{err:#}").contains("rio-gateway-config"), "{err:#}");
    }

    #[test]
    fn image_tag_check_names_component_and_both_tags() {
        check_image_tag("rio-gateway", "abc123", "abc123").unwrap();
        let err = check_image_tag("rio-scheduler", "abc123", "def456")
            .unwrap_err()
            .to_string();
        // The skew error names the component, both tags, and the fix
        // (push + redeploy; replay enablement survives that deploy).
        for needle in ["rio-scheduler", "abc123", "def456", "--push --deploy"] {
            assert!(err.contains(needle), "{err}");
        }
    }

    /// `spec` of the scheduler-ingress CNP, captured verbatim from the
    /// producer: `helm template rio . --set global.image.tag=test --set
    /// replay.enabled=true` (yq'd to JSON) — terminators, sibling
    /// entries, metrics rule and all. Hand-trimming the fixture is how a
    /// predicate quietly ends up tested against a shape the chart never
    /// renders.
    fn scheduler_ingress_rendered() -> serde_json::Value {
        json!({
            "endpointSelector": {"matchLabels": {"app.kubernetes.io/name": "rio-scheduler"}},
            "ingress": [
                {
                    "fromEndpoints": [
                        {"matchLabels": {"app.kubernetes.io/name": "rio-gateway"}},
                        {"matchLabels": {"app.kubernetes.io/name": "rio-controller"}},
                        {"matchLabels": {"gateway.networking.k8s.io/gateway-name": "rio-dashboard"}},
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-builder"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-fetcher"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {
                                "k8s:io.kubernetes.pod.namespace": "rio-replay",
                                "k8s:app.kubernetes.io/name": "rio-replay"
                            }
                        }
                    ],
                    "toPorts": [{"ports": [{"port": "9001", "protocol": "TCP"}]}]
                },
                {
                    "fromEntities": ["host", "remote-node", "cluster"],
                    "toPorts": [{"ports": [{"port": "9091", "protocol": "TCP"}]}]
                }
            ]
        })
    }

    /// `spec` of the store-ingress CNP from the same render.
    fn store_ingress_rendered() -> serde_json::Value {
        json!({
            "endpointSelector": {"matchLabels": {"app.kubernetes.io/name": "rio-store"}},
            "ingress": [
                {
                    "fromEndpoints": [
                        {"matchLabels": {"k8s:io.kubernetes.pod.namespace": "rio-system"}},
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-builder"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {"app.kubernetes.io/component": "rio-fetcher"},
                            "matchExpressions": [
                                {"key": "k8s:io.kubernetes.pod.namespace", "operator": "Exists"}
                            ]
                        },
                        {
                            "matchLabels": {
                                "k8s:io.kubernetes.pod.namespace": "rio-replay",
                                "k8s:app.kubernetes.io/name": "rio-replay"
                            }
                        }
                    ],
                    "toPorts": [{"ports": [{"port": "9002", "protocol": "TCP"}]}]
                },
                {
                    "fromEntities": ["host", "remote-node", "cluster"],
                    "toPorts": [{"ports": [{"port": "9092", "protocol": "TCP"}]}]
                }
            ]
        })
    }

    #[test]
    fn cnp_admission_predicate_matches_chart_render() {
        // Both rendered policies admit the engine on their gRPC port —
        // the predicate must find the entry among the sibling admissions.
        let scheduler = json!({"spec": scheduler_ingress_rendered()});
        let store = json!({"spec": store_ingress_rendered()});
        assert!(cnp_admits_replay(&scheduler, "9001"));
        assert!(cnp_admits_replay(&store, "9002"));

        // Wiring the verifier to the wrong service's port must fail even
        // against a fully correct render: the admission exists, but not
        // on the demanded port.
        assert!(!cnp_admits_replay(&scheduler, "9002"));
        assert!(!cnp_admits_replay(&store, "9001"));
    }

    #[test]
    fn cnp_admission_rejects_admission_on_non_grpc_rule() {
        // The discriminating case for port scoping: the rio-replay
        // fromEndpoints entry exists, but parked on the METRICS rule
        // (port 9091) while the gRPC rule (9001) carries only the
        // always-present admissions. A labels-only scan that flattens
        // every rule's fromEndpoints accepts this policy — yet Cilium
        // drops all engine gRPC, the exact silent failure the read-back
        // exists to catch (e.g. a divergent older deployment on setup's
        // idempotent no-upgrade path).
        let mut mis_paired = scheduler_ingress_rendered();
        let rules = mis_paired["ingress"].as_array_mut().unwrap();
        let admission = rules[0]["fromEndpoints"]
            .as_array_mut()
            .unwrap()
            .pop()
            .unwrap();
        rules[1]["fromEndpoints"] = json!([admission]);
        let mis_paired = json!({"spec": mis_paired});
        assert!(
            !cnp_admits_replay(&mis_paired, "9001"),
            "an admission on a non-gRPC rule admits nothing the engine dials"
        );

        // Same shape with the admission in place still passes — proves
        // the rejection above is the port pairing, not fixture damage.
        assert!(cnp_admits_replay(
            &json!({"spec": scheduler_ingress_rendered()}),
            "9001"
        ));
    }

    #[test]
    fn cnp_admission_predicate_rejects_weaker_shapes() {
        // replay.enabled=false: no admission entry anywhere.
        let disabled = json!({
            "spec": {
                "ingress": [
                    {
                        "fromEndpoints": [
                            {"matchLabels": {"k8s:io.kubernetes.pod.namespace": "rio-system"}}
                        ],
                        "toPorts": [{"ports": [{"port": "9001", "protocol": "TCP"}]}]
                    }
                ]
            }
        });
        assert!(!cnp_admits_replay(&disabled, "9001"));

        // The namespace label alone is not enough — the chart's admission
        // pairs it with the engine's name label, and the predicate must
        // demand both (a namespace-wide hole would be a policy bug). The
        // rule carries the right port so the rejection below is about
        // the labels, not the port filter.
        let ns_only = json!({
            "spec": {
                "ingress": [
                    {
                        "fromEndpoints": [
                            {"matchLabels": {"k8s:io.kubernetes.pod.namespace": "rio-replay"}}
                        ],
                        "toPorts": [{"ports": [{"port": "9001", "protocol": "TCP"}]}]
                    }
                ]
            }
        });
        assert!(!cnp_admits_replay(&ns_only, "9001"));

        // Degenerate shapes never panic and never pass.
        assert!(!cnp_admits_replay(&json!({}), "9001"));
        assert!(!cnp_admits_replay(
            &json!({"spec": {"ingress": []}}),
            "9001"
        ));
    }

    #[test]
    fn cnp_verification_reports_every_miss_with_the_remediation() {
        let scheduler = json!({"spec": scheduler_ingress_rendered()});
        let store = json!({"spec": store_ingress_rendered()});

        // The healthy render passes.
        check_cnp_admissions(&[
            ("rio-system", "scheduler-ingress", "9001", Some(scheduler)),
            ("rio-store", "store-ingress", "9002", Some(store.clone())),
        ])
        .unwrap();

        // A missing policy refuses, naming the policy and the operator
        // contract: the silent-drop consequence, the replay.namespace
        // pin, and the re-setup remediation. This is the refusal
        // setup/launch/repro surface before any engine Job exists.
        let err = check_cnp_admissions(&[
            ("rio-system", "scheduler-ingress", "9001", None),
            ("rio-store", "store-ingress", "9002", Some(store.clone())),
        ])
        .unwrap_err()
        .to_string();
        for needle in [
            "rio-system/scheduler-ingress",
            "not found",
            "silently drop",
            "replay.namespace",
            "\"rio-replay\"",
            "cargo xtask replay setup",
        ] {
            assert!(err.contains(needle), "missing {needle:?} in: {err}");
        }
        assert!(
            !err.contains("rio-store/store-ingress"),
            "the healthy policy must not be reported: {err}"
        );

        // replay.namespace drift: the same verbatim render with the
        // admission namespace swapped — exactly what a raw-helm
        // `--set replay.namespace=…` override renders. The policy
        // exists, so the miss is the no-admission shape naming the
        // namespace+label pair and the port the engine dials.
        let mut drifted = scheduler_ingress_rendered();
        drifted["ingress"][0]["fromEndpoints"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["matchLabels"]["k8s:io.kubernetes.pod.namespace"] = json!("replay-two");
        let err = check_cnp_admissions(&[
            (
                "rio-system",
                "scheduler-ingress",
                "9001",
                Some(json!({"spec": drifted})),
            ),
            ("rio-store", "store-ingress", "9002", Some(store)),
        ])
        .unwrap_err()
        .to_string();
        for needle in [
            "rio-system/scheduler-ingress",
            "no ingress admission",
            "namespace rio-replay",
            "port 9001",
        ] {
            assert!(err.contains(needle), "missing {needle:?} in: {err}");
        }

        // Both targets broken: the complete miss list in ONE error, so
        // the operator gets the full fix list at once.
        let err = check_cnp_admissions(&[
            ("rio-system", "scheduler-ingress", "9001", None),
            ("rio-store", "store-ingress", "9002", None),
        ])
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("rio-system/scheduler-ingress") && err.contains("rio-store/store-ingress"),
            "{err}"
        );
    }
}
