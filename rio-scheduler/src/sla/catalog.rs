//! ADR-023 §13c-2: catalog-derived per-hwClass `(max_cores, max_mem)`
//! ceilings. Derived **once at boot** from `describe_instance_types`
//! intersected with each hwClass's Karpenter `requirements`. Replaces
//! the hand-maintained per-class `maxCores`/`maxMem` config that drifted
//! from what `requirements` actually permit (the §13c-1 STRIKE rounds).
//!
//! ## Launchability grounding (live_050(d))
//!
//! API existence is NOT launchability. `describe_instance_types` is a
//! self-referential oracle for "can I buy this": the live counter-
//! example is the gen-8 Intel 96xlarge/metal-96xl rows — present in
//! the API, ZERO launchable us-east-2 capacity in EITHER market — that
//! set every hi ceiling to `max_cores=383` and ICE'd every fleet (the
//! 512/704 hang; the hi->od override ICED IDENTICALLY, refuting the
//! market-exhaustion frame). Ceiling candidacy is therefore grounded
//! by EXCLUSION-ONLY NEGATIVE EVIDENCE: the committed, censused
//! `[sla].unlaunchable_sizes` list (helm `karpenter.unlaunchableSizes`)
//! is synthesized as an `instance-size NotIn` requirement for EVERY
//! class inside [`derive_ceilings`] — one mint, so a loose class
//! cannot re-import a phantom into its ceiling or the global
//! (`scheduler.sla.global.derive`). Runtime staleness (a ceiling that
//! shrinks between boots) is the stale-solve revalidation law's axis
//! (`scheduler.sla.ceiling.stale-solve-revalidation`), not this
//! file's.
//!
//! **Category-pin-less classes (merged_bug_134):** the fetch is
//! UNFILTERED — a class whose `requirements` omit the
//! `instance-category` pin matches the full regional catalog
//! (reservation-only u7in-class rows included) with no 0-match warn;
//! its only phantom protection is the class-wide committed
//! `unlaunchable_sizes` exclusion (the fetcher `Gt 5` shape —
//! `fetcher_gt5_class_gets_the_exclusion` pins exactly this
//! population). Keep operator classes category-pinned, or keep the
//! exclusion list current.
//!
//! ## Why not launch-observed
//!
//! The grounding is exclusion-only by design — NEVER a
//! cap-at-largest-observed-launch. Deriving from `CostTable.cells`
//! (Acked instance types) ratchets DOWN:
//! Karpenter launches the cheapest type that fits → first 4c probe →
//! `observed_max(h)=4` → `retain_hosting_cells` strips `h` for any
//! `cores>4` → no large build routes there → no large node launches →
//! never grows. The catalog is launch-independent and available at boot.
//!
//! ## Why scheduler-side (not xtask, not controller)
//!
//! - **Not xtask:** an operator-side step that emits a helm-values
//!   fragment introduces a "rerun after editing `requirements`"
//!   staleness step. The scheduler already has IRSA
//!   `ec2:DescribeInstanceTypes` (alongside `DescribeSpotPriceHistory`,
//!   used by [`super::cost::spot_price_poller`]) and re-derives on every
//!   boot — zero operator step.
//! - **Not controller:** the controller is air-gapped (egress = rio-store
//!   + kube only, no AWS API).
//!
//! ## Data flow
//!
//! `main.rs` calls [`fetch_catalog`] once at boot (Spot cost source
//! only — Static has no AWS API), passes the result to
//! [`derive_ceilings`], and writes the map into
//! [`super::cost::CostTable::set_catalog_ceilings`]. From there it
//! flows through [`super::cost::CostTable::catalog_ceilings`] into
//! [`super::config::SlaConfig::class_ceilings`] (via
//! `solve_intent_for`'s `cost: &CostTable` snapshot) and to the
//! controller via `GetHwClassConfig`. `interrupt_housekeeping`'s
//! lease-acquire reload carries the catalog forward
//! ([`super::cost::CostTable::carry_catalog`]).

use std::collections::{BTreeMap, HashMap};

use aws_sdk_ec2::types::{ArchitectureType, InstanceTypeInfo};
use rio_common::k8s::metal_partition_op;
use tracing::warn;

use super::config::{HwClassDef, NodeSelectorReq};

/// `hw_class → (max_cores, max_mem_bytes)` derived from the AWS
/// instance-type catalog. Empty until [`derive_ceilings`] runs (boot,
/// Spot cost source); always empty under Static. Threaded as a
/// parameter to [`super::config::SlaConfig::class_ceilings`] — empty
/// → the per-class ceiling falls to `cfg.unwrap_or(global)`.
pub type CatalogCeilings = HashMap<String, (u32, u64)>;

/// Karpenter well-known label keys derived per instance type. Values
/// match the `karpenter.k8s.aws/*` labels Karpenter's discovery stamps
/// at launch — the same keys [`HwClassDef::requirements`] selects on.
/// `kubernetes.io/arch` is included so `arch In [amd64]` requirements
/// work without special-casing.
mod label {
    pub const CATEGORY: &str = "karpenter.k8s.aws/instance-category";
    pub const GENERATION: &str = "karpenter.k8s.aws/instance-generation";
    pub const SIZE: &str = "karpenter.k8s.aws/instance-size";
    pub const LOCAL_NVME: &str = "karpenter.k8s.aws/instance-local-nvme";
    pub const CPU_MANUFACTURER: &str = "karpenter.k8s.aws/instance-cpu-manufacturer";
    pub const ARCH: &str = "kubernetes.io/arch";
}

