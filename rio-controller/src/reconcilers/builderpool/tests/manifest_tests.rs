//! Manifest-diff reconciler unit tests.
//!
//! Covers the PURE pieces of `manifest.rs`: `compute_spawn_plan`
//! (diff logic), `group_by_bucket` (manifest → demand map),
//! `bucket_labels` ↔ `parse_bucket_from_labels` round-trip, and
//! `build_manifest_job` spec shape. No K8s apiserver interaction —
//! the reconcile-loop wiring (`reconcile_manifest` I/O) is what
//! VM tests cover.
//!
//! Plan 503 T4.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::ObjectMeta;
use rio_proto::types::DerivationResourceEstimate;

use crate::crds::builderpool::{Replicas, Sizing};
use crate::fixtures::test_sched_addrs;
use crate::reconcilers::builderpool::manifest::{
    Bucket, CPU_CLASS_LABEL, FLOOR_CLASS, MEMORY_CLASS_LABEL, SIZING_LABEL, SIZING_MANIFEST,
    SpawnDirective, bucket_labels, build_manifest_job, compute_spawn_plan, group_by_bucket,
    inventory_by_bucket, parse_bucket_from_labels, truncate_plan,
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
    assert!(spec.ttl_seconds_after_finished.is_some());
    assert_eq!(
        spec.active_deadline_seconds, None,
        "manifest pods are long-lived — no deadline (wrong-pool-spawn \
         isn't a concern for demand-driven buckets)"
    );

    let pod_spec = spec.template.spec.as_ref().unwrap();
    assert_eq!(pod_spec.restart_policy.as_deref(), Some("Never"));

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
