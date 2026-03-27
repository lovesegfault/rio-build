//! Generate CRD YAML for the Helm chart.
//!
//! `nix build .#crds && ./scripts/split-crds.sh result`
//!
//! serde_yml is the maintained serde_yaml fork (RUSTSEC-2024-0320).
//! Write-only here — serializes our own structs.

use kube::CustomResourceExt;
use rio_controller::crds::builderpool::BuilderPool;
use rio_controller::crds::builderpoolset::BuilderPoolSet;
use rio_controller::crds::fetcherpool::FetcherPool;

fn main() {
    // serde_yml does NOT emit the `---` document separator
    // (verified: output starts with `apiVersion:` directly). The
    // leading `---` is optional per YAML spec but kustomize is
    // stricter with multi-doc files — include it before each doc.
    // No trailing newline after the last doc: serde_yml already
    // ends with one, and kustomize chokes on `---\n\n` empty docs.
    print!("---\n{}", yaml::<BuilderPool>());
    print!("---\n{}", yaml::<BuilderPoolSet>());
    print!("---\n{}", yaml::<FetcherPool>());
}

/// Serialize one CRD to YAML. Generic over the kube-derive-
/// generated struct (BuilderPool, BuilderPoolSet, FetcherPool).
/// Panics on serialize failure — crdgen is a build-time tool; a
/// CRD that can't serialize is a compile-surface bug, not a
/// recoverable runtime condition.
fn yaml<K: CustomResourceExt>() -> String {
    serde_yml::to_string(&K::crd())
        .unwrap_or_else(|e| panic!("{} CRD serialize: {e}", K::crd_name()))
}