/// One catalog entry: the `(name, cores, mem, labels)` projection the
/// requirements matcher reads. Extracted from `InstanceTypeInfo` by
/// [`from_instance_type_info`]; constructible directly in tests so the
/// matcher is unit-testable without an AWS client.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    pub cores: u32,
    pub mem_bytes: u64,
    pub labels: BTreeMap<&'static str, String>,
}

/// Project an [`InstanceTypeInfo`] onto the Karpenter label map the
/// requirements matcher reads. `None` when the API row is missing the
/// type name, vCPU count, or memory (degenerate response — skip rather
/// than match a `(0, 0)` phantom).
// r[impl scheduler.sla.ceiling.catalog-derived+4]
pub fn from_instance_type_info(it: &InstanceTypeInfo) -> Option<CatalogEntry> {
    let name = it.instance_type()?.as_str().to_owned();
    let cores = it.v_cpu_info()?.default_v_cpus()?;
    if cores <= 0 {
        return None;
    }
    let mem_mib = it.memory_info()?.size_in_mib()?;
    if mem_mib <= 0 {
        return None;
    }
    let labels = karpenter_labels(it, &name);
    Some(CatalogEntry {
        name,
        cores: cores as u32,
        mem_bytes: (mem_mib as u64) << 20,
        labels,
    })
}

/// Derive Karpenter discovery labels for an instance type. Mirrors
/// upstream Karpenter's `instancetype.computeRequirements`:
/// `c8gd.metal-48xl` → category=`c`, generation=`8`, size=`metal-48xl`.
/// The family digits (between the first letter and the `.`) are the
/// generation; trailing letters (`g`, `d`, `n`, `e`, `i`) are family
/// modifiers Karpenter folds into the family but does NOT label
/// separately — `requirements` select on category+generation only.
// r[impl scheduler.sla.ceiling.catalog-derived+4]
fn karpenter_labels(it: &InstanceTypeInfo, name: &str) -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    let (family, size) = name.split_once('.').unwrap_or((name, ""));
    // First letter run = category; first digit run after = generation.
    let category: String = family
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let generation: String = family
        .chars()
        .skip_while(|c| c.is_ascii_alphabetic())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    m.insert(label::CATEGORY, category);
    m.insert(label::GENERATION, generation);
    m.insert(label::SIZE, size.to_owned());
    if let Some(arch) = it
        .processor_info()
        .and_then(|p| p.supported_architectures().iter().find_map(k8s_arch))
    {
        m.insert(label::ARCH, arch.to_owned());
    }
    if let Some(mfr) = it.processor_info().and_then(|p| p.manufacturer()) {
        // Karpenter lower-cases the manufacturer (`Intel` → `intel`).
        m.insert(label::CPU_MANUFACTURER, mfr.to_ascii_lowercase());
    }
    // `instance-local-nvme` is total ephemeral storage in GB (Karpenter
    // uses string-encoded integer for `Gt`/`Lt`). Absent → `0` — a
    // class with `local-nvme Gt 0` then excludes ebs-only types.
    let nvme_gb = it
        .instance_storage_info()
        .and_then(|s| s.total_size_in_gb())
        .unwrap_or(0);
    m.insert(label::LOCAL_NVME, nvme_gb.to_string());
    m
}

fn k8s_arch(a: &ArchitectureType) -> Option<&'static str> {
    match a {
        ArchitectureType::X8664 => Some("amd64"),
        ArchitectureType::Arm64 => Some("arm64"),
        _ => None,
    }
}

/// Evaluate Karpenter `NodeSelectorRequirement` semantics against a
/// derived label map. Operator semantics match
/// `corev1.NodeSelectorOperator` as Karpenter applies them to instance
/// types: `In`/`NotIn` are set membership; `Gt`/`Lt` parse both sides
/// as integers (non-numeric → no match, mirroring Karpenter's strict
/// parse); `Exists`/`DoesNotExist` test key presence. Unknown operator
/// → no match (fail-closed; the catalog ceiling falls to global rather
/// than over-routing on an operator the controller wouldn't accept).
// r[impl scheduler.sla.ceiling.catalog-derived+4]
pub fn requirements_match(
    reqs: &[NodeSelectorReq],
    labels: &BTreeMap<&'static str, String>,
) -> bool {
    reqs.iter().all(|r| {
        let v = labels.get(r.key.as_str());
        match r.operator.as_str() {
            "In" => v.is_some_and(|v| r.values.iter().any(|x| x == v)),
            // k8s `NodeSelectorOperator`: an absent label MATCHES `NotIn`
            // — the exact complement of `In` (which an absent label
            // never satisfies). Reachable: `karpenter_labels` inserts
            // `ARCH` and `CPU_MANUFACTURER` conditionally, so a `NotIn`
            // requirement on either key sees `None` for unmappable arch
            // / unreported manufacturer. `is_some_and` here under-
            // matched (filtered an instance type Karpenter would
            // launch) and silently diverged from the documented mirror.
            "NotIn" => v.is_none_or(|v| !r.values.iter().any(|x| x == v)),
            "Exists" => v.is_some(),
            "DoesNotExist" => v.is_none(),
            "Gt" => num_cmp(v, &r.values).is_some_and(|(a, b)| a > b),
            "Lt" => num_cmp(v, &r.values).is_some_and(|(a, b)| a < b),
            _ => false,
        }
    })
}

