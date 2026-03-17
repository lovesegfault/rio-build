//! Build CRD: K8s-native build submission.
//!
//! ALTERNATIVE to SSH-initiated builds (which go through the
//! gateway → scheduler directly, no CRD). Use this for K8s-
//! native workflows that want `kubectl apply -f build.yaml`
//! or programmatic submission via the K8s API.
//!
//! SSH builds do NOT require a Build CRD. The CRD is an
//! optional visibility/tracking layer.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "Build",
    namespaced,
    status = "BuildStatus",
    shortname = "rb",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Progress","type":"string","jsonPath":".status.progress"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BuildSpec {
    /// Store path to the .drv file (e.g.,
    /// `/nix/store/abc...-hello.drv`). MUST be a store path —
    /// evaluation is external (ADR-002). The .drv must already
    /// be in rio-store (upload via `nix copy --to ssh-ng://`
    /// first).
    ///
    /// CEL: must start with /nix/store/ AND end with .drv.
    /// Catches the two common mistakes: passing an attr path
    /// (`nixpkgs#hello`) instead of a drv path, or forgetting
    /// the .drv suffix (passing an output path).
    #[x_kube(validation = "self.startsWith('/nix/store/') && self.endsWith('.drv')")]
    pub derivation: String,

    /// Inter-build priority. Higher = sooner. Maps to the
    /// scheduler's `PriorityClass` — the reconciler translates.
    /// Default 0 = `Scheduled` (lowest).
    ///
    /// The scheduler also has INTRA-build priority (critical
    /// path within a DAG) — that's automatic, not configured
    /// here.
    #[serde(default)]
    pub priority: i32,

    /// Build timeout in seconds. The scheduler's
    /// `BuildOptions.build_timeout`. 0 = no timeout (builds
    /// run until complete or nix-daemon's own 2h default).
    ///
    /// CEL: non-negative. Negative timeout is meaningless.
    #[x_kube(validation = "self >= 0")]
    #[serde(default)]
    pub timeout_seconds: i64,

    /// Tenant name for multi-tenancy. Passed through to the scheduler
    /// which resolves it to a UUID via the `tenants` table. Unknown
    /// name → build rejected with `InvalidArgument`. String not UUID —
    /// the CRD uses the human-readable tenant name (what operators know).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildStatus {
    /// High-level state. Pending / Building / Succeeded /
    /// Failed / Cancelled. String not enum: same "unknown
    /// value is a reconcile error, not a schema rejection"
    /// policy as Autoscaling.metric. Future phases (e.g.,
    /// "Evaluating" set by external eval orchestrators) don't
    /// need a CRD version bump.
    #[serde(default)]
    pub phase: String,

    /// Scheduler's build_id (UUID string). Set after
    /// SubmitBuild returns. The finalizer uses this to call
    /// CancelBuild. Empty until submitted.
    #[serde(default)]
    pub build_id: String,

    /// "31/47" — completed/total derivations. Derived from
    /// scheduler's BuildEvent stream. String for the printer
    /// column (which only does simple jsonPath, no formatting).
    ///
    /// `skip_serializing_if`: Merge-patch callers that only set
    /// `phase`+`lastSequence` must NOT stomp this to "". With
    /// SSA + `.force()`, a serialized `""` claims ownership and
    /// wipes the real value. Omitting the field leaves the
    /// apiserver's copy intact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,

    /// Derivation counts. Individual fields so dashboards can
    /// `sum(.status.totalDerivations) across builds` etc.
    ///
    /// `skip_serializing_if`: same preservation rationale as
    /// `progress`. The `Unknown`-phase patch in drain_stream's
    /// exhaustion path was zeroing these — `kubectl get build`
    /// went from "5/10" to nothing at reconnect-exhaustion.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub total_derivations: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub completed_derivations: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub cached_derivations: i32,

    /// When the build actually started (scheduler's first
    /// dispatch). Not `.metadata.creationTimestamp` — that's
    /// when the CRD was applied, which may be earlier (queued
    /// behind other builds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub started_at: Option<Time>,

    /// Standard K8s Conditions. Scheduled / InputsResolved /
    /// Building / Succeeded / Failed. Each with
    /// lastTransitionTime + reason + message. Succeeded and
    /// Failed are terminal and mutually exclusive.
    #[serde(default)]
    #[schemars(schema_with = "crate::crds::any_object_array")]
    pub conditions: Vec<Condition>,

    /// Last BuildEvent sequence number seen by drain_stream.
    ///
    /// Used for WatchBuild reconnect: on controller restart, the
    /// idempotence gate in apply() sees build_id non-empty AND
    /// phase non-terminal, then calls WatchBuild with this as
    /// since_sequence. Scheduler replays from build_event_log
    /// (persisted) for events past this seq. Without it, a
    /// controller restart means status frozen until build
    /// terminates.
    ///
    /// i64 because sequence is u64 in proto but K8s JSON schema
    /// doesn't do uint64 natively. Cast at use; seq=0 means
    /// "replay all" which is the safe default.
    ///
    /// NO `skip_serializing_if`: seq=0 IS meaningful ("replay
    /// all events") and must survive a round-trip. Omitting it
    /// on Merge-patch would leave a stale nonzero seq intact →
    /// WatchBuild skips events it should replay.
    #[serde(default)]
    pub last_sequence: i64,
}

