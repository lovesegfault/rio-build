//! Manifest-diff reconciler unit tests.
//!
//! Covers the PURE pieces of `manifest.rs`: `compute_spawn_plan`
//! (diff logic), `group_by_bucket` (manifest → demand map),
//! `bucket_labels` ↔ `parse_bucket_from_labels` round-trip, and
//! `build_manifest_job` spec shape. No K8s apiserver interaction —
//! reconcile-loop wiring (`reconcile_manifest` I/O against a real
//! apiserver) is covered by the `manifest-pool` subtest at
//! `nix/tests/scenarios/lifecycle.nix` (wired via default.nix
//! vm-lifecycle-autoscale-k3s).
//!
//! Plan 503 T4.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::{Job, JobStatus};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::ObjectMeta;
use rio_proto::types::{DerivationResourceEstimate, ExecutorInfo};

use crate::crds::builderpool::{Replicas, Sizing};
use crate::fixtures::test_sched_addrs;
use crate::reconcilers::builderpool::manifest::{
    Bucket, CPU_CLASS_LABEL, CRASH_LOOP_WARN_THRESHOLD, FAILED_SWEEP_MIN, FLOOR_CLASS,
    MEMORY_CLASS_LABEL, SIZING_LABEL, SIZING_MANIFEST, SpawnDirective, bucket_labels,
    build_manifest_job, compute_spawn_plan, compute_surplus, crash_loop_tier, group_by_bucket,
    inventory_by_bucket, parse_bucket_from_labels, select_deletable_jobs, select_failed_jobs,
    sweep_cap, truncate_plan, update_idle_and_reapable,
};

use super::*;

const GI: u64 = 1024 * 1024 * 1024;

/// BuilderPool with `sizing=Manifest`. Starts from the shared
/// fixture (E0063-proof) and overrides the manifest-relevant fields.
fn test_manifest_wp() -> BuilderPool {
    let mut spec = crate::fixtures::test_workerpool_spec();
    spec.replicas = Replicas { min: 0, max: 10 };
    spec.sizing = Sizing::Manifest;
    spec.max_concurrent_builds = 1; // CEL-enforced for Manifest
    spec.fuse_cache_size = "10Gi".into();
    let mut wp = BuilderPool::new("mf-pool", spec);
    wp.metadata.uid = Some("uid-mf".into());
    wp.metadata.namespace = Some("rio".into());
    wp
}

/// Default scale-down window (matches `ScalingTiming::default`).
const WINDOW: Duration = Duration::from_secs(600);

/// Minimal Job with manifest labels. Status unset → "active"
/// (not Complete, not Failed). The reconciler only reads
/// `metadata.labels` + `status.{succeeded,failed}`.
fn job_with_bucket(bucket: Option<Bucket>) -> Job {
    let (mem, cpu) = bucket_labels(bucket);
    let mut labels = BTreeMap::new();
    labels.insert("rio.build/pool".into(), "mf-pool".into());
    labels.insert(SIZING_LABEL.into(), SIZING_MANIFEST.into());
    labels.insert(MEMORY_CLASS_LABEL.into(), mem);
    labels.insert(CPU_CLASS_LABEL.into(), cpu);
    Job {
        metadata: ObjectMeta {
            labels: Some(labels),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn est(mem_bytes: u64, cpu_m: u32) -> DerivationResourceEstimate {
    DerivationResourceEstimate {
        est_memory_bytes: mem_bytes,
        est_cpu_millicores: cpu_m,
        est_duration_secs: 0, // ignored by the diff
    }
}

// ─── compute_spawn_plan ──────────────────────────────────────────

/// The exit-criterion case: 3 demand / 1 supply → 2 spawns for that
/// bucket; 1 demand / 0 supply → 1 spawn for the other.
///
/// Given manifest `{(8Gi,2000m): 3, (32Gi,4000m): 1}` and inventory
/// `{(8Gi,2000m): 1}`, expect `(8Gi,2000m)×2 + (32Gi,4000m)×1`.
// r[verify ctrl.pool.manifest-reconcile]
#[test]
fn diff_three_demand_one_supply_spawns_two() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);

    let mut demand = BTreeMap::new();
    demand.insert(b8, 3);
    demand.insert(b32, 1);

    let mut supply = BTreeMap::new();
    supply.insert(b8, 1);
    // b32: no supply

    let plan = compute_spawn_plan(&demand, &supply, 0, 0);

    assert_eq!(plan.len(), 2, "two buckets with deficit");
    // BTreeMap iteration: (8Gi,2000m) < (32Gi,4000m) → 8Gi first.
    assert_eq!(
        plan[0],
        SpawnDirective {
            bucket: Some(b8),
            count: 2
        },
        "8Gi bucket: 3 demand - 1 supply = 2"
    );
    assert_eq!(
        plan[1],
        SpawnDirective {
            bucket: Some(b32),
            count: 1
        },
        "32Gi bucket: 1 demand - 0 supply = 1"
    );
}

/// Over-provisioned bucket (supply > demand) → zero spawns. Scale-down
/// is NOT this reconciler's job (P0505). A bucket in supply but absent
/// from demand (derivation completed) → also zero.
///
/// Mutation check: replace `saturating_sub` with `-` → underflow panic
/// on the supply>demand case.
// r[verify ctrl.pool.manifest-reconcile]
#[test]
fn overprovisioned_bucket_zero_spawns() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);

    let mut demand = BTreeMap::new();
    demand.insert(b8, 1);
    // b32: no demand (derivation completed)

    let mut supply = BTreeMap::new();
    supply.insert(b8, 5); // over-provisioned
    supply.insert(b32, 2); // orphaned

    let plan = compute_spawn_plan(&demand, &supply, 0, 0);

    assert!(
        plan.is_empty(),
        "over-provisioned + orphaned → no spawns. Got: {plan:?}"
    );
}

/// Cold-start: manifest omits 2 derivations (`queued_total -
/// manifest.len() = 2`), no floor Jobs running yet → 2 Jobs at floor.
/// Bucket: `None` (→ `spec.resources`).
// r[verify ctrl.pool.manifest-reconcile]
#[test]
fn cold_start_omitted_derivations_get_floor_jobs() {
    let demand = BTreeMap::new();
    let supply = BTreeMap::new();

    let plan = compute_spawn_plan(&demand, &supply, 2, 0);

    assert_eq!(
        plan,
        vec![SpawnDirective {
            bucket: None,
            count: 2
        }],
        "2 cold-start derivations → 2 floor Jobs"
    );
}

/// Cold-start with some floor supply already running: 3 cold-start
/// demand, 1 floor Job active → spawn 2 more. Same subtraction
/// semantics as ephemeral's `spawn_count` (active Job claims a
/// queued derivation).
#[test]
fn cold_start_subtracts_floor_supply() {
    let plan = compute_spawn_plan(&BTreeMap::new(), &BTreeMap::new(), 3, 1);
    assert_eq!(
        plan,
        vec![SpawnDirective {
            bucket: None,
            count: 2
        }]
    );

    // Over-provisioned floor: 1 demand, 3 supply → zero.
    let plan = compute_spawn_plan(&BTreeMap::new(), &BTreeMap::new(), 1, 3);
    assert!(plan.is_empty());
}

