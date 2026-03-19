//! Generate CRD YAML for the Helm chart.
//!
//! `nix build .#crds && ./scripts/split-crds.sh result`
//!
//! serde_yml is the maintained serde_yaml fork (RUSTSEC-2024-0320).
//! Write-only here — serializes our own structs.

use kube::CustomResourceExt;

fn main() {
    let workerpool =
        serde_yml::to_string(&rio_controller::WorkerPool::crd()).expect("WorkerPool serializes");

    // serde_yml does NOT emit the `---` document separator
    // (verified: output starts with `apiVersion:` directly). The
    // leading `---` is optional per YAML spec but kustomize is
    // stricter with multi-doc files — include it.
    print!("---\n{workerpool}");
}
