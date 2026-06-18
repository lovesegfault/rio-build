//! k8s/nix interop helpers shared across the workspace.

/// `EC2NodeClass` name whose hw-classes are partitioned to bare-metal
/// instance sizes (the I-205 BIOS-AMI partition). Both
/// `cover::build_nodeclaim` (controller), `catalog::derive_ceilings`
/// (scheduler), and `probe_boot::mk_probe_nodeclaim` (xtask) gate
/// `karpenter.k8s.aws/instance-size In/NotIn metalSizes` on
/// `node_class == this` — see [`metal_partition_op`].
pub const METAL_NODE_CLASS: &str = "rio-metal";

/// The §13c metal-partition predicate. A hw-class with `node_class ==
/// `[`METAL_NODE_CLASS`] gets the `In` side of the
/// `karpenter.k8s.aws/instance-size` requirement; every other class
/// gets `NotIn`. Total over the partition: there is no third side —
/// the `metalSizes` list either selects (metal) or excludes (everything
/// else). Adding a third partition (e.g. a separate large-metal class)
/// requires a new return variant here, which forces every caller —
/// `cover::build_nodeclaim`, `catalog::derive_ceilings`,
/// `probe_boot::mk_probe_nodeclaim`, and helm
/// `templates/karpenter.yaml`'s `nodePools` loop — to handle it.
pub fn metal_partition_op(node_class: &str) -> &'static str {
    if node_class == METAL_NODE_CLASS {
        "In"
    } else {
        "NotIn"
    }
}

/// The fetcher partition feature. Every FOD intent requires `[fetcher]`;
/// every fetcher hwClass provides `[fetcher]`. The bidirectional ∅-guard
/// in [`features_compatible`] makes this a strict partition: a featureless
/// builder cannot route to a fetcher cell (`[] ⊆ [fetcher]` fails the
/// ∅-guard), a FOD cannot route to a builder cell (`[fetcher] ⊆ []`
/// fails the subset check). See `r[sched.sla.fod-feature-derivation]`.
pub const FETCHER_FEATURE: &str = "fetcher";

/// The fetcher node taint AND label key (one key for both, mirroring the
/// metal pattern: `rio.build/kvm` is both `metal-*`'s taint key and label
/// key). Pre-§13e the static `rio-fetcher` NodePool used a separate
/// `rio.build/node-role: fetcher` label; §13e unified the key so
/// `taints_routing_to(FETCHER_TAINT_KEY)` and `cells_to_selector_terms`
/// read the SAME key from the hwClass config.
pub const FETCHER_TAINT_KEY: &str = "rio.build/fetcher";

/// The builder node taint key, sibling of [`FETCHER_TAINT_KEY`]. Unlike
/// the fetcher key (which §13e unified into both taint AND label key),
/// this is taint-only — builder node *labels* are `cover::NODE_ROLE_LABEL`
/// (`rio.build/node-role: builder`), not this key.
/// Stamped on every cover-minted builder NodeClaim by
/// `cover::builder_taint()` (so non-builder cluster pods stay off
/// builder nodes — ADR-019); tolerated by `pod::effective_tolerations`'s
/// builder arm (r38 bug_027). The pair-coupling invariant is the same
/// as the fetcher case: the key the cover writes MUST match the key the
/// toleration reads, byte-for-byte, or the pod is permanently Pending.
/// One const for both ends so drift is a compile error.
///
/// NOT in hwClass config (cover.rs has a TODO to single-source ALL
/// role taints/labels through hwClass config; this const is the
/// `taints_routing_to(BUILDER_TAINT_KEY)` lookup key that close needs).
pub const BUILDER_TAINT_KEY: &str = "rio.build/builder";

/// Map a single nix `system` (e.g. `"x86_64-linux"`) to its
/// `kubernetes.io/arch` label value. `None` for empty/`builtin`/
/// unknown — caller treats an unmappable system as undroppable (no node
/// can host it). Same arch table as the per-Pool
/// `nix_systems_to_k8s_arch` (I-098); single-string here because
/// `SpawnIntent.system` is scalar. Shared by the controller's FFD
/// `agnostic_arch` path and the scheduler's bypass-path `--capacity`
/// arch-match.
pub fn system_to_k8s_arch(system: &str) -> Option<&'static str> {
    match system.split_once('-').map_or(system, |(a, _)| a) {
        "x86_64" | "i686" => Some("amd64"),
        "aarch64" | "armv7l" | "armv6l" => Some("arm64"),
        _ => None,
    }
}

