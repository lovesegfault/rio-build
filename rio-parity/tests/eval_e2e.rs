//! Scoped end-to-end eval-set build. #[ignore]d: needs network
//! (hydra.nixos.org + the github.com tarball), `nix`, and
//! `nix-eval-jobs` on PATH, and imports the nixpkgs tree (~300-700 MB)
//! into the local /nix/store. This is the one-time drvPath
//! fidelity-gate run for a scoped eval set — the same flow
//! `rio-parity eval` runs for real campaigns — pinned to the recorded
//! eval 1824219 so its expected drvPaths are known in advance.
//!
//!   nix develop -c cargo nextest run -p rio-parity --run-ignored all -E 'binary(eval_e2e)'
//!
//! Budget: ~5 hydra.nixos.org requests, one ~50 MB github tarball, a
//! few minutes of evaluation.

use rio_parity::cmd::eval::{EvalArgs, run};

#[tokio::test]
#[ignore = "needs nix + nix-eval-jobs + network (manual fidelity-gate run, never in CI)"]
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
        .expect("eval-set build should succeed and be non-divergent");

    // Locate the single produced eval-set dir.
    let eval_dir = out_dir.join("1824219");
    let digests: Vec<_> = std::fs::read_dir(&eval_dir).unwrap().collect();
    assert_eq!(digests.len(), 1, "exactly one key-digest prefix");
    let set = digests[0].as_ref().unwrap().path();

    // manifest.jsonl: both jobs, drvPaths bit-identical to Hydra's
    // (the recorded fixtures pin the expected values).
    let manifest = std::fs::read_to_string(set.join("manifest.jsonl")).unwrap();
    assert!(manifest.contains("/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv"));
    assert!(manifest.contains("/nix/store/0vb08cn5pf24mzjbibpc7n37g62lacfj-jq-1.8.1.drv"));
    assert_eq!(manifest.lines().count(), 2);

    // fidelity.json: exhaustive, no mismatches.
    let fidelity: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(set.join("fidelity.json")).unwrap()).unwrap();
    assert_eq!(fidelity["divergent"], false);
    assert_eq!(fidelity["checked"], 2);

    // dep-closure.jsonl: one record per job, adjacency form with one
    // deps entry per dependency drv (the hello closure alone holds
    // several hundred derivations).
    let dep = std::fs::read_to_string(set.join("dep-closure.jsonl")).unwrap();
    assert_eq!(dep.lines().count(), 2);
    let first: serde_json::Value = serde_json::from_str(dep.lines().next().unwrap()).unwrap();
    let deps = first["deps"].as_array().unwrap();
    assert!(deps.len() > 500);
    assert!(
        deps[0].get("drvPath").is_some() && deps[0].get("outputPaths").is_some(),
        "deps entries carry drvPath + outputPaths (adjacency form)"
    );

    // drvs.tar.zst exists and is a zstd stream.
    let archive = std::fs::read(set.join("drvs.tar.zst")).unwrap();
    assert!(
        archive.len() > 100_000,
        "archive suspiciously small: {}",
        archive.len()
    );
    assert_eq!(&archive[..4], &[0x28, 0xB5, 0x2F, 0xFD]);

    // evalset.json records the argv and the verdict.
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(set.join("evalset.json")).unwrap()).unwrap();
    assert_eq!(meta["fidelity_divergent"], false);
    assert_eq!(meta["dry_run"], false);
    assert!(meta["evaluator_argv"].as_array().unwrap().len() > 5);
    assert!(
        meta["source_store_path"]
            .as_str()
            .unwrap()
            .ends_with("-source")
    );
}