/// Mixed: bucketed demand + cold-start in the same plan. BTreeMap
/// iteration order → bucketed directives first, cold-start last.
#[test]
fn mixed_bucketed_and_cold_start() {
    let b8 = (8 * GI, 2000);
    let mut demand = BTreeMap::new();
    demand.insert(b8, 2);

    let plan = compute_spawn_plan(&demand, &BTreeMap::new(), 3, 0);

    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].bucket, Some(b8), "bucketed first");
    assert_eq!(plan[0].count, 2);
    assert_eq!(plan[1].bucket, None, "cold-start last");
    assert_eq!(plan[1].count, 3);
}

/// Zero demand + zero cold-start → empty plan. No `count: 0` noise.
#[test]
fn empty_manifest_empty_plan() {
    let plan = compute_spawn_plan(&BTreeMap::new(), &BTreeMap::new(), 0, 0);
    assert!(plan.is_empty());
}

// ─── truncate_plan ───────────────────────────────────────────────

// r[verify ctrl.pool.manifest-fairness]
/// The P0503 regression: 100-tiny + 3-huge + 20-cold, budget=10.
/// OLD truncation: 10× tiny, 0× huge, 0× cold. Cold never escapes.
/// NEW: floor pass gives 1 each (3 directives → 3 budget), then
/// proportional on 7 remaining (tiny gets most; huge + cold get
/// their proportion of 2+19 / 120).
#[test]
fn truncate_plan_per_bucket_floor_under_sustained_tiny_load() {
    let plan = vec![
        SpawnDirective {
            bucket: Some((4 * GI, 2000)),
            count: 100,
        }, // tiny
        SpawnDirective {
            bucket: Some((48 * GI, 16000)),
            count: 3,
        }, // huge
        SpawnDirective {
            bucket: None,
            count: 20,
        }, // cold
    ];
    let out = truncate_plan(&plan, 10);

    assert_eq!(out.len(), 3, "all three classes spawn at least once");
    // Floor: every class got ≥1.
    for d in &out {
        assert!(
            d.count >= 1,
            "bucket {:?} starved (count={})",
            d.bucket,
            d.count
        );
    }
    // Budget conserved.
    assert_eq!(out.iter().map(|d| d.count).sum::<usize>(), 10);
    // Cold-start specifically (the escape-hatch bug): it's in the output.
    assert!(
        out.iter().any(|d| d.bucket.is_none() && d.count >= 1),
        "cold-start must spawn — otherwise derivations never graduate"
    );
}

/// Total demand 5, budget 20 → spawn exactly 5 (don't over-allocate).
/// The old inline loop got this for free by iterating up to
/// `directive.count`; two-pass needs an explicit cap.
#[test]
fn truncate_plan_budget_exceeds_demand_no_overspawn() {
    let plan = vec![
        SpawnDirective {
            bucket: Some((8 * GI, 4000)),
            count: 3,
        },
        SpawnDirective {
            bucket: None,
            count: 2,
        },
    ];
    let out = truncate_plan(&plan, 20);
    assert_eq!(out.iter().map(|d| d.count).sum::<usize>(), 5);
    assert_eq!(out[0].count, 3);
    assert_eq!(out[1].count, 2);
}

/// 5 buckets, budget 3 → first 3 get their floor, last 2 starve
/// THIS tick. Next tick, supply has changed (those 3 are now
/// counted), so floor-pass covers the next slice. N ticks = coverage.
#[test]
fn truncate_plan_budget_less_than_buckets_covers_prefix() {
    let plan: Vec<_> = (1..=5)
        .map(|i| SpawnDirective {
            bucket: Some((i as u64 * 4 * GI, 2000)),
            count: 10,
        })
        .collect();
    let out = truncate_plan(&plan, 3);
    assert_eq!(out.len(), 3);
    for d in &out {
        assert_eq!(d.count, 1);
    }
}

#[test]
fn truncate_plan_zero_budget_noop() {
    let plan = vec![SpawnDirective {
        bucket: None,
        count: 5,
    }];
    assert!(truncate_plan(&plan, 0).is_empty());
}

// ─── group_by_bucket ─────────────────────────────────────────────

/// Three estimates with the same (mem, cpu) → count=3 at one key.
/// Scheduler pre-bucketed, so this is the common case.
#[test]
fn grouping_collapses_identical_buckets() {
    let estimates = vec![
        est(8 * GI, 2000),
        est(8 * GI, 2000),
        est(8 * GI, 2000),
        est(32 * GI, 4000),
    ];
    let grouped = group_by_bucket(&estimates);
    assert_eq!(grouped.len(), 2, "two distinct buckets");
    assert_eq!(grouped.get(&(8 * GI, 2000)), Some(&3));
    assert_eq!(grouped.get(&(32 * GI, 4000)), Some(&1));
}

/// Same memory, different cpu → DIFFERENT buckets. A 4Gi/2000m build
/// and a 4Gi/8000m build don't share a pod.
#[test]
fn grouping_key_is_both_dimensions() {
    let estimates = vec![est(4 * GI, 2000), est(4 * GI, 8000)];
    let grouped = group_by_bucket(&estimates);
    assert_eq!(
        grouped.len(),
        2,
        "same mem, different cpu → two buckets — key is (mem, cpu) not mem alone"
    );
}

// ─── label round-trip ────────────────────────────────────────────

/// bucket_labels ↔ parse_bucket_from_labels: inverse functions.
/// THE critical round-trip — a drift here = perpetual over-spawn
/// (reconciler never counts its own Jobs as supply). This test
/// is the CI hedge against future label-format edits.
///
/// Covers: typical buckets (scheduler's 4GiB/2000m grid) and the
/// boundary case (4Gi — smallest non-zero scheduler bucket).
// r[verify ctrl.pool.manifest-labels]
#[test]
fn label_roundtrip() {
    let cases: &[Bucket] = &[
        (4 * GI, 2000),
        (8 * GI, 2000),
        (32 * GI, 4000),
        (48 * GI, 16000),
        (256 * GI, 64000),
    ];
    for &bucket in cases {
        let (mem_label, cpu_label) = bucket_labels(Some(bucket));

        // Stitch into a minimal Job with ONLY the labels the parser
        // reads — proves the parser isn't accidentally depending on
        // other fields.
        let j = Job {
            metadata: ObjectMeta {
                labels: Some(BTreeMap::from([
                    (MEMORY_CLASS_LABEL.into(), mem_label.clone()),
                    (CPU_CLASS_LABEL.into(), cpu_label.clone()),
                ])),
                ..Default::default()
            },
            ..Default::default()
        };
        let parsed = parse_bucket_from_labels(&j);
        assert_eq!(
            parsed,
            Some(bucket),
            "round-trip failed for ({mem_label}, {cpu_label}): got {parsed:?}"
        );
    }
}