// r[impl sched.sla.hwclass.provides.bidir]
/// Bidirectional ∅-guard feature-match predicate. Single source for
/// the §13c/D3/D10/I-181 routing rule — open-coding this at ≥4 sites
/// (scheduler `solve_intent_for` `h_all` partition, `retain_hosting_cells`
/// chokepoint, controller `fallback_cell` / FFD `simulate` agnostic
/// backstop, scheduler `compute_spawn_intents` request filter) lets
/// them drift, and drift here is "kvm intent routed to non-kvm cell"
/// or "metal node absorbs non-kvm build".
///
/// `true` iff every `required` feature is in `provides` AND
/// `required.is_empty() == provides.is_empty()`. The second clause is
/// the bidirectional guard: a class providing `[kvm]` rejects
/// featureless intents (so metal doesn't absorb non-kvm — `[]⊆anything`
/// is vacuously true, so the subset check alone would let it through);
/// a class providing `[]` rejects `[kvm]` intents (so non-metal isn't
/// picked for kvm — already rejected by the subset check, the ∅-guard
/// is redundant in this direction). Subset (not
/// equality) on the populated side keeps the door open for
/// `provides=[kvm, big-parallel]` hosting `required=[kvm]`.
///
/// §13d (r30 mb_012): moved from `rio-scheduler::sla::config` to
/// `rio-common::k8s` so the controller's `nodeclaim_pool` consumer-side
/// backstop (`fallback_cell`, FFD `simulate` agnostic filter) shares
/// the same predicate as the scheduler's producer chokepoint —
/// "placement ⊇ provisioning" requires both sides to agree.
pub fn features_compatible(required: &[String], provides: &[String]) -> bool {
    required.iter().all(|f| provides.contains(f)) && required.is_empty() == provides.is_empty()
}

/// bug_063 (R25): the shared typed capacity-term decoder — ONE decode
/// law for the `(hw_class_names, node_affinity)` wire grammar,
/// consumed by the scheduler's `decode_capacity_requirement` and the
/// controller's `cells_of_checked` (single-site hardening did not
/// survive cross-crate template reuse; the law now has one home).
pub mod capacity_term;

/// live_056-b: the builder's serving-state file — the contract between
/// `rio-builder` (writes it once `connect_upstreams` succeeds:
/// post-connect, pre-first-pull) and the controller's Job spec (mints
/// an exec readiness probe testing it), shared here so neither side
/// can drift (the merged_bug_035 shared-constant law). Pod Ready ⟺
/// the builder is past cold start and asking for work; a
/// policy-blackholed builder never creates it and stays NotReady for
/// exactly the un-served window. `/tmp` is writable for BOTH executor
/// kinds (builders: writable rootfs; fetchers: the `tmp` tmpfs mount
/// in `READ_ONLY_ROOT_MOUNTS`).
pub const BUILDER_SERVING_STATE_FILE: &str = "/tmp/rio-serving";

// ── pod ephemeral-storage denomination (bug_065, R33'(ii)) ──────────
// ONE shared mint for "the quota" across the controller/scheduler
// seam. The controller stamps the overlays emptyDir sizeLimit AND the
// pod ephemeral-storage request/limit from these fns
// (pool/jobs.rs::apply_intent_resources). kubelet's DESIRED quota
// size for the volume is min(pod ephemeral limit, emptyDir sizeLimit)
// (k8s 1.33 desired_state_of_world.go AddPodToVolume — the sizeLimit
// side here, always smaller: the pod limit adds fuse + log on top) —
// BUT at the deployed minors AssignQuota writes the NON-ENFORCING
// sentinel instead of the desired size (quota_linux.go: `fsbytes :=
// ibytes; if fsbytes > 0 { fsbytes = -1 }` — "when enforcing quotas
// are enabled, we'll condition this"; the project quota tracks usage
// for eviction, it does not enforce), so the worker-visible hard
// limit is the sentinel (reads ~u64::MAX) and the DiskFull lane is
// DORMANT from kubelet-assigned quotas. The vm-kubelet-projquota
// denomination cells pin that sentinel at the deployed-adjacent
// minor: if kubelet ever starts enforcing the desired size, the cells
// red and this contract activates. [`overlay_size_limit_bytes`] is
// therefore the denomination of (a) the kubelet EVICTION accounting
// (sizeLimit overshoot — pod-attributed eviction), and (b) any
// ENFORCED-quota world (the vm-quota-probe manual-limit harness; a
// future enforcing kubelet); the scheduler's disk-axis trust band
// (actor/floor.rs::observe_peaks) consumes the SAME fn — quantifier: census(disk_four_caller_census) — with the
// [`DISK_HEADROOM_MIN`]/[`DISK_HEADROOM_MAX`] codomain bounds, and
// REFUSES sentinel-armed claims by construction — the two sides
// cannot drift into different denominations without this file
// changing (the §Simulator-shares-accounting law one seam up;
// `footprint::container_mem_bytes` is the mem-axis precedent).

