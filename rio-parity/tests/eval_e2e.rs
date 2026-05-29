//! Scoped end-to-end recorder run. #[ignore]d: needs network
//! (hydra.nixos.org, the github.com tarball, and cache.nixos.org for
//! the truth sweep), `nix`, `nix-eval-jobs`, and `mkdwarfs` on PATH,
//! and imports the nixpkgs tree (~300-700 MB) into the local
//! /nix/store. This is the one-time drvPath fidelity-gate run for a
//! scoped recording — the same flow `rio-parity eval` runs for real
//! campaigns — pinned to the recorded eval 1824219 so its expected
//! drvPaths are known in advance.
//!
//!   nix develop -c cargo nextest run -p rio-parity --run-ignored all -E 'binary(eval_e2e)'
//!
//! Budget: ~5 hydra.nixos.org requests, one ~50 MB github tarball, a
//! handful of cache.nixos.org narinfo fetches, a few minutes of
//! evaluation, and one mkdwarfs pack of a small archive.

use rio_parity::archive::reader::ReplayArchive;
use rio_parity::cmd::eval::{EvalArgs, run};

const HELLO_DRV: &str = "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv";
const JQ_DRV: &str = "/nix/store/0vb08cn5pf24mzjbibpc7n37g62lacfj-jq-1.8.1.drv";

#[tokio::test]
#[ignore = "needs nix + nix-eval-jobs + mkdwarfs + network (manual fidelity-gate run, never in CI)"]
async fn eval_scoped_two_plain_jobs_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("out");

    let args = EvalArgs {
        hydra_eval: 1824219,
        scope: "jobs:nixpkgs.hello.x86_64-linux,nixpkgs.jq.x86_64-linux".into(),
        jobset: Some("nixos/unstable".into()),
        systems: vec!["x86_64-linux".into()],
        out_dir: out_dir.clone(),
        work_dir: None,
        s3_bucket: None,
        s3_prefix: "parity".into(),
        dry_run: false,
        force: false,
        hydra_url: "https://hydra.nixos.org".into(),
        cache_url: "https://cache.nixos.org".into(),
        narinfo_concurrency: 8,
        contact: std::env::var("RIO_PARITY_CONTACT").ok(),
        hydra_request_cap: None,
        source_tarball_url: None,
        rev_count: None,
        short_rev: None,
        // Plain packages carry no versionSuffix; nixos.channel does
        // (recorded fixture: releasename nixos-26.05pre975402.68d8aa3d661f).
        version_job: Some("nixos.channel".into()),
        nix_bin: "nix".into(),
        nix_eval_jobs_bin: "nix-eval-jobs".into(),
        eval_workers: 2,
        eval_max_memory_mb: 4096,
        fidelity_samples: 100,
    };

    run(args)
        .await
        .expect("the recording should succeed and be non-divergent");

    // Locate the single produced recording dir.
    let eval_dir = out_dir.join("1824219");
    let digests: Vec<_> = std::fs::read_dir(&eval_dir).unwrap().collect();
    assert_eq!(digests.len(), 1, "exactly one recipe-digest prefix");
    let set = digests[0].as_ref().unwrap().path();

    // fidelity.json: exhaustive, no mismatches.
    let fidelity: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(set.join("fidelity.json")).unwrap()).unwrap();
    assert_eq!(fidelity["divergent"], false);
    assert_eq!(fidelity["checked"], 2);

    // The packed image and the standalone manifest sit next to the
    // staging directory; the image is the published form, so open that.
    let image = set.join("archive.dwarfs");
    let standalone_manifest = set.join("manifest.json");
    assert!(image.is_file(), "archive.dwarfs missing from {set:?}");
    assert!(
        standalone_manifest.is_file(),
        "manifest.json missing from {set:?}"
    );

    let archive = ReplayArchive::open(&image).expect("open the packed archive");
    let manifest = archive.manifest();
    assert!(!manifest.capabilities.timed);
    assert!(manifest.capabilities.expected_outcomes);
    assert!(manifest.capabilities.dependency_closures);
    assert_eq!(manifest.counts.workload_units, 2);
    assert_eq!(manifest.counts.requests, 2);
    assert_eq!(
        manifest.substituters.relay,
        vec!["https://cache.nixos.org".to_string()]
    );

    // units.jsonl: both jobs, drvPaths bit-identical to Hydra's (the
    // recorded fixtures pin the expected values).
    let units = archive.units();
    assert_eq!(units.len(), 2);
    assert_eq!(
        units[HELLO_DRV].label.as_deref(),
        Some("nixpkgs.hello.x86_64-linux")
    );
    assert_eq!(
        units[JQ_DRV].label.as_deref(),
        Some("nixpkgs.jq.x86_64-linux")
    );

    // closures.jsonl: direct adjacency over the union closure (the
    // hello closure alone holds several hundred derivations), and the
    // workload units' ATerm members are embedded and readable back.
    assert!(archive.closures().len() > 500);
    assert!(archive.read_drv(HELLO_DRV).unwrap().contains("Derive("));

    // outcomes.jsonl: truth swept at creation — hello has been on
    // cache.nixos.org (and green on Hydra) for this recorded eval, so
    // its expected outcome is `built` with per-output NAR identity.
    let hello_outcome = archive
        .expected_outcome(0, HELLO_DRV)
        .expect("hello has an expected outcome");
    assert_eq!(hello_outcome.outcome.as_str(), "built");
    assert!(!hello_outcome.outputs.is_empty());

    // Provenance carries the recorder identity, the recipe, and the
    // source coordinates.
    assert_eq!(manifest.provenance["recorder"], "rio-parity-eval");
    assert_eq!(manifest.provenance["source"]["hydra_eval_id"], 1824219);
    assert_eq!(
        manifest.provenance["recipe_digest"].as_str().unwrap().len(),
        64
    );
    assert!(
        manifest.provenance["evaluator"]["argv"]
            .as_array()
            .unwrap()
            .len()
            > 5
    );
    assert!(
        manifest.provenance["source"]["source_store_path"]
            .as_str()
            .unwrap()
            .ends_with("-source")
    );

    // The standalone manifest copy is identity-equivalent to the image:
    // it hashes to the archive id the reader derived from the image.
    let manifest_bytes = std::fs::read(&standalone_manifest).unwrap();
    let derived = rio_parity::archive::identity::archive_id_from_manifest_bytes(&manifest_bytes);
    assert_eq!(archive.archive_id(), Some(derived.as_str()));
}