/// Cold-start: `bucket_labels(None)` → `("floor", "floor")` →
/// `parse_bucket_from_labels` → `None`. The floor sentinel is NOT a
/// bucket; floor supply is counted separately via `is_floor_job`.
#[test]
fn label_roundtrip_floor_sentinel() {
    let (mem, cpu) = bucket_labels(None);
    assert_eq!(mem, FLOOR_CLASS);
    assert_eq!(cpu, FLOOR_CLASS);

    let j = Job {
        metadata: ObjectMeta {
            labels: Some(BTreeMap::from([
                (MEMORY_CLASS_LABEL.into(), mem),
                (CPU_CLASS_LABEL.into(), cpu),
            ])),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        parse_bucket_from_labels(&j),
        None,
        "floor labels parse to None — not a bucket"
    );
}

/// parse_bucket_from_labels returns None for garbage. A foreign Job
/// with `rio.build/sizing=manifest` and broken class labels doesn't
/// crash the reconciler, doesn't count as supply.
#[test]
fn parse_rejects_malformed_labels() {
    let mk = |mem: &str, cpu: &str| Job {
        metadata: ObjectMeta {
            labels: Some(BTreeMap::from([
                (MEMORY_CLASS_LABEL.into(), mem.into()),
                (CPU_CLASS_LABEL.into(), cpu.into()),
            ])),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(parse_bucket_from_labels(&mk("8Gi", "")), None, "empty cpu");
    assert_eq!(
        parse_bucket_from_labels(&mk("", "2000m")),
        None,
        "empty mem"
    );
    assert_eq!(
        parse_bucket_from_labels(&mk("8G", "2000m")),
        None,
        "wrong mem suffix (G not Gi)"
    );
    assert_eq!(
        parse_bucket_from_labels(&mk("8Gi", "2000")),
        None,
        "missing cpu suffix"
    );
    assert_eq!(
        parse_bucket_from_labels(&mk("eightGi", "2000m")),
        None,
        "non-numeric mem"
    );
    // Missing label entirely → None (no panic on `.get().unwrap()`).
    assert_eq!(
        parse_bucket_from_labels(&Job::default()),
        None,
        "no labels at all"
    );
}

/// inventory_by_bucket: reconstruct supply map from a list of Jobs.
/// Skips floor Jobs (counted separately) and unparseable Jobs.
#[test]
fn inventory_groups_by_parsed_labels() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);
    let jobs = [
        job_with_bucket(Some(b8)),
        job_with_bucket(Some(b8)),
        job_with_bucket(Some(b32)),
        job_with_bucket(None), // floor — NOT in the bucket map
    ];
    let refs: Vec<&Job> = jobs.iter().collect();
    let inv = inventory_by_bucket(&refs);
    assert_eq!(inv.len(), 2, "floor Job not a bucket");
    assert_eq!(inv.get(&b8), Some(&2));
    assert_eq!(inv.get(&b32), Some(&1));
}

// ─── build_manifest_job ──────────────────────────────────────────

/// Spawned Job carries the round-trip labels. This is the
/// end-to-end assertion: `build_manifest_job` sets what
/// `parse_bucket_from_labels` reads. The `label_roundtrip` test
/// proves the formatting pair agrees; THIS proves `build_manifest_job`
/// actually CALLS the formatting.
///
/// Also asserts pod-template labels match (kube-scheduler packs by
/// pod labels, `kubectl get pods -l rio.build/memory-class=8Gi`
/// should work).
// r[verify ctrl.pool.manifest-labels]
#[test]
fn built_job_labels_roundtrip_through_inventory() {
    let wp = test_manifest_wp();
    let oref = wp.controller_owner_ref(&()).unwrap();
    let bucket = (8 * GI, 2000);

    let job = build_manifest_job(&wp, oref, &test_sched_addrs(), "store:9002", Some(bucket))
        .expect("build_manifest_job");

    // The Job's own labels.
    let labels = job.metadata.labels.as_ref().unwrap();
    assert_eq!(
        labels.get(SIZING_LABEL),
        Some(&SIZING_MANIFEST.to_string()),
        "sizing label — inventory selector filters on this"
    );
    assert_eq!(labels.get(MEMORY_CLASS_LABEL), Some(&"8Gi".to_string()));
    assert_eq!(labels.get(CPU_CLASS_LABEL), Some(&"2000m".to_string()));
    assert_eq!(
        labels.get("rio.build/pool"),
        Some(&"mf-pool".to_string()),
        "pool label — inventory selector filters on this too"
    );

    // Pod-template labels: same set. `kubectl get pods -l ...` works.
    let pod_labels = job
        .spec
        .as_ref()
        .unwrap()
        .template
        .metadata
        .as_ref()
        .unwrap()
        .labels
        .as_ref()
        .unwrap();
    assert_eq!(pod_labels.get(MEMORY_CLASS_LABEL), Some(&"8Gi".to_string()));

    // ROUND-TRIP: the Job we just built parses back to the bucket we
    // gave it. If this fails, the reconciler spawns it next tick
    // AGAIN (doesn't count as supply) → runaway.
    assert_eq!(
        parse_bucket_from_labels(&job),
        Some(bucket),
        "spawn→inventory round-trip — this failing = perpetual over-spawn"
    );
}

/// build_manifest_job applies the resources override to the pod.
/// The bucket's (mem, cpu) shows up in `containers[0].resources`
/// (plus the FUSE device-plugin request that `build_executor_pod_spec`
/// always adds for non-privileged pods).
// r[verify ctrl.pool.manifest-reconcile]
#[test]
fn built_job_pod_has_bucket_resources() {
    let wp = test_manifest_wp();
    let oref = wp.controller_owner_ref(&()).unwrap();
    let bucket = (32 * GI, 4000);

    let job = build_manifest_job(&wp, oref, &test_sched_addrs(), "store:9002", Some(bucket))
        .expect("build_manifest_job");

    let pod_spec = job.spec.unwrap().template.spec.unwrap();
    let resources = pod_spec.containers[0]
        .resources
        .as_ref()
        .expect("resources set — this is the WHOLE POINT of manifest mode");

    // Memory: 32Gi in bytes. Quantity string is raw bytes (see
    // bucket_to_resources).
    let requests = resources.requests.as_ref().unwrap();
    assert_eq!(
        requests.get("memory"),
        Some(&Quantity((32 * GI).to_string())),
        "memory request from bucket, not spec.resources (which is None)"
    );
    assert_eq!(requests.get("cpu"), Some(&Quantity("4000m".into())));

    // Limits = requests (precise-fit, no burst).
    let limits = resources.limits.as_ref().unwrap();
    assert_eq!(limits.get("memory"), requests.get("memory"));
    assert_eq!(limits.get("cpu"), requests.get("cpu"));
}

/// Cold-start Job (`bucket: None`) falls through to `spec.resources`.
/// test_manifest_wp has `spec.resources = None` (from the fixture),
/// so the pod gets just the FUSE device-plugin request — NOT a
/// manufactured bucket.
#[test]
fn cold_start_job_uses_spec_resources_floor() {
    let wp = test_manifest_wp();
    // spec.resources is None (fixture default). A real operator would
    // set a floor, but proving the cold-start path does NOT invent a
    // bucket is the point here.
    assert!(wp.spec.resources.is_none(), "precondition");

    let oref = wp.controller_owner_ref(&()).unwrap();
    let job = build_manifest_job(&wp, oref, &test_sched_addrs(), "store:9002", None)
        .expect("build_manifest_job");

    // Label is the floor sentinel.
    assert_eq!(
        job.metadata
            .labels
            .as_ref()
            .unwrap()
            .get(MEMORY_CLASS_LABEL),
        Some(&FLOOR_CLASS.to_string())
    );

    // Resources: no memory/cpu request (spec.resources was None,
    // override was None → params.resources stays None → only the
    // FUSE device-plugin request). If someone's Some-path leaks
    // into the None-path, memory/cpu would appear.
    let pod_spec = job.spec.unwrap().template.spec.unwrap();
    let resources = pod_spec.containers[0].resources.as_ref().unwrap();
    assert!(
        resources
            .requests
            .as_ref()
            .map(|r| !r.contains_key("memory") && !r.contains_key("cpu"))
            .unwrap_or(true),
        "cold-start with spec.resources=None → no memory/cpu request"
    );
}

/// Job spec load-bearing fields. Mirror of ephemeral's
/// `job_spec_load_bearing_fields` with manifest-specific assertions.
/// Key DIFFERENCES from ephemeral:
///   - NO RIO_EPHEMERAL (pod loops — would never complete with it)
///   - NO active_deadline_seconds (long-lived)
///   - RIO_MAX_BUILDS=1 (CEL defensive override, SAME as ephemeral)
// r[verify ctrl.pool.manifest-single-build]
#[test]
fn job_spec_load_bearing_fields() {
    let wp = test_manifest_wp();
    let oref = wp.controller_owner_ref(&()).unwrap();
    let job = build_manifest_job(
        &wp,
        oref,
        &test_sched_addrs(),
        "store:9002",
        Some((8 * GI, 2000)),
    )
    .unwrap();

    // ownerReference → GC on BuilderPool delete.
    let orefs = job.metadata.owner_references.as_ref().unwrap();
    assert_eq!(orefs[0].kind, "BuilderPool");
    assert_eq!(orefs[0].controller, Some(true));

    let spec = job.spec.as_ref().unwrap();
    assert_eq!(spec.backoff_limit, Some(0), "K8s must not retry");
    assert_eq!(spec.parallelism, Some(1));
    // r[verify ctrl.pool.manifest-long-lived]
    assert_eq!(
        spec.ttl_seconds_after_finished, None,
        "manifest pods never self-terminate (no RIO_EPHEMERAL) → \
         controller deletion is the only scale-down path → TTL \
         would race it"
    );
    assert_eq!(
        spec.active_deadline_seconds, None,
        "manifest pods are long-lived — no deadline (wrong-pool-spawn \
         isn't a concern for demand-driven buckets)"
    );

    let pod_spec = spec.template.spec.as_ref().unwrap();
    assert_eq!(pod_spec.restart_policy.as_deref(), Some("Never"));

    // r[verify ctrl.pool.manifest-long-lived]
    // NO RIO_EPHEMERAL — manifest pods loop. An accidental
    // RIO_EPHEMERAL=1 here would make every manifest pod exit
    // after one build → constant respawn churn.
    let env = pod_spec.containers[0].env.as_ref().unwrap();
    assert!(
        !env.iter().any(|e| e.name == "RIO_EPHEMERAL"),
        "manifest pods are NOT ephemeral — no RIO_EPHEMERAL env"
    );

    // RIO_MAX_BUILDS=1 (defensive override).
    let max_builds: Vec<_> = env.iter().filter(|e| e.name == "RIO_MAX_BUILDS").collect();
    assert_eq!(max_builds.len(), 1, "exactly one (find-and-replace)");
    assert_eq!(max_builds[0].value.as_deref(), Some("1"));

    // Sanity: build_pod_spec reuse — scheduler addr env present.
    assert!(env.iter().any(|e| e.name == "RIO_SCHEDULER_ADDR"));
}

/// Job name format: `{pool}-mf-{mem}g-{cpu}m-{random6}`. The class
/// in the name is operator-ergonomic (`kubectl get jobs` shows size);
/// labels carry the functional data.
#[test]
fn job_name_format() {
    let wp = test_manifest_wp();
    let oref = wp.controller_owner_ref(&()).unwrap();
    let job = build_manifest_job(
        &wp,
        oref,
        &test_sched_addrs(),
        "store:9002",
        Some((8 * GI, 2000)),
    )
    .unwrap();
    let name = job.metadata.name.unwrap();

    assert_eq!(wp.name_any(), "mf-pool");
    assert!(
        name.starts_with("mf-pool-mf-8g-2000m-"),
        "expected {{pool}}-mf-8g-2000m-{{suffix}}, got {name}"
    );
    let suffix = name.strip_prefix("mf-pool-mf-8g-2000m-").unwrap();
    assert_eq!(suffix.len(), 6, "6-char random suffix");
    assert!(
        suffix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        "DNS-1123 suffix: {suffix}"
    );
    // Total length well under 63.
    assert!(name.len() <= 63, "K8s name limit");
}

/// Cold-start Job name: `{pool}-mf-floorg-floorm-{random6}`.
/// `"floor"` doesn't strip either suffix in the name-tag computation
/// (no `Gi`/`m` suffix), so both tags are `"floor"`. A bit redundant
/// (`floorg-floorm`) but valid DNS-1123 and distinct from bucketed
/// names.
#[test]
fn job_name_format_cold_start() {
    let wp = test_manifest_wp();
    let oref = wp.controller_owner_ref(&()).unwrap();
    let job = build_manifest_job(&wp, oref, &test_sched_addrs(), "store:9002", None).unwrap();
    let name = job.metadata.name.unwrap();
    assert!(
        name.starts_with("mf-pool-mf-floorg-floorm-"),
        "cold-start name: got {name}"
    );
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
}

// ─── compute_surplus ─────────────────────────────────────────────

/// Mirror of `diff_three_demand_one_supply_spawns_two`: same input,
/// opposite direction. supply>demand → surplus; supply≤demand → zero.
#[test]
fn surplus_is_supply_minus_demand() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);

    let mut demand = BTreeMap::new();
    demand.insert(b8, 1);
    // b32: no demand (derivation completed, or queue drained)

    let mut supply = BTreeMap::new();
    supply.insert(b8, 5);
    supply.insert(b32, 2);

    let surplus = compute_surplus(&demand, &supply);

    assert_eq!(surplus.get(&b8), Some(&4), "5 - 1 = 4");
    assert_eq!(
        surplus.get(&b32),
        Some(&2),
        "supply-only bucket: 2 - 0 = 2 (the PRIMARY scale-down case — work finished)"
    );
}