/// Reserved bytes for build logs + nix-daemon state living OUTSIDE
/// the overlay (stdout/stderr capture lands on the container fs).
/// 1 GiB headroom; part of [`pod_ephemeral_request_bytes`], never of
/// the overlay sizeLimit.
pub const LOG_BUDGET_BYTES: u64 = 1 << 30;

/// The disk-headroom codomain LOWER bound: the scheduler's
/// variance-aware curve `headroom(n_eff) = 1.25 + 0.7/sqrt(n_eff)`
/// (sla/fit.rs) approaches-but-never-reaches 1.25 from above, and the
/// controller's flat fallback (1.5, pre-ADR-023 skew) sits inside the
/// band. The corroboration band's floor derives from this bound; the
/// curve's conformance is pinned where the band lives
/// (actor/floor.rs tests).
pub const DISK_HEADROOM_MIN: f64 = 1.25;

/// The disk-headroom codomain UPPER bound: `headroom(n_eff)` is
/// monotone decreasing on the clamped domain `n_eff >= 1`, so its
/// maximum is `headroom(1) = 1.25 + 0.7 = 1.95` exactly.
pub const DISK_HEADROOM_MAX: f64 = 1.95;

/// kubelet rounds the assigned project quota UP to fs quota blocks
/// (1 KiB units in the XFS ioctl); consumers comparing a read-back
/// `hard_limit_bytes` against [`overlay_size_limit_bytes`] allow this
/// much slack above the stamped value. 4 KiB covers any fs block
/// size in the fleet.
pub const KUBELET_QUOTA_BLOCK_SLACK: u64 = 4096;

// The band is non-degenerate and brackets the controller's flat
// fallback (1.5) — compile-time, so a band edit that orphans the
// fallback cannot build.
const _: () = assert!(DISK_HEADROOM_MIN < 1.5 && 1.5 < DISK_HEADROOM_MAX);

/// The overlays emptyDir sizeLimit for a solved disk axis:
/// `disk_bytes x headroom` (the solve-axis quota denomination — what
/// kubelet enforces on the overlay project and what the worker's
/// exhaustion telemetry reports as `hard_limit_bytes`).
pub fn overlay_size_limit_bytes(disk_bytes: u64, headroom: f64) -> u64 {
    (disk_bytes as f64 * headroom) as u64
}

