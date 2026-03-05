//! Generate CRD YAML for kustomize.
//!
//! `cargo run --bin crdgen > deploy/base/crds.yaml`
//!
//! Two documents separated by `---`. serde_yaml doesn't have a
//! multi-document writer so we concat manually. That's fine —
//! `---` on its own line is the YAML document separator.
//!
//! serde_yaml is deprecated (RUSTSEC-2024-0320) but this is
//! WRITE-ONLY: we serialize our own structs, no untrusted
//! deserialization = no attack surface. See deny.toml.

use kube::CustomResourceExt;

fn main() {
    let workerpool =
        serde_yaml::to_string(&rio_controller::WorkerPool::crd()).expect("WorkerPool serializes");
    let build = serde_yaml::to_string(&rio_controller::Build::crd()).expect("Build serializes");

    // serde_yaml does NOT emit the `---` document separator
    // (verified: output starts with `apiVersion:` directly).
    // Concat with explicit separator. The leading `---` on the
    // first doc is optional per YAML spec but kustomize is
    // stricter with multi-doc files — include it.
    print!("---\n{workerpool}---\n{build}");
}
