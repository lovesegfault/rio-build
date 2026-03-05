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
