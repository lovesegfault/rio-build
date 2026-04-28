//! Typed binding to Karpenter's `karpenter.sh/v1` NodeClaim CRD.
//!
//! NOT a CRD we own — Karpenter installs the schema. This module exists
//! so the `nodeclaim_pool` reconciler (ADR-023 §13b) can `Api<NodeClaim>`
//! list/watch/create/delete with typed spec/status instead of
//! `DynamicObject` + `serde_json::Value` paths. The `crd()` output is
//! never applied (NOT in `crdgen`); only the kube `Resource` impl and
//! the serde shape matter.
//!
//! Field coverage is the subset the reconciler reads/writes:
//! - **spec**: `nodeClassRef` + `requirements` + `resources.requests`
//!   (what we set when creating a claim to cover a deficit)
//! - **status**: `conditions` (Launched/Registered/Initialized),
//!   `nodeName`, `providerID`, `capacity`/`allocatable` (FFD bin
//!   sizing for in-flight claims)
//!
//! Karpenter spec fields we don't touch (taints, startupTaints,
//! kubelet, expireAfter, …) are absorbed by `preserve-unknown-fields`
//! on round-trip — this struct never `replace()`s a Karpenter-authored
//! claim, only `create()`s its own and `delete()`s by name.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Karpenter NodeClaim spec — the create-side fields the reconciler
/// populates. `JsonSchema` (NOT `KubeSchema`): no CEL — Karpenter's
/// installed CRD owns validation; the schema this derive emits is
/// never installed.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[kube(
    group = "karpenter.sh",
    version = "v1",
    kind = "NodeClaim",
    plural = "nodeclaims",
    status = "NodeClaimStatus",
    derive = "Default"
)]
#[serde(rename_all = "camelCase")]
pub struct NodeClaimSpec {
    /// Pointer to the EC2NodeClass (or other provider NodeClass).
    /// Required by Karpenter's admission — every claim names its
    /// provider config.
    pub node_class_ref: NodeClassRef,

    /// Node-selector requirements (instance-type / arch / capacity-type
    /// constraints). Karpenter extends the core
    /// `NodeSelectorRequirement` shape with an optional `minValues`;
    /// a local struct (not `k8s_openapi::…::NodeSelectorRequirement`)
    /// carries that field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<NodeSelectorRequirementWithMin>,

    /// `resources.requests` — the cores/mem/disk floor the reconciler
    /// asks Karpenter to provision for. `schema_with`: k8s-openapi's
    /// `ResourceRequirements` doesn't impl `JsonSchema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object")]
    pub resources: Option<ResourceRequirements>,
}

/// Karpenter `spec.nodeClassRef`. All three fields are required by
/// Karpenter v1's CRD schema (group was optional in v1beta1; now
/// mandatory).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeClassRef {
    /// Provider API group — `karpenter.k8s.aws` for EC2NodeClass.
    pub group: String,
    /// `EC2NodeClass` (or other provider's kind).
    pub kind: String,
    /// NodeClass object name.
    pub name: String,
}

/// Karpenter's `NodeSelectorRequirement` superset: core k8s
/// key/operator/values plus `minValues` (Karpenter-specific — "at
/// least N distinct values must be available across this key" for
/// instance-type flexibility).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorRequirementWithMin {
    pub key: String,
    /// `In` / `NotIn` / `Exists` / `DoesNotExist` / `Gt` / `Lt`.
    pub operator: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Karpenter extension — minimum distinct instance types matching
    /// this requirement. Absent = 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_values: Option<i32>,
}

/// NodeClaim status — Karpenter writes; the reconciler reads
/// `conditions` for Launched/Registered/Initialized gating and
/// `capacity`/`allocatable` for FFD bin-sizing of in-flight claims
/// (where the backing Node isn't Ready yet so `Node.status.
/// allocatable` is unavailable).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeClaimStatus {
    /// Standard `metav1.Condition` list. Karpenter condition types:
    /// `Launched`, `Registered`, `Initialized`, `Drifted`, `Empty`,
    /// `ConsistentStateFound`. The reconciler keys on
    /// `Registered=True` to flip a claim from "projected capacity"
    /// to "Node.status.allocatable".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "crate::conditions_array")]
    pub conditions: Vec<Condition>,

    /// Backing `Node` name once registered. `None` while in-flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,

    /// Cloud-provider instance ID (`aws:///<zone>/<instance-id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "providerID")]
    pub provider_id: Option<String>,

    /// Resolved instance capacity (cpu/memory/ephemeral-storage/pods).
    /// Karpenter populates this from the chosen instance type at
    /// Launch — available BEFORE the Node registers, which is why FFD
    /// reads it here and not from `Node.status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object")]
    pub capacity: Option<BTreeMap<String, Quantity>>,

    /// `capacity` minus kubelet/system reserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object")]
    pub allocatable: Option<BTreeMap<String, Quantity>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a Karpenter-shaped status through our binding. Guards
    /// the camelCase renames + the `providerID` literal (ID, not Id) —
    /// a serde mis-rename here means the reconciler silently reads
    /// `None` for fields Karpenter populated.
    #[test]
    fn deserialize_karpenter_status() {
        let json = serde_json::json!({
            "conditions": [{
                "type": "Registered",
                "status": "True",
                "lastTransitionTime": "2026-01-01T00:00:00Z",
                "reason": "Registered",
                "message": "",
            }],
            "nodeName": "ip-10-0-1-5.ec2.internal",
            "providerID": "aws:///us-east-1a/i-0123456789abcdef0",
            "capacity": { "cpu": "8", "memory": "32Gi" },
            "allocatable": { "cpu": "7910m", "memory": "31Gi" },
        });
        let s: NodeClaimStatus = serde_json::from_value(json).expect("decodes");
        assert_eq!(s.node_name.as_deref(), Some("ip-10-0-1-5.ec2.internal"));
        assert_eq!(
            s.provider_id.as_deref(),
            Some("aws:///us-east-1a/i-0123456789abcdef0")
        );
        assert_eq!(s.conditions[0].type_, "Registered");
        assert_eq!(s.capacity.as_ref().unwrap()["cpu"], Quantity("8".into()));
    }

    /// Spec serializes with Karpenter's wire field names. The reconciler
    /// `create()`s these — a snake_case leak would be a 422 from the
    /// Karpenter webhook.
    #[test]
    fn serialize_spec_camelcase() {
        let spec = NodeClaimSpec {
            node_class_ref: NodeClassRef {
                group: "karpenter.k8s.aws".into(),
                kind: "EC2NodeClass".into(),
                name: "rio-builder".into(),
            },
            requirements: vec![NodeSelectorRequirementWithMin {
                key: "karpenter.sh/capacity-type".into(),
                operator: "In".into(),
                values: vec!["spot".into()],
                min_values: Some(1),
            }],
            resources: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"nodeClassRef\""));
        assert!(json.contains("\"minValues\":1"));
        assert!(!json.contains("node_class_ref"));
        assert!(!json.contains("min_values"));
    }
}
