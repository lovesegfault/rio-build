//! CustomResourceDefinitions.
//!
//! `#[derive(CustomResource)]` generates the full-object struct
//! (metadata + spec + status) from the annotated Spec struct.
//! `#[kube(...)]` configures group/version/kind/scope and opts in
//! to the status subresource + shortnames + printer columns.
//!
//! CEL validation via `#[x_kube(validation = "...")]` — kube-rs's
//! idiomatic way to inject `x-kubernetes-validations` into the
//! generated OpenAPI schema. NOT schemars extend (that's the raw
//! fallback; x_kube is the first-class path).

pub mod build;
pub mod workerpool;

// ----- schemars helpers for k8s-openapi passthrough types -----------
//
// k8s-openapi types (ResourceRequirements, Toleration, Condition)
// don't impl JsonSchema. Previous approach was `#[schemars(with =
// "serde_json::Value")]` — but that emits an EMPTY schema `{}`, which
// the K8s apiserver REJECTS:
//   "Required value: must not be empty for specified object fields"
//
// These schema_with fns emit the minimum K8s accepts: `type: object`
// + `x-kubernetes-preserve-unknown-fields: true`. The apiserver
// validates against its OWN schema for well-known types (it knows
// what ResourceRequirements looks like); we just need to tell it
// "this is an object, don't strip unknown fields."
//
// `pub(crate)`: used across build.rs and workerpool.rs via
// `#[schemars(schema_with = "crate::crds::any_object")]`.

/// Schema for `Option<K8sType>` fields where K8sType is a single
/// object (ResourceRequirements, etc). nullable + object +
/// preserve-unknown.
pub(crate) fn any_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "nullable": true,
        "x-kubernetes-preserve-unknown-fields": true,
    })
}

/// Schema for `Vec<K8sType>` or `Option<Vec<K8sType>>` fields
/// (tolerations, conditions). Array of preserve-unknown objects.
/// K8s REQUIRES `items.type` — the earlier empty-schema emitted
/// `items: {}` which fails CRD validation.
pub(crate) fn any_object_array(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "nullable": true,
        "items": {
            "type": "object",
            "x-kubernetes-preserve-unknown-fields": true,
        },
    })
}