/// `(label_value, requirement_value)` parsed as `i64`, or `None` if
/// either side fails to parse or `values` isn't a singleton (Karpenter
/// `Gt`/`Lt` require exactly one value).
fn num_cmp(v: Option<&String>, values: &[String]) -> Option<(i64, i64)> {
    let [b] = values else { return None };
    Some((v?.parse().ok()?, b.parse().ok()?))
}

/// Per-hwClass `(max_cores, max_mem)`: `argmax_t cores` over the matched
/// catalog ∩ requirements ∩ metal-partition set, emitting *that type's*
/// `(cores − 1, mem × 9/10)` — both axes reduced for kubelet
/// `kubeReserved`/`systemReserved`/eviction overhead and Karpenter's
/// `vmMemoryOverheadPercent` (Karpenter binpacks against `Capacity −
/// Overhead`, so a request of the raw capacity on either axis never fits
/// any instance); never an independent per-axis max (which would phantom
/// a `(192c, 1.5TiB)` from disjoint `(192c, 32GiB)` and `(32c, 1.5TiB)`
/// types and ICE-loop Karpenter). All three derive_ceilings axes now
/// reserve allocatable from capacity (cores `−1`, mem `×0.9`, disk
/// `×0.9` in controller.yaml). The shape differs because the overhead
/// scales differently: kubelet's CPU reserve is a near-flat ~110m
/// regardless of vCPU count, so a flat `−1` covers every instance size
/// we ship; mem/disk overhead (`kubeReserved.memory`, Karpenter's
/// `vmMemoryOverheadPercent`, image cache) grows with capacity, so a
/// proportional `×0.9` is needed. ANY future 4th axis (GPU, ephemeral)
/// MUST also apply a margin and MUST decide flat-vs-proportional by
/// whether its kubelet/Karpenter reserve scales with capacity.
///
/// Metal partition mirrors `cover::build_nodeclaim`: `nodeClass ==
/// rio-metal` → `instance-size In metal_sizes`; else `NotIn`. Empty
/// `metal_sizes` → no partition (vmtest). 0-match classes → omitted
/// from the map (operator typo or AWS deprecation; warn) so they fall
/// to the global ceiling and the [`super::metrics`] uncatalogued gauge
/// fires.
// r[impl scheduler.sla.ceiling.catalog-derived+4]
pub fn derive_ceilings(
    catalog: &[CatalogEntry],
    hw_classes: &HashMap<String, HwClassDef>,
    metal_sizes: &[String],
    unlaunchable_sizes: &[String],
) -> CatalogCeilings {
    // live_050(d)/live_051(a): the committed launch-evidence exclusion,
    // synthesized ONCE and applied to EVERY class (the class-wide
    // mint — a loose class whose own requirements omit the row cannot
    // re-import a phantom into its ceiling, and `resolve_globals`'
    // max-over-classes therefore cannot exceed the largest honest
    // ceiling by composition). See the module doc's launchability
    // grounding law.
    let unlaunchable_req = (!unlaunchable_sizes.is_empty()).then(|| NodeSelectorReq {
        key: label::SIZE.into(),
        operator: "NotIn".into(),
        values: unlaunchable_sizes.to_vec(),
    });
    let mut out = CatalogCeilings::new();
    for (h, def) in hw_classes {
        let metal_req = (!metal_sizes.is_empty()).then(|| NodeSelectorReq {
            key: label::SIZE.into(),
            // §Partition-single-source: same predicate as
            // `cover::build_nodeclaim` and `probe_boot::mk_probe_nodeclaim`.
            operator: metal_partition_op(&def.node_class).into(),
            values: metal_sizes.to_vec(),
        });
        let best = catalog
            .iter()
            .filter(|e| requirements_match(&def.requirements, &e.labels))
            .filter(|e| {
                metal_req
                    .as_ref()
                    .is_none_or(|r| requirements_match(std::slice::from_ref(r), &e.labels))
            })
            .filter(|e| {
                unlaunchable_req
                    .as_ref()
                    .is_none_or(|r| requirements_match(std::slice::from_ref(r), &e.labels))
            })
            .max_by_key(|e| (e.cores, e.mem_bytes));
        match best {
            Some(e) => {
                // r40 bug_013: Karpenter binpacks against `Capacity −
                // Overhead`. On nodeadm/AL2023 the CPU reserve is
                // ~60m–150m + eviction threshold, so a request of
                // `default_v_cpus` exact never fits any instance —
                // `Launched=False` → ICE-mask → permanent mint→reap
                // loop. Reserve 1 core (covers kubeReserved +
                // systemReserved + eviction across all instance sizes
                // we ship today; the disk axis already does a `×0.9`
                // margin in controller.yaml). Best-effort tier with an
                // Amdahl fit hits `cap_c == ceiling` exactly (`p̄ =
                // c_opt = ∞`), so this is the default outcome on
                // metal, not an edge case. `.max(1)` floors a future
                // 1-core catalog row at 1 instead of 0.
                //
                // r42 bug_005: mem axis gets the same allocatable-vs-
                // capacity margin (`×0.9` mirroring disk). Without it a
                // memory-heavy build whose `MemFit` p90 lands in
                // `(allocatable, raw_capacity]` for some hwClass h passes
                // `retain_hosting_cells`'s `mem <= class_ceilings(h).1`
                // filter (config.rs) — the request is admitted but no real
                // instance has `allocatable >= request` — same mint→reap
                // loop as the cores axis. The global clamp at
                // `solve_intent_for` (`mem.min(self.sla_ceilings.max_mem)`)
                // can also pin `mem` there when the resolved global equals
                // the largest class's catalog mem (r[scheduler.sla.global.
                // derive]). `evictionHard.memory.available` defaults 100Mi,
                // `kubeReserved.memory` ~ ½–2 GiB depending on instance
                // size, plus the ENA/EBS-volume kernel overhead Karpenter
                // calls `vmMemoryOverhead` (~0.075 of capacity by default).
                // 10% covers the worst case across the catalog. ANY future
                // 4th axis (GPU mem, ephemeral) MUST also apply a margin.
                out.insert(
                    h.clone(),
                    (e.cores.saturating_sub(1).max(1), (e.mem_bytes / 10) * 9),
                );
            }
            None if catalog.is_empty() => {
                // Static cost source / describe_instance_types failed
                // → every class is uncatalogued. `class_ceilings`
                // falls to global (graceful degradation); the per-tick
                // gauge surfaces it.
                warn!(
                    hw_class = %h,
                    "§13c-2: empty AWS catalog (Static cost source or \
                     describe_instance_types failed at boot); ceiling \
                     falls to global for every class."
                );
            }
            None => {
                // sh-016: the catalog has data but THIS class matched
                // zero types — operator typo / nonexistent SKU (gen-7
                // x86 c/m/r local-nvme: c7gd/m7gd/r7gd are arm64-only).
                // `class_ceilings` returns (0,0) → the class fails
                // every size gate and is structurally excluded from
                // emission. error-level: this is a config defect, not
                // a degraded mode.
                tracing::error!(
                    hw_class = %h,
                    "§13c-2: no instance type in the AWS catalog matches \
                     this hwClass's requirements; class is EXCLUDED \
                     (ceiling (0,0)). Check sla.hwClasses.{h}.requirements \
                     against the deployment region's available types."
                );
            }
        }
    }
    out
}