/// Deficit buckets (demand > supply) are NOT in the surplus map.
/// No `count: 0` noise, no negative wraparound.
#[test]
fn surplus_omits_deficit_buckets() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);

    let mut demand = BTreeMap::new();
    demand.insert(b8, 3);
    demand.insert(b32, 1);

    let mut supply = BTreeMap::new();
    supply.insert(b8, 1); // deficit
    supply.insert(b32, 1); // exactly met

    let surplus = compute_surplus(&demand, &supply);
    assert!(
        surplus.is_empty(),
        "deficit + exactly-met → no surplus. Got: {surplus:?}"
    );
}

// ─── update_idle_and_reapable ────────────────────────────────────
//
// All tests simulate elapsed time via `Instant` arithmetic (no real
// sleeps — CI noisy neighbors make those flaky). Same pattern as
// `scaling/tests.rs::stabilization_*`.
//
// The function takes `now` as a parameter; tests advance it by
// adding to a fixed `t0` base. `idle_since` timestamps are stamped
// at whatever `now` was passed, so the window check is pure
// subtraction.

/// Plan T4 case 1: bucket surplus for 599s → NOT reapable.
/// Window is 600s; one second short of the threshold.
// r[verify ctrl.pool.manifest-scaledown]
#[test]
fn surplus_599s_not_reapable() {
    let b8 = (8 * GI, 2000);
    let surplus = BTreeMap::from([(b8, 3)]);
    let mut idle = BTreeMap::new();

    let t0 = Instant::now();
    // First tick: stamps t0.
    let r0 = update_idle_and_reapable(&mut idle, &surplus, t0, WINDOW);
    assert!(r0.is_empty(), "freshly surplus → not reapable");
    assert_eq!(idle.get(&b8), Some(&t0), "idle_since stamped at t0");

    // 599s later: window not elapsed.
    let t599 = t0 + Duration::from_secs(599);
    let r599 = update_idle_and_reapable(&mut idle, &surplus, t599, WINDOW);
    assert!(
        r599.is_empty(),
        "599s < 600s window → not reapable. Got: {r599:?}"
    );
    assert_eq!(
        idle.get(&b8),
        Some(&t0),
        "timestamp preserved (still surplus; or_insert, not insert)"
    );
}

