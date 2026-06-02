//! Live-network smoke tests against hydra.nixos.org and
//! cache.nixos.org. #[ignore]d: CI has no network access. Run manually
//! (one-time prep before the first real eval-set build, and to confirm
//! the recorded fixtures still match what the live services serve):
//!
//!   nix develop -c cargo nextest run -p rio-replay --run-ignored all -E 'binary(live_network)'
//!
//! Politeness: this file issues 4 hydra.nixos.org requests and 1
//! cache.nixos.org request total.

use rio_replay::hydra::HydraClient;
use rio_replay::narhash::NarHash;
use rio_replay::nixcache::NixCacheClient;

const EVAL_ID: u64 = 1824219;

#[tokio::test]
#[ignore = "live network: hydra.nixos.org + cache.nixos.org (manual smoke, never in CI)"]
async fn live_hydra_eval_jobset_and_constituents_shape() {
    let ua = rio_replay::user_agent(std::env::var("RIO_REPLAY_CONTACT").ok().as_deref());
    let c = HydraClient::new(
        "https://hydra.nixos.org",
        &ua,
        10,
        std::time::Duration::from_millis(1000),
    )
    .unwrap();

    let eval = c.get_eval(EVAL_ID).await.expect("GET /eval/<id>");
    assert_eq!(eval.id, EVAL_ID);
    assert_eq!(
        eval.jobsetevalinputs["nixpkgs"].revision.as_deref(),
        Some("68d8aa3d661f0e6bd5862291b5bb263b2a6595c9"),
        "recorded fixture and live response must agree on the pinned revision"
    );

    let js = c
        .get_jobset("nixos", "unstable")
        .await
        .expect("GET /jobset");
    assert_eq!(
        js.nixexprpath.as_deref(),
        Some("nixos/release-combined.nix")
    );

    // Verifies the /build/<id>/constituents response shape — the one
    // Hydra endpoint without a recorded fixture (see the
    // `HydraClient::get_constituents` TODO).
    let agg = c
        .get_eval_job(EVAL_ID, "tested")
        .await
        .expect("GET /eval/<id>/job/tested");
    let constituents = c
        .get_constituents(agg.id)
        .await
        .expect("GET /build/<id>/constituents");
    assert!(
        constituents
            .iter()
            .any(|b| b.job == "nixos.iso_minimal.x86_64-linux"),
        "constituents of `tested` should include nixos.iso_minimal.x86_64-linux; got {} entries",
        constituents.len()
    );
}

#[tokio::test]
#[ignore = "live network: cache.nixos.org (manual smoke, never in CI)"]
async fn live_cache_narinfo_for_a_known_hydra_output() {
    let ua = rio_replay::user_agent(None);
    let c = NixCacheClient::new("https://cache.nixos.org", &ua).unwrap();
    // Output path of nixpkgs.hello.x86_64-linux from the recorded
    // job fixture (eval 1824219, build 324433458).
    let info = c
        .fetch_narinfo("/nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3")
        .await
        .expect("HTTP ok")
        .expect("hello narinfo present upstream");
    assert!(info.nar_hash.starts_with("sha256:"));
    let hexhash = NarHash::parse(&info.nar_hash)
        .expect("convertible")
        .to_hex();
    assert_eq!(hexhash.len(), 64);
    assert!(info.nar_size > 0);
}