/// Pull the instance-type catalog — UNFILTERED (merged_bug_134): a
/// bare `describe_instance_types` paginator. The FULL regional
/// catalog enters, reservation-only and previous-generation types
/// included; there is NO server-side family or generation filter
/// here (the pre-fix doc promised `c`/`m`/`r` + current-generation —
/// a universe the body never fetched, and one it MUST NOT fetch: the
/// metal doctrine's classes and any operator class pinning another
/// category live outside `c`/`m`/`r`). Phantom protection rests
/// SOLELY on each class's `requirements` intersected with the
/// committed `[sla].unlaunchable_sizes` exclusion — the module
/// header's launchability grounding law; the
/// `w12_ak_catalog_doc_matches_the_unfiltered_body` pin keeps this
/// claim equal to the body in BOTH directions. Best-effort — on API
/// error return empty (every class falls to global, the uncatalogued
/// gauge fires per-class, the operator alerts on it).
pub async fn fetch_catalog(ec2: &aws_sdk_ec2::Client) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    let mut paginator = ec2.describe_instance_types().into_paginator().send();
    while let Some(page) = paginator.next().await {
        match page {
            Ok(p) => {
                for it in p.instance_types() {
                    if let Some(e) = from_instance_type_info(it) {
                        out.push(e);
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "§13c-2: describe_instance_types failed; \
                       per-class catalog ceilings fall to global");
                return Vec::new();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::config::NodeLabelMatch;
    use super::*;

    /// In-memory catalog entry for tests — no AWS client.
    fn ce(name: &str, cores: u32, mem_gib: u64, arch: &str, nvme_gb: i64) -> CatalogEntry {
        let (family, size) = name.split_once('.').unwrap();
        let category: String = family
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let generation: String = family
            .chars()
            .skip_while(|c| c.is_ascii_alphabetic())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let mut labels = BTreeMap::new();
        labels.insert(label::CATEGORY, category);
        labels.insert(label::GENERATION, generation);
        labels.insert(label::SIZE, size.to_owned());
        labels.insert(label::ARCH, arch.to_owned());
        labels.insert(label::LOCAL_NVME, nvme_gb.to_string());
        labels.insert(label::CPU_MANUFACTURER, "amd".to_owned());
        CatalogEntry {
            name: name.into(),
            cores,
            mem_bytes: mem_gib << 30,
            labels,
        }
    }

    fn req(key: &str, op: &str, values: &[&str]) -> NodeSelectorReq {
        NodeSelectorReq {
            key: key.into(),
            operator: op.into(),
            values: values.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn hw(node_class: &str, reqs: Vec<NodeSelectorReq>) -> HwClassDef {
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: "rio.build/hw-band".into(),
                value: "x".into(),
            }],
            requirements: reqs,
            node_class: node_class.into(),
            ..Default::default()
        }
    }

    /// §13c-2 r[verify scheduler.sla.ceiling.catalog-derived+4]: the core
    /// red-first test. `requirements` intersected with the catalog
    /// picks `argmax_t cores` over the **matched** set, not the global
    /// max. A `[c, m]` × gen `[7]` requirement matches
    /// `[c7a.large, m7i.4xlarge]`, NOT `r8g.metal-48xl`; argmax →
    /// `m7i.4xlarge` → `(16 − 1 kubelet reserve, 64GiB × 9/10)`.
    #[test]
    fn requirements_match_picks_argmax_in_matched_set() {
        let catalog = vec![
            ce("c7a.large", 2, 4, "amd64", 0),
            ce("m7i.4xlarge", 16, 64, "amd64", 0),
            ce("r8g.metal-48xl", 192, 1536, "arm64", 0),
        ];
        let classes = HashMap::from([(
            "lo-x86".to_owned(),
            hw(
                "rio-default",
                vec![
                    req(label::CATEGORY, "In", &["c", "m"]),
                    req(label::GENERATION, "In", &["7"]),
                ],
            ),
        )]);
        let out = derive_ceilings(&catalog, &classes, &[], &[]);
        assert_eq!(
            out.get("lo-x86"),
            Some(&(15, (64 << 30) / 10 * 9)),
            "argmax over matched [c7a.large, m7i.4xlarge] minus 1-core kubelet \
             reserve and 10% mem reserve"
        );
    }

    /// R13 (live_050(d) — phantom-catalog ceiling; W7-N): certifies
    /// *a catalog containing an API-existent, never-launchable top
    /// type does NOT become the ceiling* — the committed censused
    /// exclusion (`unlaunchable_sizes`, the rev-3/rev-4 overlay
    /// content as chart defaults) removes it from candidacy at boot.
    /// Pre-fix red (transcript in the commit body): the 96xlarge
    /// phantom set `max_cores=383` — the live boot-log shape.
    /// Kill-isolation: the same catalog WITHOUT the exclusion still
    /// yields the phantom (the violability lane — the exclusion is
    /// config, not a hardcode).
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    #[test]
    fn phantom_top_type_does_not_set_the_ceiling() {
        let catalog = vec![
            // The phantom: exists in describe_instance_types, zero
            // launchable us-east-2 capacity in EITHER market (the
            // live_050 boot-log evidence: max_cores=383 = 384-1).
            ce("r8i.96xlarge", 384, 3072, "amd64", 0),
            // The largest BUYABLE gen-8 c/m/r (family proven live).
            ce("c8a.48xlarge", 192, 384, "amd64", 0),
        ];
        let classes = HashMap::from([(
            "hi-ebs-x86".to_owned(),
            hw(
                "rio-default",
                vec![
                    req(label::CATEGORY, "In", &["c", "m", "r"]),
                    req(label::GENERATION, "In", &["8"]),
                ],
            ),
        )]);
        // The SHIPPED exclusion (values.yaml karpenter.unlaunchableSizes).
        let unlaunchable: Vec<String> = ["96xlarge", "metal-96xl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = derive_ceilings(&catalog, &classes, &[], &unlaunchable);
        assert_eq!(
            out.get("hi-ebs-x86"),
            Some(&(191, (384u64 << 30) / 10 * 9)),
            "the committed launch-evidence exclusion grounds the argmax \
             at the largest LAUNCHABLE type (192-1), not the phantom"
        );
        // Kill-isolation / violability: no exclusion => the phantom
        // argmax returns (the pre-fix left, reproduced on demand).
        let out = derive_ceilings(&catalog, &classes, &[], &[]);
        assert_eq!(
            out.get("hi-ebs-x86"),
            Some(&(383, (3072u64 << 30) / 10 * 9)),
            "violability lane: empty exclusion reproduces the phantom"
        );
    }

    /// R14 (the metalSizes rot; the W7-O census's red row): metal-96xl
    /// absent from the partition list LEAKS through the band classes'
    /// NotIn filter. Pre-fix red (transcript in the commit body): the
    /// band ceiling = the metal-96xl phantom (383).
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    #[test]
    fn metal_96xl_is_partitioned() {
        let catalog = vec![
            ce("c8i.metal-96xl", 384, 768, "amd64", 0),
            ce("c8a.48xlarge", 192, 384, "amd64", 0),
        ];
        let classes = HashMap::from([(
            "hi-ebs-x86".to_owned(),
            hw(
                "rio-default",
                vec![
                    req(label::CATEGORY, "In", &["c", "m", "r"]),
                    req(label::GENERATION, "In", &["8"]),
                ],
            ),
        )]);
        // The SHIPPED partition list (values.yaml karpenter.metalSizes).
        let metal_sizes: Vec<String> = [
            "metal",
            "metal-16xl",
            "metal-24xl",
            "metal-32xl",
            "metal-48xl",
            "metal-96xl",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = derive_ceilings(&catalog, &classes, &metal_sizes, &[]);
        assert_eq!(
            out.get("hi-ebs-x86"),
            Some(&(191, (384u64 << 30) / 10 * 9)),
            "metal-96xl is partitioned OUT of the band classes (NotIn \
             metalSizes) — the author-typed list can no longer omit it"
        );
    }

    /// W7-P (non-regression at the buyable boundary): the committed
    /// exclusion never strips LAUNCHABLE types — a 48xlarge-launchable
    /// family yields the 48xlarge-derived ceiling (191 = 192-1), not a
    /// degraded or zero one, with the exclusion active.
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    #[test]
    fn exclusion_does_not_strip_launchable_types() {
        let catalog = vec![ce("c8a.48xlarge", 192, 384, "amd64", 0)];
        let classes = HashMap::from([(
            "hi-ebs-x86".to_owned(),
            hw(
                "rio-default",
                vec![
                    req(label::CATEGORY, "In", &["c", "m", "r"]),
                    req(label::GENERATION, "In", &["8"]),
                ],
            ),
        )]);
        let unlaunchable: Vec<String> = ["96xlarge", "metal-96xl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = derive_ceilings(&catalog, &classes, &[], &unlaunchable);
        assert_eq!(
            out.get("hi-ebs-x86"),
            Some(&(191, (384u64 << 30) / 10 * 9)),
            "the exclusion is surgical — launchable types untouched"
        );
    }

    /// live_050(e)(iii): the fetcher `instance-generation Gt 5` shape
    /// shares the phantom-match class — its NotIn-metalSizes partition
    /// admits the same 96xl rows into the argmax. The class-wide
    /// committed exclusion MUST cover it (no per-class row needed —
    /// the seam binds every class).
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    #[test]
    fn fetcher_gt5_class_gets_the_exclusion() {
        let catalog = vec![
            ce("r8i.96xlarge", 384, 3072, "amd64", 0),
            ce("c8a.48xlarge", 192, 384, "amd64", 0),
        ];
        let classes = HashMap::from([(
            "fetcher-x86".to_owned(),
            hw("rio-default", vec![req(label::GENERATION, "Gt", &["5"])]),
        )]);
        let metal_sizes: Vec<String> = [
            "metal",
            "metal-16xl",
            "metal-24xl",
            "metal-32xl",
            "metal-48xl",
            "metal-96xl",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let unlaunchable: Vec<String> = ["96xlarge", "metal-96xl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = derive_ceilings(&catalog, &classes, &metal_sizes, &unlaunchable);
        assert_eq!(
            out.get("fetcher-x86"),
            Some(&(191, (384u64 << 30) / 10 * 9)),
            "the Gt-5 class is covered by the class-wide exclusion"
        );
        // Violability (the live pre-rev-4 left): without the committed
        // exclusion the Gt-5 class imports the phantom.
        let out = derive_ceilings(&catalog, &classes, &metal_sizes, &[]);
        assert_eq!(
            out.get("fetcher-x86"),
            Some(&(383, (3072u64 << 30) / 10 * 9)),
            "violability: the loose Gt-5 shape reproduces the live import"
        );
    }

    /// The ceiling-derivation product census (R15): (class-kind ×
    /// catalog-row) cells from the alphabet — band (NotIn partition) /
    /// metal (In partition) / fetcher (Gt-5, partition-NotIn) classes
    /// against phantom and buyable rows of both partitions, with the
    /// shipped metalSizes + exclusion lists. Each class's ceiling MUST
    /// come from its buyable partition row; no cell sees a phantom.
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    #[test]
    fn ceiling_derivation_product_census() {
        let catalog = vec![
            ce("r8i.96xlarge", 384, 3072, "amd64", 0),
            ce("c8i.metal-96xl", 384, 768, "amd64", 0),
            ce("c8a.48xlarge", 192, 384, "amd64", 0),
            ce("c8a.metal-48xl", 192, 384, "amd64", 0),
        ];
        let metal_sizes: Vec<String> = [
            "metal",
            "metal-16xl",
            "metal-24xl",
            "metal-32xl",
            "metal-48xl",
            "metal-96xl",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let unlaunchable: Vec<String> = ["96xlarge", "metal-96xl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let classes = HashMap::from([
            (
                "band".to_owned(),
                hw(
                    "rio-default",
                    vec![
                        req(label::CATEGORY, "In", &["c", "m", "r"]),
                        req(label::GENERATION, "In", &["8"]),
                    ],
                ),
            ),
            (
                "metal".to_owned(),
                hw(
                    rio_common::k8s::METAL_NODE_CLASS,
                    vec![
                        req(label::CATEGORY, "In", &["c", "m", "r"]),
                        req(label::GENERATION, "Gt", &["5"]),
                    ],
                ),
            ),
            (
                "fetcher".to_owned(),
                hw("rio-default", vec![req(label::GENERATION, "Gt", &["5"])]),
            ),
        ]);
        let out = derive_ceilings(&catalog, &classes, &metal_sizes, &unlaunchable);
        // band + fetcher: NotIn partition → 48xlarge (191); metal: In
        // partition → metal-48xl (191). Nobody sees 383.
        for h in ["band", "metal", "fetcher"] {
            assert_eq!(
                out.get(h),
                Some(&(191, (384u64 << 30) / 10 * 9)),
                "{h}: ceiling from the buyable partition row"
            );
        }
    }

    /// `Gt 0` on `instance-local-nvme` excludes ebs-only types; the
    /// nvme-only argmax wins.
    #[test]
    fn local_nvme_gt_zero_excludes_ebs_only() {
        let catalog = vec![
            ce("c8a.48xlarge", 192, 384, "amd64", 0),
            ce("c8gd.24xlarge", 96, 192, "arm64", 5700),
        ];
        let classes = HashMap::from([(
            "nvme-arm".to_owned(),
            hw(
                "rio-nvme",
                vec![
                    req(label::CATEGORY, "In", &["c", "m", "r"]),
                    req(label::ARCH, "In", &["arm64"]),
                    req(label::LOCAL_NVME, "Gt", &["0"]),
                ],
            ),
        )]);
        let out = derive_ceilings(&catalog, &classes, &[], &[]);
        assert_eq!(
            out.get("nvme-arm"),
            Some(&(95, (192u64 << 30) / 10 * 9)),
            "Gt 0 excludes ebs-only c8a.48xlarge; 96c − 1 kubelet reserve, 10% mem reserve"
        );
    }

    /// 0-match → class omitted from the map (warn'd). The caller then
    /// falls to global and the uncatalogued gauge fires.
    #[test]
    fn zero_match_omits_class() {
        let catalog = vec![ce("c7a.large", 2, 4, "amd64", 0)];
        let classes = HashMap::from([(
            "ghost".to_owned(),
            hw(
                "rio-default",
                vec![req(label::CATEGORY, "In", &["nonexistent-family"])],
            ),
        )]);
        let out = derive_ceilings(&catalog, &classes, &[], &[]);
        assert!(out.is_empty(), "0-match class omitted, not (0,0)");
    }

    /// r40 bug_013 floor: a 1-core catalog row yields a `(1, mem)`
    /// ceiling, not `(0, mem)`. Real EC2 types are ≥2 vCPUs so this is
    /// paranoia, but a 0-core ceiling would strip every cell for the
    /// class — fail loud at the floor instead.
    #[test]
    fn one_core_entry_floors_at_one() {
        let catalog = vec![ce("t.tiny", 1, 1, "amd64", 0)];
        let classes = HashMap::from([(
            "tiny".to_owned(),
            hw("rio-default", vec![req(label::CATEGORY, "In", &["t"])]),
        )]);
        let out = derive_ceilings(&catalog, &classes, &[], &[]);
        assert_eq!(
            out.get("tiny"),
            Some(&(1, (1u64 << 30) / 10 * 9)),
            "1-core entry floors at 1 after kubelet reserve, not 0; mem still gets 10% reserve"
        );
    }

    /// Metal partition: `nodeClass == rio-metal` synthesizes
    /// `instance-size In metalSizes`; everything else gets `NotIn`.
    /// Mirrors `cover::build_nodeclaim`'s I-205 partition.
    #[test]
    fn metal_partition_splits_by_node_class() {
        let catalog = vec![
            ce("c8a.48xlarge", 192, 384, "amd64", 0),
            ce("c8a.metal-48xl", 192, 384, "amd64", 0),
            ce("c8a.24xlarge", 96, 192, "amd64", 0),
        ];
        let classes = HashMap::from([
            (
                "ebs-x86".to_owned(),
                hw("rio-default", vec![req(label::CATEGORY, "In", &["c"])]),
            ),
            (
                "metal-x86".to_owned(),
                hw("rio-metal", vec![req(label::CATEGORY, "In", &["c"])]),
            ),
        ]);
        let metal_sizes = vec!["metal".to_owned(), "metal-48xl".to_owned()];
        let out = derive_ceilings(&catalog, &classes, &metal_sizes, &[]);
        // Both pick a real type; the partition determines WHICH one.
        // `cores` ties at 192 — the assertion is on the EXCLUSION
        // (metal-x86 must not pick a non-metal size and vice versa).
        // 192c − 1 kubelet reserve = 191; mem × 9/10.
        let mem = (384u64 << 30) / 10 * 9;
        assert_eq!(out.get("ebs-x86"), Some(&(191, mem)));
        assert_eq!(out.get("metal-x86"), Some(&(191, mem)));
        // With only the metal type in the catalog: ebs-x86 would have
        // 0 matches and metal-x86 picks it.
        let metal_only = vec![ce("c8a.metal-48xl", 192, 384, "amd64", 0)];
        let out = derive_ceilings(&metal_only, &classes, &metal_sizes, &[]);
        assert!(
            !out.contains_key("ebs-x86"),
            "NotIn metalSizes excludes the only type"
        );
        assert_eq!(out.get("metal-x86"), Some(&(191, mem)));
    }

    /// r42 bug_005: the mem reserve is the contract, not a derived
    /// constant. A 64 GiB instance type yields a ceiling of exactly
    /// `(64 << 30) / 10 * 9` = 61_847_529_057 bytes (≈57.6 GiB) —
    /// strictly less than the raw `mem_bytes`. A regression to raw
    /// `e.mem_bytes` is caught here even if the other tests are
    /// mechanically updated to track the implementation.
    #[test]
    fn mem_axis_reserves_ten_percent() {
        let catalog = vec![ce("m7i.4xlarge", 16, 64, "amd64", 0)];
        let classes = HashMap::from([(
            "x86".to_owned(),
            hw("rio-default", vec![req(label::CATEGORY, "In", &["m"])]),
        )]);
        let out = derive_ceilings(&catalog, &classes, &[], &[]);
        let (_, mem) = *out.get("x86").expect("class matched");
        assert_eq!(mem, 61_847_529_057, "(64 << 30) / 10 * 9, integer division");
        assert!(
            mem < (64u64 << 30),
            "mem ceiling must be strictly below raw capacity"
        );
    }

    /// All NodeSelector operators evaluated. `Gt`/`Lt` are numeric;
    /// non-numeric → no match (Karpenter strict parse).
    #[test]
    fn requirements_match_operators() {
        let labels = ce("c8gd.4xlarge", 16, 32, "arm64", 950).labels;
        assert!(requirements_match(
            &[req(label::GENERATION, "In", &["8"])],
            &labels
        ));
        assert!(!requirements_match(
            &[req(label::GENERATION, "NotIn", &["8"])],
            &labels
        ));
        assert!(requirements_match(
            &[req(label::GENERATION, "Gt", &["7"])],
            &labels
        ));
        assert!(!requirements_match(
            &[req(label::GENERATION, "Gt", &["8"])],
            &labels
        ));
        assert!(requirements_match(
            &[req(label::GENERATION, "Lt", &["9"])],
            &labels
        ));
        assert!(requirements_match(
            &[req(label::CATEGORY, "Exists", &[])],
            &labels
        ));
        assert!(!requirements_match(
            &[req("nonexistent", "Exists", &[])],
            &labels
        ));
        assert!(requirements_match(
            &[req("nonexistent", "DoesNotExist", &[])],
            &labels
        ));
        // Non-numeric Gt → no match.
        assert!(!requirements_match(
            &[req(label::CATEGORY, "Gt", &["a"])],
            &labels
        ));
        // Unknown operator → fail-closed (no match).
        assert!(!requirements_match(
            &[req(label::CATEGORY, "Bogus", &["c"])],
            &labels
        ));
        // Multiple Gt values → ill-formed → no match.
        assert!(!requirements_match(
            &[req(label::GENERATION, "Gt", &["7", "6"])],
            &labels
        ));
    }

    /// k8s `corev1.NodeSelectorOperator` semantics (which the
    /// doc-comment claims to mirror): an absent label MATCHES `NotIn`
    /// — `NotIn` is the exact complement of `In`, and `In` on an absent
    /// label is `false`. Reachable: `karpenter_labels` inserts `ARCH`
    /// and `CPU_MANUFACTURER` conditionally, so a `NotIn` requirement
    /// on either key sees `None` for unmappable arch / unreported
    /// manufacturer. Pre-fix `is_some_and` returned `false` (under-
    /// matched, fail-closed but diverged from the documented mirror).
    #[test]
    fn requirements_match_notin_absent_label() {
        let labels = ce("c8gd.4xlarge", 16, 32, "arm64", 950).labels;
        // `In` on an absent key → false (no match).
        assert!(!requirements_match(
            &[req("nonexistent", "In", &["x"])],
            &labels
        ));
        // `NotIn` on an absent key → true (k8s: absent satisfies NotIn).
        assert!(
            requirements_match(&[req("nonexistent", "NotIn", &["x"])], &labels),
            "NotIn must match an absent label key (k8s NodeSelectorOperator semantics)"
        );
        // `NotIn` on a present key still excludes when value ∈ set.
        assert!(!requirements_match(
            &[req(label::CATEGORY, "NotIn", &["c"])],
            &labels
        ));
        // §one-step-removed inverse: `In` must NOT also be `is_none_or`
        // — present-and-matching only.
        assert!(requirements_match(
            &[req(label::CATEGORY, "In", &["c"])],
            &labels
        ));
    }

    /// Family letter/digit parser: `c8gd` → (`c`, `8`); `m7i` →
    /// (`m`, `7`); `t3a` → (`t`, `3`).
    #[test]
    fn family_parser_handles_modifier_suffixes() {
        let e = ce("c8gd.metal-48xl", 192, 384, "arm64", 5700);
        assert_eq!(e.labels[label::CATEGORY], "c");
        assert_eq!(e.labels[label::GENERATION], "8");
        assert_eq!(e.labels[label::SIZE], "metal-48xl");
        let e = ce("m7i.4xlarge", 16, 64, "amd64", 0);
        assert_eq!(e.labels[label::CATEGORY], "m");
        assert_eq!(e.labels[label::GENERATION], "7");
    }
    // r[verify scheduler.sla.ceiling.catalog-derived+4]
    /// **W12-AK (merged_bug_134; the doc arm, made falsifiable)** —
    /// *the documented universe equals the fetched universe, or the
    /// doc says it doesn't and the backstop is pinned.* The pre-fix
    /// doc promised c/m/r families + a current-generation filter over
    /// a body with ZERO `.filters()` calls — unfalsifiable by CI, and
    /// the campaign's launchability-exclusion audit reasoned against
    /// that false universe. The doc now states the fetch is
    /// UNFILTERED with requirements ∩ unlaunchable_sizes as the only
    /// backstop (`fetcher_gt5_class_gets_the_exclusion` pins the
    /// un-pinned-class population); THIS pin holds the claim equal to
    /// the body in BOTH directions — landing a server-side filter
    /// without re-deriving the doc and the grounding header REDs here.
    #[test]
    fn w12_ak_catalog_doc_matches_the_unfiltered_body() {
        let src = include_str!("catalog.rs");
        let prod = src
            .split_once("#[cfg(test)]\nmod tests")
            .map_or(src, |(p, _)| p);
        assert_eq!(
            prod.matches(".filters(").count(),
            0,
            "fetch_catalog is documented UNFILTERED — a filter landed; \
             re-derive the doc and the launchability grounding header \
             with it (merged_bug_134)"
        );
        assert!(
            prod.contains("UNFILTERED (merged_bug_134)"),
            "the doc carries the truthful universe claim"
        );
    }
}