/// Plan T4 case 2: bucket surplus for 601s → reapable, surplus count
/// propagated. `>=` threshold: exactly 600s is ALSO reapable.
// r[verify ctrl.pool.manifest-scaledown]
#[test]
fn surplus_601s_reapable() {
    let b8 = (8 * GI, 2000);
    let surplus = BTreeMap::from([(b8, 3)]);
    let mut idle = BTreeMap::new();

    let t0 = Instant::now();
    update_idle_and_reapable(&mut idle, &surplus, t0, WINDOW);

    // Exactly 600s: `>=` threshold → reapable.
    let t600 = t0 + WINDOW;
    let r600 = update_idle_and_reapable(&mut idle, &surplus, t600, WINDOW);
    assert_eq!(
        r600.get(&b8),
        Some(&3),
        "exactly at window → reapable (>= not >); surplus count = 3"
    );

    // 601s: definitely past.
    let t601 = t0 + Duration::from_secs(601);
    let r601 = update_idle_and_reapable(&mut idle, &surplus, t601, WINDOW);
    assert_eq!(r601.get(&b8), Some(&3), "601s > 600s → reapable");
}

/// Plan T4 case 3: demand returns mid-window → clock resets. Surplus
/// again → must wait the FULL window from the new surplus-start,
/// NOT from the original t0.
///
/// The scenario: 48Gi bucket goes surplus (no 48Gi builds queued).
/// At t=300s, a 48Gi derivation enters the queue → demand returns
/// → bucket no longer surplus → idle_since cleared. That build
/// finishes at t=400s → surplus again. The window restarts at
/// t=400s; bucket isn't reapable until t=1000s (400s + 600s).
// r[verify ctrl.pool.manifest-scaledown]
#[test]
fn demand_returns_resets_clock() {
    let b48 = (48 * GI, 4000);
    let mut idle = BTreeMap::new();

    let t0 = Instant::now();

    // t0: bucket goes surplus. Stamp.
    let surplus = BTreeMap::from([(b48, 2)]);
    update_idle_and_reapable(&mut idle, &surplus, t0, WINDOW);
    assert_eq!(idle.get(&b48), Some(&t0));

    // t=300s: demand returns (bucket no longer surplus). Clock clears.
    let t300 = t0 + Duration::from_secs(300);
    let empty = BTreeMap::new();
    let r300 = update_idle_and_reapable(&mut idle, &empty, t300, WINDOW);
    assert!(r300.is_empty());
    assert!(
        !idle.contains_key(&b48),
        "demand returned → idle_since cleared (retain prunes it)"
    );

    // t=400s: surplus again. Stamp at t400, NOT t0.
    let t400 = t0 + Duration::from_secs(400);
    update_idle_and_reapable(&mut idle, &surplus, t400, WINDOW);
    assert_eq!(
        idle.get(&b48),
        Some(&t400),
        "re-surplus → fresh stamp at t400"
    );

    // t=601s: would have been reapable from t0 (601 > 600), but the
    // clock reset at t400. Only 201s elapsed since re-surplus → NOT
    // reapable. THIS is the anti-flap check.
    let t601 = t0 + Duration::from_secs(601);
    let r601 = update_idle_and_reapable(&mut idle, &surplus, t601, WINDOW);
    assert!(
        r601.is_empty(),
        "t601 - t400 = 201s < 600s → not reapable. \
         Without the reset, this would wrongly delete. Got: {r601:?}"
    );

    // t=1000s: 600s since t400 → reapable.
    let t1000 = t0 + Duration::from_secs(1000);
    let r1000 = update_idle_and_reapable(&mut idle, &surplus, t1000, WINDOW);
    assert_eq!(
        r1000.get(&b48),
        Some(&2),
        "t1000 - t400 = 600s ≥ window → reapable"
    );
}

/// A surplus bucket that DISAPPEARS entirely (all Jobs died, or
/// operator manually deleted them) is pruned from idle_since. No
/// stale entry if that bucket later reappears.
#[test]
fn vanished_bucket_pruned_from_idle() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);
    let mut idle = BTreeMap::new();

    let t0 = Instant::now();

    // Both surplus initially.
    let both = BTreeMap::from([(b8, 1), (b32, 1)]);
    update_idle_and_reapable(&mut idle, &both, t0, WINDOW);
    assert_eq!(idle.len(), 2);

    // b32's Job died → no longer in supply → no longer surplus.
    // b8 still surplus.
    let just_b8 = BTreeMap::from([(b8, 1)]);
    update_idle_and_reapable(&mut idle, &just_b8, t0 + Duration::from_secs(100), WINDOW);
    assert_eq!(idle.len(), 1, "b32 pruned");
    assert!(idle.contains_key(&b8), "b8 retained");
    assert!(!idle.contains_key(&b32), "b32 cleared — no stale entry");
}

// ─── select_deletable_jobs ───────────────────────────────────────
//
// Tests the executor_id → Job matching + idle check. ExecutorInfo
// is constructed minimal (only executor_id + running_builds matter).
// K8s Job-pod naming: pod is `{job_name}-{random5}`, executor_id
// IS the pod name (downward API).

/// Minimal ExecutorInfo. Only `executor_id` + `running_builds` are
/// read by `select_deletable_jobs`; the rest is prost defaults.
fn executor(id: &str, running: u32) -> ExecutorInfo {
    ExecutorInfo {
        executor_id: id.into(),
        running_builds: running,
        ..Default::default()
    }
}

/// Job with a name + bucket labels. `select_deletable_jobs` matches
/// by name prefix; `job_with_bucket` (the existing fixture) has no
/// name, so this is a named variant.
fn named_job(name: &str, bucket: Bucket) -> Job {
    let mut j = job_with_bucket(Some(bucket));
    j.metadata.name = Some(name.into());
    j
}

