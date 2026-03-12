//! Generate CRD YAML for kustomize.
//!
//! `cargo run --bin crdgen > infra/k8s/base/crds.yaml`
//!
//! Two documents separated by `---`. serde_yml doesn't have a
//! multi-document writer so we concat manually. That's fine —
//! `---` on its own line is the YAML document separator.
//!
//! serde_yml is the maintained serde_yaml fork (RUSTSEC-2024-0320).
//! Write-only here — serializes our own structs.

use kube::CustomResourceExt;

fn main() {
    let workerpool =
        serde_yml::to_string(&rio_controller::WorkerPool::crd()).expect("WorkerPool serializes");
    let build = serde_yml::to_string(&rio_controller::Build::crd()).expect("Build serializes");

    // serde_yml does NOT emit the `---` document separator
    // (verified: output starts with `apiVersion:` directly).
    // Concat with explicit separator. The leading `---` on the
    // first doc is optional per YAML spec but kustomize is
    // stricter with multi-doc files — include it.
    print!("---\n{workerpool}---\n{build}");
}
