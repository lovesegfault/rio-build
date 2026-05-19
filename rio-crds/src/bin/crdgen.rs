//! Generate CRD YAML for the Helm chart.
//!
//! `cargo xtask regen crds`
//!
//! Writes one `<crd-name>.yaml` per CRD into the directory given as the
//! sole argument. This is the single serialization path for the
//! committed `infra/helm/crds/` files: `cargo xtask regen crds`
//! produces them and the `crds-drift` flake check rebuilds them
//! hermetically and `diff -r`s. Both run this binary, so the bytes
//! match by construction — there is no second encoder to drift.
//!
//! serde_yml is the maintained serde_yaml fork (RUSTSEC-2024-0320).
//! Write-only here — serializes our own structs.

use std::path::Path;

use kube::CustomResourceExt;
use rio_crds::componentscaler::ComponentScaler;
use rio_crds::pool::Pool;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: crdgen <out-dir>");
        std::process::exit(2);
    });
    let out = Path::new(&out);
    write::<Pool>(out);
    write::<ComponentScaler>(out);
}

/// Serialize one CRD to `<out>/<crd-name>.yaml`. Generic over the
/// kube-derive-generated struct (Pool, ComponentScaler).
/// `crd_name()` is `<plural>.<group>` — same value as `metadata.name`,
/// which is what the per-CRD files have always been keyed on.
///
/// Panics on serialize/IO failure — crdgen is a build-time tool; a
/// CRD that can't serialize is a compile-surface bug, not a
/// recoverable runtime condition.
fn write<K: CustomResourceExt>(out: &Path) {
    let yaml = serde_yml::to_string(&K::crd())
        .unwrap_or_else(|e| panic!("{} CRD serialize: {e}", K::crd_name()));
    let path = out.join(format!("{}.yaml", K::crd_name()));
    std::fs::write(&path, yaml).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
