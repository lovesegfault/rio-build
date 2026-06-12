//! The container-memory footprint law — the ONE constructed quantity
//! every capacity gate on BOTH sides of the scheduler/controller
//! process boundary compares (bughunt-11 merged_bug_016).
//!
//! Round-10's live_058-a pad (`fix(rio-controller): pad container
//! memory above the solved build size`) minted the constructor in the
//! controller's `intent_pod_footprint` only. The coupled admission
//! predicates kept reading the BARE solve — the scheduler's
//! `retain_hosting_cells` size gate (`mem <= cm`) and the controller's
//! own `fallback_cell` (`i.mem_bytes <= cls_m`) — against the same
//! `GetHwClassConfig`-mirrored ceilings that the provisioning
//! partition (`cover::sizing`) rejects PADDED. That opened a
//! `(ceiling − pad, ceiling]` dead band: admission said yes,
//! provisioning said no, and three dispatch funnels pinned demand
//! exactly at the ceiling — an infinite advisory requeue strand of
//! exactly the largest builds, silently replacing the designed
//! bounded at-cap poison terminal.
//!
//! The law (`ctrl.pool.gate-superset`): a numeric pad straddling a
//! process boundary is not in-process-sealable — both processes MUST
//! consume the SAME shared constant and compare the SAME constructed
//! quantity. This module is that shared home: a leaf-crate constant
//! pair plus the forward map (solve → container) and its inverse
//! (ceiling → max hostable solve), related by the adjunction pinned
//! in [`tests::adjunction_forward_inverse`]. The scheduler's
//! admission gates and dispatch clamps, the controller's
//! `fallback_cell` predicate, and the controller's footprint
//! constructor (`intent_pod_footprint`) all route here; the
//! cross-process contract test in `rio-controller` renders both
//! sides' gate quantities from these functions and goes red on any
//! per-side constant reintroduction.
//!
//! The pad/floor R17 derivation basis (incident pins, violability
//! axes, and the re-derive-from-RSS-telemetry measurement note) lives
//! with the values below — moved verbatim from the controller-local
//! consts this home replaces.

/// Round-10 live_058-a (HIGH): the worker's RESIDENT overhead pad on
/// the container-mem axis — the rio-builder daemon + FUSE client +
/// log capture that live INSIDE the container the k8s limit binds,
/// on top of the solved BUILD size. The solve sizes the BUILD; the
/// limit binds the CONTAINER (k8s `memory.max` is set at the
/// delegated POD level — the per-build sub-cgroup carries no limit
/// of its own), so a warm tiny fit (~45-69 MB solved, the live
/// incident specimens) raw-stamped as request==limit landed BELOW
/// the worker's own baseline and the kernel OOM-killed the whole
/// container before/regardless of the build — the live_058 2.75h
/// same-size requeue loop. Derivation basis (recorded per the A1
/// duty): the incident pins baseline > 69 MB (those containers
/// died); the pad covers daemon RSS + FUSE client structures + log
/// capture with headroom — 256 MiB is the HYPOTHESIS value carried
/// from the incident review, VIOLABLE (R17, all axes): size = the
/// pad itself per pod (the cost of never under-housing the worker);
/// cost = 256 MiB × pods/node of billable mem; population = every
/// builder/fetcher container; time N/A. Measurement note: re-derive
/// from worker-baseline RSS telemetry once a soak window exists —
/// the consts are the knob, [`container_mem_bytes`] is the law.
pub const WORKER_MEM_OVERHEAD_BYTES: u64 = 256 << 20;

/// The container-mem FLOOR (live_058-a): no container renders below
/// this regardless of how tiny the solve is — tiny solves carry the
/// same resident worker. 512 MiB = pad + the sub-pad solve band with
/// headroom (the incident's 45-69 MB solves land here). VIOLABLE
/// (R17): same axes as the pad; the floor only binds when
/// `solved + pad < floor`, i.e. solves under 256 MiB.
pub const CONTAINER_MEM_MIN_BYTES: u64 = 512 << 20;

// r[impl ctrl.pool.gate-superset]
// r[impl ctrl.pool.container-overhead+2]
/// The forward map: solved BUILD mem → the container mem a pod for
/// that solve will actually request. This is the constructed quantity
/// of the merged_bug_016 law —
/// `max(solved + WORKER_MEM_OVERHEAD_BYTES, CONTAINER_MEM_MIN_BYTES)`
/// — and EVERY predicate that decides mem feasibility against a class
/// ceiling compares `container_mem_bytes(solve) <= ceiling`, never
/// the bare solve. Monotone and total; saturates at `u64::MAX`.
#[must_use]
pub const fn container_mem_bytes(solved_mem_bytes: u64) -> u64 {
    let padded = solved_mem_bytes.saturating_add(WORKER_MEM_OVERHEAD_BYTES);
    if padded < CONTAINER_MEM_MIN_BYTES {
        CONTAINER_MEM_MIN_BYTES
    } else {
        padded
    }
}