/// Pod `ephemeral-storage` request/limit: the overlay component plus
/// the FUSE-cache emptyDir budget plus [`LOG_BUDGET_BYTES`]. The
/// controller-side wrapper (`pool/jobs.rs::pod_ephemeral_request`)
/// delegates here; helm's `14-disk-ceiling.sh` mirrors the arithmetic
/// by content (its rows are pinned by the controller's
/// `disk_four_caller_census`).
pub fn pod_ephemeral_request_bytes(disk_bytes: u64, headroom: f64, fuse_cache_bytes: u64) -> u64 {
    overlay_size_limit_bytes(disk_bytes, headroom)
        .saturating_add(fuse_cache_bytes)
        .saturating_add(LOG_BUDGET_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metal_partition_op_in_for_metal() {
        assert_eq!(metal_partition_op("rio-metal"), "In");
        assert_eq!(metal_partition_op("rio-default"), "NotIn");
        assert_eq!(metal_partition_op("rio-nvme"), "NotIn");
        assert_eq!(metal_partition_op(""), "NotIn");
    }

    #[test]
    fn system_to_arch_mapping() {
        assert_eq!(system_to_k8s_arch("x86_64-linux"), Some("amd64"));
        assert_eq!(system_to_k8s_arch("i686-linux"), Some("amd64"));
        assert_eq!(system_to_k8s_arch("aarch64-linux"), Some("arm64"));
        assert_eq!(system_to_k8s_arch("armv7l-linux"), Some("arm64"));
        assert_eq!(system_to_k8s_arch("builtin"), None);
        assert_eq!(system_to_k8s_arch(""), None);
        assert_eq!(system_to_k8s_arch("riscv64-linux"), None);
    }

    /// §13c T1b: bidirectional ∅-guard. Single canonical predicate for
    /// D3/D10/I-181 routing — open-coding at ≥4 sites lets them drift.
    // r[verify sched.sla.hwclass.provides.bidir]
    #[test]
    fn features_compatible_bidirectional_guard() {
        let s = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| (*s).into()).collect() };
        // Both empty → compatible.
        assert!(features_compatible(&[], &[]));
        // Exact match → compatible.
        assert!(features_compatible(&s(&["kvm"]), &s(&["kvm"])));
        // required=[], provides=[kvm] → INcompatible (∅-guard: metal
        // must not absorb non-kvm; []⊆anything is vacuously true so
        // the subset check alone would let it through).
        assert!(!features_compatible(&[], &s(&["kvm"])));
        // required=[kvm], provides=[] → INcompatible (subset check:
        // non-metal must not host kvm; ∅-guard redundant here).
        assert!(!features_compatible(&s(&["kvm"]), &[]));
        // Subset on populated side → compatible (provides=[kvm,bp]
        // hosts required=[kvm]).
        assert!(features_compatible(&s(&["kvm"]), &s(&["kvm", "bp"])));
        // required ⊄ provides → incompatible.
        assert!(!features_compatible(&s(&["kvm", "bp"]), &s(&["kvm"])));
        // bug_007: nixos-test + kvm both required, class provides only
        // kvm → unroutable. Surfaced metal `providesFeatures: [kvm]`
        // missing `nixos-test`.
        assert!(!features_compatible(
            &s(&["kvm", "nixos-test"]),
            &s(&["kvm"])
        ));
    }

    /// bug_065: the one-mint identities. The ephemeral request is
    /// EXACTLY overlay + fuse + log (no hidden terms — helm's
    /// 14-disk-ceiling mirror and the controller census both assume
    /// this decomposition), the overlay component is the plain
    /// product, and the headroom band consts bracket the fallback.
    #[test]
    fn pod_ephemeral_decomposes_into_overlay_fuse_log() {
        let gi = 1u64 << 30;
        for disk in [gi, 3 * gi, 100 * gi] {
            for h in [DISK_HEADROOM_MIN, 1.5, DISK_HEADROOM_MAX] {
                assert_eq!(
                    pod_ephemeral_request_bytes(disk, h, 50 * gi),
                    overlay_size_limit_bytes(disk, h) + 50 * gi + LOG_BUDGET_BYTES,
                );
                assert_eq!(overlay_size_limit_bytes(disk, h), (disk as f64 * h) as u64);
            }
        }
        // Saturation, not overflow, at the absurd end.
        assert_eq!(
            pod_ephemeral_request_bytes(u64::MAX, 1.0, u64::MAX),
            u64::MAX
        );
    }

    /// §13e: the fetcher partition is total under [`features_compatible`]'s
    /// ∅-guard. Asserts the const value composes with the predicate the
    /// way the scheduler/controller route on it: `[fetcher]` cannot land
    /// on `[]` cells and `[]` cannot land on `[fetcher]` cells, so a FOD
    /// always goes to a fetcher cell and a non-FOD never does. The
    /// taint-key partition (`FETCHER_TAINT_KEY` vs `rio.build/kvm`) is
    /// asserted at chart-render in `helm/20-fetcher-feature-routing.sh`.
    #[test]
    fn fetcher_partition_is_total() {
        // Mirror the `s` helper convention from
        // `features_compatible_bidirectional_guard` above.
        let s = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| (*s).into()).collect() };
        // The ∅-guard makes this a strict partition: featureless ⇎ [fetcher].
        assert!(!features_compatible(&[], &s(&[FETCHER_FEATURE])));
        assert!(!features_compatible(&s(&[FETCHER_FEATURE]), &[]));
        assert!(features_compatible(
            &s(&[FETCHER_FEATURE]),
            &s(&[FETCHER_FEATURE])
        ));
        // FODs never need kvm, kvm builds never need fetcher.
        assert!(!features_compatible(
            &s(&[FETCHER_FEATURE]),
            &s(&["kvm", "nixos-test"])
        ));
    }
}
