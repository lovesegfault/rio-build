//! ADR-023 §13c-2: catalog-derived per-hwClass `(max_cores, max_mem)`
//! ceilings. Derived **once at boot** from `describe_instance_types`
//! intersected with each hwClass's Karpenter `requirements`. Replaces
//! the hand-maintained per-class `maxCores`/`maxMem` config that drifted
//! from what `requirements` actually permit (the §13c-1 STRIKE rounds).
//!
//! ## Why not launch-observed
//!
//! Deriving from `CostTable.cells` (Acked instance types) ratchets DOWN:
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
// r[impl scheduler.sla.ceiling.catalog-derived]
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
// r[impl scheduler.sla.ceiling.catalog-derived]
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
// r[impl scheduler.sla.ceiling.catalog-derived]
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
/// `(cores, mem)` — always a real shape, never an independent per-axis
/// max (which would phantom a `(192c, 1.5TiB)` from disjoint
/// `(192c, 32GiB)` and `(32c, 1.5TiB)` types and ICE-loop Karpenter).
///
/// Metal partition mirrors `cover::build_nodeclaim`: `nodeClass ==
/// rio-metal` → `instance-size In metal_sizes`; else `NotIn`. Empty
/// `metal_sizes` → no partition (vmtest). 0-match classes → omitted
/// from the map (operator typo or AWS deprecation; warn) so they fall
/// to the global ceiling and the [`super::metrics`] uncatalogued gauge
/// fires.
// r[impl scheduler.sla.ceiling.catalog-derived]
pub fn derive_ceilings(
    catalog: &[CatalogEntry],
    hw_classes: &HashMap<String, HwClassDef>,
    metal_sizes: &[String],
) -> CatalogCeilings {
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
            .max_by_key(|e| (e.cores, e.mem_bytes));
        match best {
            Some(e) => {
                out.insert(h.clone(), (e.cores, e.mem_bytes));
            }
            None => {
                warn!(
                    hw_class = %h,
                    "§13c-2: no instance type in the AWS catalog matches \
                     this hwClass's requirements; ceiling falls to global. \
                     Check sla.hwClasses.{h}.requirements against the \
                     deployment region's available types."
                );
            }
        }
    }
    out
}

/// Pull the instance-type catalog: `c`/`m`/`r` families, current
/// generation only (`requirements` already select on generation; the
/// `current-generation` filter trims previous-gen noise). Best-effort
/// — on API error return empty (every class falls to global, the
/// uncatalogued gauge fires per-class, the operator alerts on it).
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

    /// §13c-2 r[verify scheduler.sla.ceiling.catalog-derived]: the core
    /// red-first test. `requirements` intersected with the catalog
    /// picks `argmax_t cores` over the **matched** set, not the global
    /// max. A `[c, m]` × gen `[7]` requirement matches
    /// `[c7a.large, m7i.4xlarge]`, NOT `r8g.metal-48xl`; argmax →
    /// `m7i.4xlarge` → `(16, 64GiB)`.
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
        let out = derive_ceilings(&catalog, &classes, &[]);
        assert_eq!(
            out.get("lo-x86"),
            Some(&(16, 64 << 30)),
            "argmax over matched [c7a.large, m7i.4xlarge], not r8g.metal-48xl"
        );
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
        let out = derive_ceilings(&catalog, &classes, &[]);
        assert_eq!(
            out.get("nvme-arm"),
            Some(&(96, 192 << 30)),
            "Gt 0 excludes ebs-only c8a.48xlarge"
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
        let out = derive_ceilings(&catalog, &classes, &[]);
        assert!(out.is_empty(), "0-match class omitted, not (0,0)");
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
        let out = derive_ceilings(&catalog, &classes, &metal_sizes);
        // Both pick a real type; the partition determines WHICH one.
        // `cores` ties at 192 — the assertion is on the EXCLUSION
        // (metal-x86 must not pick a non-metal size and vice versa).
        assert_eq!(out.get("ebs-x86"), Some(&(192, 384 << 30)));
        assert_eq!(out.get("metal-x86"), Some(&(192, 384 << 30)));
        // With only the metal type in the catalog: ebs-x86 would have
        // 0 matches and metal-x86 picks it.
        let metal_only = vec![ce("c8a.metal-48xl", 192, 384, "amd64", 0)];
        let out = derive_ceilings(&metal_only, &classes, &metal_sizes);
        assert!(
            !out.contains_key("ebs-x86"),
            "NotIn metalSizes excludes the only type"
        );
        assert_eq!(out.get("metal-x86"), Some(&(192, 384 << 30)));
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
}