/// Plan T4 case 4: 3 Jobs surplus, 1 is mid-build → delete at most
/// 2. The busy Job is skipped; only confirmed-idle Jobs are selected.
// r[verify ctrl.pool.manifest-scaledown]
#[test]
fn busy_job_skipped_for_deletion() {
    let b8 = (8 * GI, 2000);
    let jobs = [
        named_job("pool-mf-8g-2000m-aaa", b8),
        named_job("pool-mf-8g-2000m-bbb", b8),
        named_job("pool-mf-8g-2000m-ccc", b8),
    ];
    let refs: Vec<&Job> = jobs.iter().collect();

    // Executors: aaa idle, bbb BUSY (running_builds=1), ccc idle.
    // K8s pod naming: `{job}-{5char}` → prefix match.
    let executors = [
        executor("pool-mf-8g-2000m-aaa-xyzwq", 0),
        executor("pool-mf-8g-2000m-bbb-pqrst", 1), // mid-build!
        executor("pool-mf-8g-2000m-ccc-lmnop", 0),
    ];

    // All 3 surplus, all 3 in the reapable budget.
    let reapable = BTreeMap::from([(b8, 3)]);

    let deletable = select_deletable_jobs(&refs, &reapable, &executors);

    assert_eq!(
        deletable.len(),
        2,
        "3 surplus, 1 busy → delete at most 2. Deleting bbb would \
         orphan its build (SIGTERM mid-compilation → scheduler \
         reassigns, work wasted). Got: {:?}",
        deletable
            .iter()
            .map(|j| j.metadata.name.as_deref())
            .collect::<Vec<_>>()
    );
    // The two idle Jobs are selected; busy bbb is NOT.
    let names: Vec<&str> = deletable
        .iter()
        .map(|j| j.metadata.name.as_deref().unwrap())
        .collect();
    assert!(names.contains(&"pool-mf-8g-2000m-aaa"));
    assert!(names.contains(&"pool-mf-8g-2000m-ccc"));
    assert!(
        !names.contains(&"pool-mf-8g-2000m-bbb"),
        "busy Job NOT in deletable set"
    );
}

/// Per-bucket budget cap: surplus=1 means delete at most 1, even if
/// 3 Jobs are idle. Next tick re-diffs; if still surplus, deletes
/// the next one. Prevents over-delete when surplus shrinks between
/// the idle-stamp and the delete (demand partially returned).
#[test]
fn surplus_budget_caps_deletions_per_bucket() {
    let b8 = (8 * GI, 2000);
    let jobs = [
        named_job("p-mf-8g-2000m-aaa", b8),
        named_job("p-mf-8g-2000m-bbb", b8),
        named_job("p-mf-8g-2000m-ccc", b8),
    ];
    let refs: Vec<&Job> = jobs.iter().collect();

    // All idle.
    let executors = [
        executor("p-mf-8g-2000m-aaa-11111", 0),
        executor("p-mf-8g-2000m-bbb-22222", 0),
        executor("p-mf-8g-2000m-ccc-33333", 0),
    ];

    // Only 1 surplus (demand=2, supply=3 → surplus=1).
    let reapable = BTreeMap::from([(b8, 1)]);

    let deletable = select_deletable_jobs(&refs, &reapable, &executors);
    assert_eq!(
        deletable.len(),
        1,
        "surplus=1 → delete exactly 1, not all 3 idle Jobs"
    );
}

/// Unknown executor state (no matching ExecutorInfo) → conservative
/// skip. Pod starting up (not heartbeating yet), or RPC-timing race.
/// "Don't delete a Job mid-build" extends to "don't delete a Job
/// whose state you can't verify".
#[test]
fn unknown_executor_state_skipped() {
    let b8 = (8 * GI, 2000);
    let jobs = [
        named_job("p-mf-8g-2000m-known", b8),
        named_job("p-mf-8g-2000m-ghost", b8),
    ];
    let refs: Vec<&Job> = jobs.iter().collect();

    // Only "known" has a registered executor.
    let executors = [executor("p-mf-8g-2000m-known-abcde", 0)];

    let reapable = BTreeMap::from([(b8, 2)]);

    let deletable = select_deletable_jobs(&refs, &reapable, &executors);
    assert_eq!(deletable.len(), 1);
    assert_eq!(
        deletable[0].metadata.name.as_deref(),
        Some("p-mf-8g-2000m-known"),
        "only the confirmed-idle Job; ghost (no executor) skipped \
         — might be starting up, can't prove idle"
    );
}

/// Prefix match boundary: `{job}-` with trailing dash. Job
/// "p-mf-8g-2000m-abc" must NOT match executor of a DIFFERENT Job
/// "p-mf-8g-2000m-abcdef" (whose pod is "p-mf-8g-2000m-abcdef-
/// {5char}"). Random suffixes make collisions astronomically
/// unlikely, but the trailing dash is defense in depth.
#[test]
fn prefix_match_requires_trailing_dash() {
    let b8 = (8 * GI, 2000);
    // Two Jobs where one's name is a prefix of the other.
    let jobs = [
        named_job("p-mf-8g-2000m-abc", b8),
        named_job("p-mf-8g-2000m-abcdef", b8),
    ];
    let refs: Vec<&Job> = jobs.iter().collect();

    // Only abcdef's pod is registered (and idle). abc's pod is not
    // registered. A naive `starts_with("p-mf-8g-2000m-abc")` (no
    // dash) would falsely match "p-mf-8g-2000m-abcdef-xxxxx" →
    // wrongly conclude Job "abc" is idle.
    let executors = [executor("p-mf-8g-2000m-abcdef-zzzzz", 0)];

    let reapable = BTreeMap::from([(b8, 2)]);

    let deletable = select_deletable_jobs(&refs, &reapable, &executors);
    assert_eq!(deletable.len(), 1);
    assert_eq!(
        deletable[0].metadata.name.as_deref(),
        Some("p-mf-8g-2000m-abcdef"),
        "only the Job whose pod is registered. Job 'abc' has no \
         registered pod → unknown → skipped. Trailing dash in \
         '{{job}}-' prevents the prefix collision."
    );
}

/// Jobs in non-reapable buckets are ignored. Multi-bucket scenario:
/// 8Gi reapable (window elapsed), 32Gi NOT reapable (still within
/// window). Only 8Gi Jobs are eligible.
#[test]
fn non_reapable_bucket_ignored() {
    let b8 = (8 * GI, 2000);
    let b32 = (32 * GI, 4000);
    let jobs = [
        named_job("p-mf-8g-2000m-aaa", b8),
        named_job("p-mf-32g-4000m-bbb", b32),
    ];
    let refs: Vec<&Job> = jobs.iter().collect();

    // Both idle.
    let executors = [
        executor("p-mf-8g-2000m-aaa-11111", 0),
        executor("p-mf-32g-4000m-bbb-22222", 0),
    ];

    // Only b8 reapable (b32's window hasn't elapsed).
    let reapable = BTreeMap::from([(b8, 1)]);

    let deletable = select_deletable_jobs(&refs, &reapable, &executors);
    assert_eq!(deletable.len(), 1);
    assert_eq!(
        deletable[0].metadata.name.as_deref(),
        Some("p-mf-8g-2000m-aaa"),
        "32Gi Job skipped — bucket not in reapable map"
    );
}

/// Floor Jobs (`bucket_labels(None)` → "floor") are skipped.
/// `parse_bucket_from_labels` returns None for floor → the
/// `let Some(bucket) = ... else continue` bails early.
#[test]
fn floor_job_skipped_for_deletion() {
    let b8 = (8 * GI, 2000);
    let mut floor_job = job_with_bucket(None);
    floor_job.metadata.name = Some("p-mf-floorg-floorm-xxx".into());
    let bucketed = named_job("p-mf-8g-2000m-yyy", b8);
    let jobs = [floor_job, bucketed];
    let refs: Vec<&Job> = jobs.iter().collect();

    let executors = [
        executor("p-mf-floorg-floorm-xxx-11111", 0),
        executor("p-mf-8g-2000m-yyy-22222", 0),
    ];

    let reapable = BTreeMap::from([(b8, 1)]);

    let deletable = select_deletable_jobs(&refs, &reapable, &executors);
    assert_eq!(deletable.len(), 1);
    assert_eq!(
        deletable[0].metadata.name.as_deref(),
        Some("p-mf-8g-2000m-yyy"),
        "floor Job skipped (not a tracked bucket); bucketed Job deleted"
    );
}

