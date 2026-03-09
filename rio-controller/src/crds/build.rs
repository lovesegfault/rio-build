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

    /// Tenant identifier for multi-tenancy. Passed through to the
    /// scheduler; not yet enforced. String not UUID — tenant naming
    /// is an operator concern.
    // TODO(phase4): enforcement (scheduler-side tenant isolation).
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
    #[serde(default)]
    pub progress: String,

    /// Derivation counts. Individual fields so dashboards can
    /// `sum(.status.totalDerivations) across builds` etc.
    #[serde(default)]
    pub total_derivations: i32,
    #[serde(default)]
    pub completed_derivations: i32,
    #[serde(default)]
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
    #[serde(default)]
    pub last_sequence: i64,
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
}