// r[impl ctrl.pool.gate-superset]
// r[impl sys.liveness.exit-edge]
/// The inverse map: a class/global mem ceiling → the LARGEST solved
/// mem whose [`container_mem_bytes`] still fits under it, or `None`
/// when the ceiling cannot host any container at all
/// (`ceiling < CONTAINER_MEM_MIN_BYTES` — config validation refuses
/// such ceilings; the `None` arm is the fail-closed backstop).
///
/// Dispatch funnels that pin demand at a ceiling (the at-cap floor
/// doubling cap, the global dispatch clamp, the stale-solve re-solve
/// clamp) MUST pin at `max_hostable_solve_mem(ceiling)` so their
/// output renders a container of exactly `ceiling` — hostable — and
/// the designed bounded at-cap retry terminal stays reachable. The
/// adjunction with the forward map
/// (`container_mem_bytes(s) <= c  ⟺  s <= max_hostable_solve_mem(c)`)
/// is what makes the admission/provisioning dead band EMPTY by
/// construction; it is pinned by `adjunction_forward_inverse` below.
#[must_use]
pub const fn max_hostable_solve_mem(ceiling_bytes: u64) -> Option<u64> {
    if ceiling_bytes < CONTAINER_MEM_MIN_BYTES {
        None
    } else {
        // ceiling >= 512 MiB > pad, so the subtraction cannot wrap and
        // the floor branch of the forward map is satisfied at the
        // result.
        Some(ceiling_bytes - WORKER_MEM_OVERHEAD_BYTES)
    }
}

// r[impl ctrl.pool.gate-superset]
/// The band-boundary population for the cross-process gate contract
/// tests — [GEN-SET]: rendered from the shared maps (never hand-typed
/// per side), so the scheduler-side and controller-side
/// gate-equals-law witnesses quantify over the SAME cells. For a
/// hosting ceiling the knife edge is `cap' = max_hostable_solve_mem`
/// (the largest admissible solve) and `cap' + 1` (the first refused
/// one); the pre-merged_bug_016 dead band was exactly
/// `(ceiling − pad, ceiling]` = `(cap', ceiling]`, with `ceiling`
/// itself the value the dispatch funnels pinned. A per-side constant
/// reintroduced on either gate flips the knife-edge cells — the
/// contract tests go red there first.
#[must_use]
pub fn band_boundary_cells(ceiling_bytes: u64) -> Vec<u64> {
    match max_hostable_solve_mem(ceiling_bytes) {
        // Non-hosting ceiling: every solve refuses; probe zero, the
        // ceiling itself, and one past it.
        None => vec![0, ceiling_bytes, ceiling_bytes.saturating_add(1)],
        Some(cap) => vec![
            0,
            cap / 2,
            cap.saturating_sub(1),
            cap,
            cap + 1,
            ceiling_bytes,
            ceiling_bytes.saturating_add(1),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forward map IS the live_058-a container law, verbatim:
    /// pad-additive above the floor band, floored below it.
    #[test]
    fn forward_map_is_the_container_law() {
        // Sub-floor solves (the incident's 45-69 MB band) floor at
        // CONTAINER_MEM_MIN_BYTES.
        assert_eq!(container_mem_bytes(0), CONTAINER_MEM_MIN_BYTES);
        assert_eq!(container_mem_bytes(69 << 20), CONTAINER_MEM_MIN_BYTES);
        // At and above the floor knee the pad is additive.
        assert_eq!(
            container_mem_bytes(256 << 20),
            (256 << 20) + WORKER_MEM_OVERHEAD_BYTES
        );
        assert_eq!(
            container_mem_bytes(64 << 30),
            (64 << 30) + WORKER_MEM_OVERHEAD_BYTES
        );
        // Total: saturates instead of wrapping.
        assert_eq!(container_mem_bytes(u64::MAX), u64::MAX);
    }

    // r[verify ctrl.pool.gate-superset]
    /// **W11-Z (adjunction leg)** — *proposition: the forward and
    /// inverse maps form a Galois adjunction on every ceiling that can
    /// host a container:*
    /// `container_mem_bytes(s) <= c ⟺ s <= max_hostable_solve_mem(c)`.
    /// This is the algebra that makes the admission/provisioning dead
    /// band empty: a gate comparing the forward map and a funnel
    /// pinning at the inverse can never disagree by a band cell.
    /// Population: ceilings at the floor knee, the ×0.9-margin shape,
    /// and the boundary cells `cap'`, `cap'+1` per ceiling.
    #[test]
    fn adjunction_forward_inverse() {
        let ceilings = [
            CONTAINER_MEM_MIN_BYTES,
            CONTAINER_MEM_MIN_BYTES + 1,
            1 << 30,
            (64u64 << 30) / 10 * 9, // the derive_ceilings ×0.9 shape
            64 << 30,
            512 << 30,
        ];
        for c in ceilings {
            let cap = max_hostable_solve_mem(c).expect("ceiling >= container floor hosts");
            // The pinned maximum renders exactly within the ceiling…
            assert!(
                container_mem_bytes(cap) <= c,
                "container({cap}) > ceiling {c}"
            );
            // …and one byte more does not: the band above the pin is
            // empty.
            assert!(
                container_mem_bytes(cap + 1) > c,
                "container({}) <= ceiling {c} — dead band re-opened",
                cap + 1
            );
        }
        // Fail-closed arm: a ceiling below the container floor hosts
        // nothing — there is no solve to pin at.
        assert_eq!(max_hostable_solve_mem(CONTAINER_MEM_MIN_BYTES - 1), None);
        assert_eq!(max_hostable_solve_mem(0), None);
    }
}