// ─── select_failed_jobs ──────────────────────────────────────────

/// Job with `status.failed = 1`. With `backoff_limit=0` (manifest's
/// build_manifest_job), one pod crash → Job terminally Failed.
fn failed_job(name: &str) -> Job {
    let mut j = job_with_bucket(Some((8 * GI, 2000)));
    j.metadata.name = Some(name.into());
    j.status = Some(JobStatus {
        failed: Some(1),
        ..Default::default()
    });
    j
}

/// 5 Failed + 3 active → all 5 Failed selected, 3 active untouched.
/// Failed Jobs need NO idle-check: `status.failed > 0` means the pod
/// already terminated — nothing to interrupt. Contrast with
/// `select_deletable_jobs` which requires `running_builds == 0`
/// from ListExecutors.
// r[verify ctrl.pool.manifest-failed-sweep+2]
#[test]
fn failed_jobs_swept_without_idle_check() {
    let cap = FAILED_SWEEP_MIN; // 5 failed < cap → all selected
    let mut jobs: Vec<Job> = (0..5).map(|i| failed_job(&format!("failed-{i}"))).collect();
    // Active: status unset → neither Failed nor Complete. These are
    // live pods; sweeping them would orphan running builds.
    jobs.extend((0..3).map(|i| named_job(&format!("active-{i}"), (8 * GI, 2000))));

    let swept = select_failed_jobs(&jobs, cap);

    assert_eq!(
        swept.len(),
        5,
        "all 5 Failed Jobs swept — no idle-grace wait, no ListExecutors \
         RPC needed. The pod already crashed; delete is unconditional."
    );
    for j in &swept {
        assert!(
            j.metadata.name.as_deref().unwrap().starts_with("failed-"),
            "only Failed Jobs selected; active Jobs left for the \
             reapable pass. Got: {:?}",
            j.metadata.name
        );
    }
}

/// 30 Failed Jobs, cap=20 → exactly 20 selected. A day-long
/// crash-loop at 10s/tick leaves 8640 Failed Jobs; firing 8640
/// deletes in one tick would spike apiserver write load right when
/// the operator just fixed the image. Bounded-per-tick means the
/// backlog clears gradually. The cap is a caller param (computed
/// as `max(FAILED_SWEEP_MIN, replicas.max)`) — this test asserts
/// the truncation mechanics; convergence math is in
/// `failed_sweep_cap_tracks_replicas_max`.
#[test]
fn failed_sweep_bounded_per_tick() {
    let jobs: Vec<Job> = (0..30)
        .map(|i| failed_job(&format!("failed-{i}")))
        .collect();

    let swept = select_failed_jobs(&jobs, FAILED_SWEEP_MIN);

    assert_eq!(
        swept.len(),
        FAILED_SWEEP_MIN,
        "sweep bounded to cap even with 30 Failed Jobs present. \
         Next tick re-lists and sweeps the next batch."
    );
    assert!(
        swept.len() <= FAILED_SWEEP_MIN,
        "bound invariant: swept ≤ cap always"
    );
}

/// CONVERGENCE PROOF: the sweep cap must be ≥ `replicas.max`,
/// otherwise a full crash-loop (every `headroom`-worth fails per
/// tick) accumulates net `+(max − cap)/tick` forever. P0511's fixed
/// cap=20 diverged for `max ≥ 20`.
///
/// Reconciler computes `cap = max(FAILED_SWEEP_MIN, replicas.max)`.
/// Small pools get the floor; large pools scale to their own
/// ceiling. Net accumulation ≤ 0 in both regimes.
// r[verify ctrl.pool.manifest-failed-sweep+2]
#[test]
fn failed_sweep_cap_tracks_replicas_max() {
    // Small pool: floor dominates. A max=5 pool still sweeps 20/tick
    // — clears historical backlog faster than it accumulates.
    assert_eq!(FAILED_SWEEP_MIN.max(5_usize), 20);
    // Large pool: replicas.max dominates. This is the load-bearing
    // case — cap=50 means the sweep can clear a full 50-headroom
    // tick's worth of crashes. P0511 would have returned 20 here.
    assert_eq!(FAILED_SWEEP_MIN.max(50_usize), 50);

    // 50 Failed Jobs, cap=50 → all selected (not clamped to 20).
    // This is the convergence guarantee: sweep keeps up with a
    // max=50 pool's worst-case accumulation.
    let failed: Vec<Job> = (0..50).map(|i| failed_job(&format!("f-{i}"))).collect();
    let selected = select_failed_jobs(&failed, 50);
    assert_eq!(
        selected.len(),
        50,
        "large-pool cap not artificially low — all 50 swept in one tick"
    );

    // Same 50 Failed, cap=20 (small-pool floor) → bounded.
    let selected_small = select_failed_jobs(&failed, 20);
    assert_eq!(selected_small.len(), 20);
}

/// Boundary table for sweep_cap. The `.max(0)` clamp (row 3) guards
/// i32→usize wrap; the `replicas.max > FAILED_SWEEP_MIN` case (row 5)
/// is the ceiling-tracks-spawn-rate property that VM tests never hit
/// (small pools only). Row 4 is the P0511-divergence canary: if
/// .max() → .min() at the call site, `sweep_cap(4)` → 4 not 20.
// r[verify ctrl.pool.manifest-failed-sweep+2]
#[test]
fn sweep_cap_boundaries() {
    assert_eq!(FAILED_SWEEP_MIN, 20, "doc-const sync");
    // (replicas_max, expected, what-it-proves)
    #[rustfmt::skip]
    let cases = [
        (0,        20,                 "zero → floor"),
        (-1,       20,                 "negative clamped (i32→usize wrap guard)"),
        (i32::MIN, 20,                 "MIN clamped"),
        (4,        20,                 "small pool → floor (P0511 canary: .min typo → 4)"),
        (20,       20,                 "equal to floor"),
        (100,      100,                "ceiling tracks spawn rate — VM never hits this"),
        (i32::MAX, i32::MAX as usize,  "MAX passes through"),
    ];
    for (replicas_max, want, why) in cases {
        assert_eq!(sweep_cap(replicas_max), want, "{why}");
    }
}

/// T2 regression: sort before truncate. Without this, `list()`-order
/// `.take(cap)` may keep skipping the same old Job forever — it
/// never sorts to the front of an apiserver-arbitrary list.
#[test]
fn failed_sweep_oldest_first_under_backlog() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::{SignedDuration, Timestamp};

    let now = Timestamp::now();
    let mut jobs: Vec<Job> = (0..5).map(|i| failed_job(&format!("f-{i}"))).collect();
    // f-2 is the oldest (2 days ago), f-0 is newest. List order
    // ≠ age order — this simulates apiserver-arbitrary iteration.
    jobs[0].metadata.creation_timestamp = Some(Time(now));
    jobs[1].metadata.creation_timestamp = Some(Time(now - SignedDuration::from_secs(3600)));
    jobs[2].metadata.creation_timestamp = Some(Time(now - SignedDuration::from_secs(2 * 86400)));
    jobs[3].metadata.creation_timestamp = Some(Time(now - SignedDuration::from_secs(7200)));
    jobs[4].metadata.creation_timestamp = Some(Time(now - SignedDuration::from_secs(86400)));

    // cap=1: only the single oldest survives truncate.
    let selected = select_failed_jobs(&jobs, 1);
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].metadata.name.as_deref(),
        Some("f-2"),
        "oldest (f-2, 2 days ago) selected; NOT list-first (f-0, now). \
         Without sort-before-truncate, f-2 might never be swept."
    );

    // cap=3: oldest three, in age order.
    let selected3 = select_failed_jobs(&jobs, 3);
    let names: Vec<&str> = selected3
        .iter()
        .map(|j| j.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["f-2", "f-4", "f-3"],
        "sweep order is strictly oldest-first (2d, 1d, 2h)"
    );
}