/// Predicate for `skip_serializing_if`. Free fn (not closure)
/// because serde's attribute wants a path, not an expression.
fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn crd_serializes() {
        let crd = Build::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: Build"));
        assert!(yaml.contains("rb"));
    }

    #[test]
    fn cel_rules_in_schema() {
        let crd = Build::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(
            json.contains("self.startsWith('/nix/store/')"),
            "derivation path CEL rule missing"
        );
        assert!(json.contains("self >= 0"), "timeout CEL rule missing");
    }

    /// T1: `skip_serializing_if` omits zero counts + empty progress
    /// but KEEPS `last_sequence: 0`. Without this, a Merge-patch
    /// that intended to set only `phase` would also stomp counts/
    /// progress to their defaults (SSA claims ownership of every
    /// field present in the patch body).
    #[test]
    fn status_serde_skips_zeros_but_keeps_last_sequence() {
        // Default status: everything zero/empty.
        let status = BuildStatus::default();
        let json = serde_json::to_value(&status).expect("serializes");
        let obj = json.as_object().expect("is object");

        // Zero-valued counts omitted.
        assert!(
            !obj.contains_key("totalDerivations"),
            "zero total_derivations should be omitted"
        );
        assert!(
            !obj.contains_key("completedDerivations"),
            "zero completed_derivations should be omitted"
        );
        assert!(
            !obj.contains_key("cachedDerivations"),
            "zero cached_derivations should be omitted"
        );
        // Empty progress omitted.
        assert!(
            !obj.contains_key("progress"),
            "empty progress should be omitted"
        );
        // last_sequence=0 IS serialized (it means "replay all").
        assert_eq!(
            obj.get("lastSequence"),
            Some(&serde_json::json!(0)),
            "last_sequence=0 must be explicitly serialized"
        );

        // Nonzero values DO serialize.
        let status = BuildStatus {
            total_derivations: 10,
            completed_derivations: 5,
            progress: "5/10".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&status).expect("serializes");
        let obj = json.as_object().expect("is object");
        assert_eq!(obj.get("totalDerivations"), Some(&serde_json::json!(10)));
        assert_eq!(obj.get("completedDerivations"), Some(&serde_json::json!(5)));
        assert_eq!(obj.get("progress"), Some(&serde_json::json!("5/10")));
        // Round-trip: deserialize back, fields preserved.
        let roundtrip: BuildStatus = serde_json::from_value(json).expect("deserializes");
        assert_eq!(roundtrip.total_derivations, 10);
        assert_eq!(roundtrip.completed_derivations, 5);
        assert_eq!(roundtrip.progress, "5/10");
    }
}