/// REGRESSION GUARD: Failed Jobs don't count toward replicas.max.
///
/// This is the crash-loop amplifier's OTHER half: with max=5, 3
/// active, 10 Failed, the reconciler must compute budget=2 (5-3),
/// NOT budget=0 (5-13 saturated). Failed Jobs are invisible to the
/// ceiling check — correct behavior (they're not supply), but it
/// means the ceiling doesn't cap crash-loop accumulation. The sweep
/// handles Failed separately; this test locks in the ceiling
/// arithmetic so a future refactor doesn't accidentally "fix" it by
/// counting Failed toward the cap (which would break legitimate
/// replacement spawns).
#[test]
fn failed_jobs_excluded_from_ceiling() {
    // 3 active (status unset → not Complete, not Failed).
    let mut jobs: Vec<Job> = (0..3)
        .map(|i| named_job(&format!("active-{i}"), (8 * GI, 2000)))
        .collect();
    // 10 Failed. These are NOT supply.
    jobs.extend((0..10).map(|i| failed_job(&format!("failed-{i}"))));

    // The reconciler's active_jobs filter (manifest.rs inventory
    // pass): status.succeeded == 0 AND status.failed == 0.
    let active_jobs: Vec<&Job> = jobs
        .iter()
        .filter(|j| {
            let s = j.status.as_ref();
            s.and_then(|st| st.succeeded).unwrap_or(0) == 0
                && s.and_then(|st| st.failed).unwrap_or(0) == 0
        })
        .collect();

    let active_total: i32 = active_jobs.len().try_into().unwrap();
    let ceiling: i32 = 5; // spec.replicas.max
    let headroom = ceiling.saturating_sub(active_total).max(0) as usize;

    assert_eq!(
        active_total, 3,
        "Failed Jobs filtered from active count. 13 Jobs total, \
         10 Failed → active_total=3. If this were 13, the headroom \
         would saturate to 0 and replacement spawns would never fire."
    );
    assert_eq!(
        headroom, 2,
        "budget = max(5) - active(3) = 2. The 10 Failed Jobs don't \
         appear in this arithmetic at all — swept separately."
    );
}

/// Tier boundaries for CrashLoopDetected event dedup. K8s collapses
/// events by (reason, message); stable tier → stable message →
/// apiserver increments event.count instead of emitting a new event
/// every tick. Tier transitions (3→10, 10→50) emit a fresh event,
/// which is correct: escalation IS news.
#[test]
fn crash_loop_tier_stable_within_bucket() {
    // Base tier: threshold..10.
    assert_eq!(crash_loop_tier(CRASH_LOOP_WARN_THRESHOLD), "3+");
    assert_eq!(crash_loop_tier(9), "3+");
    // Mid tier: 10..50.
    assert_eq!(crash_loop_tier(10), "10+");
    assert_eq!(crash_loop_tier(49), "10+");
    // High tier: 50+.
    assert_eq!(crash_loop_tier(50), "50+");
    assert_eq!(crash_loop_tier(8640), "50+"); // day-long crash-loop

    // Stability WITHIN a tier is the dedup invariant: 6 Failed at
    // tick T, 7 at T+1 (sweep raced re-create) → same "3+" tier →
    // same message → K8s bumps event.count instead of a new event.
    assert_eq!(
        crash_loop_tier(6),
        crash_loop_tier(7),
        "within-tier stability: count fluctuation doesn't change the \
         message, so K8s can dedup across ticks"
    );
}

// r[verify ctrl.pool.manifest-failed-sweep+2]
#[test]
fn sweep_ordered_before_spawn_in_source() {
    // STRUCTURAL GUARD: the Failed-Job sweep MUST appear before the
    // spawn loop in reconcile_manifest's body. A return-on-spawn-error
    // can't skip a sweep that already ran. This test greps the source
    // — brittle to refactoring, but that brittleness is the point:
    // anyone reordering these sections trips this test and has to
    // consciously decide the ordering is still safe.
    //
    // Quota-deadlock scenario this guards: ResourceQuota on
    // count/jobs.batch exhausted by Failed Jobs → spawn 403 →
    // pre-reorder: return Err skipped sweep → can't clear quota.
    // The reconciler could not recover without `kubectl delete`.
    let src = include_str!("../manifest.rs");
    let sweep_marker = "---- Sweep Failed Jobs FIRST ----";
    let spawn_marker = "---- Spawn ----";
    let sweep_pos = src
        .find(sweep_marker)
        .expect("sweep section comment present");
    let spawn_pos = src
        .find(spawn_marker)
        .expect("spawn section comment present");
    assert!(
        sweep_pos < spawn_pos,
        "Failed-Job sweep at byte {sweep_pos} MUST precede spawn at \
         byte {spawn_pos}. Reordering spawn-before-sweep reintroduces \
         the quota-exhaustion deadlock (spawn 403 → early return → \
         sweep skipped → quota never clears).",
    );
}

#[test]
fn spawn_loop_no_early_return_on_error() {
    // The Failed arm at the try_spawn_job match is warn+continue for
    // <N consecutive fails, not unconditional return. grep for the
    // specific `return Err(e.into())` that was the deadlock line —
    // it should be gone from spawn_manifest_jobs. Slices from the
    // helper's `fn` keyword to `reconcile_manifest` (next function)
    // so a `return Err` elsewhere doesn't false-positive.
    //
    // P0522: the `return Err(Error::Kube(e))` inside the helper is
    // the threshold bail (SPAWN_FAIL_THRESHOLD consecutive fails) —
    // intentional. The pre-P0516 bail was `return Err(e.into())`
    // on the FIRST failure; this test still guards that specific
    // pattern (which the threshold preserves as NOT the behavior).
    let src = include_str!("../manifest.rs");
    let helper_start = src
        .find("fn spawn_manifest_jobs(")
        .expect("spawn_manifest_jobs helper present");
    let helper_end = src[helper_start..]
        .find("fn reconcile_manifest(")
        .map(|i| i + helper_start)
        .expect("reconcile_manifest follows spawn_manifest_jobs");
    let helper_body = &src[helper_start..helper_end];
    assert!(
        !helper_body.contains("return Err(e.into())"),
        "spawn helper must warn+continue on single create error, not \
         bail — pre-P0516 unconditional bail skipped the idle-reapable \
         pass + status patch"
    );
    // Positive: the warn message is there (proves the Failed arm
    // exists and does something observable below threshold).
    assert!(
        helper_body.contains("manifest Job spawn failed; continuing tick"),
        "spawn helper should warn on sub-threshold create error with \
         a grep-able message"
    );
    // P0522 threshold bail IS present — the escalation path.
    assert!(
        helper_body.contains("return Err(Error::Kube(e))"),
        "spawn helper should bail via Error::Kube after \
         SPAWN_FAIL_THRESHOLD consecutive failures"
    );
}
