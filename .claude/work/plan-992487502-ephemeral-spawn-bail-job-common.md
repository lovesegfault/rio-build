# Plan 992487502: extract spawn-error arm to job_common — ephemeral sibling has the pre-P0516 bail

[`ephemeral.rs:226`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs) has the EXACT `Err(e) => return Err(e.into())` that [P0516](plan-0516-manifest-quota-deadlock.md) removed from [`manifest.rs:357`](../../rio-controller/src/reconcilers/builderpool/manifest.rs). Both files were rewritten together over [`job_common.rs`](../../rio-controller/src/reconcilers/builderpool/job_common.rs) at [`ea64f7f2`](https://github.com/search?q=ea64f7f2&type=commits); both already call `spawn_prerequisites` from job_common. Consolidator at mc=75 surfaced this.

**Same consequence:** one spawn error aborts the batch AND skips `patch_job_pool_status` at [`ephemeral.rs:242`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs). P0516's commit message — "one spawn error should not skip unrelated work" — applies verbatim. The quota-deadlock angle is weaker for ephemeral (no Failed-Job sweep — TTL-based cleanup), but the principle stands.

**Why extract instead of inline-fix:** [P0522](plan-0522-warn-continue-escalation.md) (UNIMPL, on frontier) adds a consecutive-fail threshold + metric to `manifest.rs` ONLY. If P0522 lands first, ephemeral is still bailing and the threshold exists in one sibling. If THIS plan lands first and extracts to `job_common`, P0522's threshold goes in one place and both reconcilers get it. ~40L helper, 2 callers, both already depend on the module.

origin=consolidator. No collision risk with in-flight p518/p519 (neither touches `ephemeral.rs`).

## Entry criteria

- [P0516](plan-0516-manifest-quota-deadlock.md) merged (warn+continue pattern exists at `manifest.rs:371-385` to extract) — **DONE**

## Tasks

### T1 — `fix(controller):` extract `try_spawn_job` to job_common with warn+continue

The match shape at [`manifest.rs:360-386`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) (post-P0516 warn+continue) and [`ephemeral.rs:211-227`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs) (pre-P0516 bail) differ only in the `Err(e)` arm and the `info!` fields. Extract the common structure.

NEW in [`job_common.rs`](../../rio-controller/src/reconcilers/builderpool/job_common.rs), after `spawn_prerequisites` (natural neighbor at `:90-103`):

```rust
/// Outcome of a single `jobs_api.create` attempt. Caller decides
/// what to do on `Failed` — manifest counts toward a consecutive-fail
/// threshold (P0522); ephemeral just loops.
pub(super) enum SpawnOutcome {
    Spawned,
    /// 409 AlreadyExists — name collision (random-suffix collision,
    /// concurrent reconcile). Next tick picks a fresh name. Not
    /// worth propagating — would trigger error_policy backoff.
    NameCollision,
    /// Spawn failed (quota blip, admission webhook, apiserver flap).
    /// NOT a bail — P0516 [`33424b8a`]: one spawn error shouldn't
    /// skip the rest of the tick. Subsequent spawns may succeed;
    /// the status patch below is independent. Caller logs `warn!`
    /// with its own context (bucket, queued, ceiling, etc).
    Failed(kube::Error),
}

/// Create a Job, classifying the outcome. Shared by manifest +
/// ephemeral spawn loops — both had the same match arm at ea64f7f2;
/// manifest got warn+continue at P0516 (33424b8a), ephemeral didn't.
/// Extracting here means both get it, and P0522's threshold lives
/// in one place.
pub(super) async fn try_spawn_job(
    jobs_api: &Api<Job>,
    job: &Job,
) -> SpawnOutcome {
    match jobs_api.create(&PostParams::default(), job).await {
        Ok(_) => SpawnOutcome::Spawned,
        Err(kube::Error::Api(ae)) if ae.code == 409 => SpawnOutcome::NameCollision,
        Err(e) => SpawnOutcome::Failed(e),
    }
}
```

**Call sites:** both spawn loops become:

```rust
// manifest.rs :~360, inside the directive loop:
match try_spawn_job(&jobs_api, &job).await {
    SpawnOutcome::Spawned => {
        info!(pool = %name, job = %job_name, bucket = ?directive.bucket,
              "spawned manifest Job");
    }
    SpawnOutcome::NameCollision => {
        debug!(pool = %name, job = %job_name, "Job name collision; will retry");
    }
    SpawnOutcome::Failed(e) => {
        // P0516 warn+continue. P0522 will add consecutive-fail
        // threshold + metric HERE (or inside try_spawn_job if
        // the counter is sharable between callers — prefer caller-
        // side so per-reconciler thresholds can differ).
        warn!(pool = %name, job = %job_name, bucket = ?directive.bucket,
              error = %e, "manifest Job spawn failed; continuing tick");
    }
}

// ephemeral.rs :~211, inside the for _ in 0..to_spawn loop:
match try_spawn_job(&jobs_api, &job).await {
    SpawnOutcome::Spawned => {
        info!(pool = %name, job = %job_name, queued, active, ceiling,
              "spawned ephemeral Job");
    }
    SpawnOutcome::NameCollision => {
        debug!(pool = %name, job = %job_name, "Job name collision; will retry");
    }
    SpawnOutcome::Failed(e) => {
        // Was `return Err(e.into())` — THE bug. Matches pre-P0516
        // manifest.rs. Now warn+continue: status patch at :242
        // runs regardless; next tick retries.
        warn!(pool = %name, job = %job_name, queued, active, ceiling,
              error = %e, "ephemeral Job spawn failed; continuing tick");
    }
}
```

**Design note — why an enum, not a `Result`:** `SpawnOutcome::Failed` is NOT an error the caller propagates — it's a classified non-success the caller handles inline. A `Result<SpawnSuccess, kube::Error>` with a `?` at the call site would re-introduce the bail. The enum forces exhaustive handling.

**P0522 integration point:** when P0522 adds the threshold, it can either (a) wrap `try_spawn_job` with a `SpawnTracker` struct that holds `consecutive_fails`, or (b) keep the counter caller-side (in the spawn loop). Prefer (b) — lets manifest and ephemeral have different thresholds if ever needed, and keeps `try_spawn_job` stateless. Leave a `// TODO(P0522): threshold counter` at the manifest `Failed` arm.

### T2 — `test(controller):` ephemeral spawn-fail doesn't skip status patch

The bug this fixes: `Err(e) => return Err(e.into())` at `:226` means `patch_job_pool_status` at `:242` never runs on spawn failure. Observable via status staleness.

```rust
// r[verify ctrl.pool.ephemeral]
#[tokio::test]
async fn ephemeral_spawn_fail_still_patches_status() {
    // Mock jobs_api.create: returns Err (quota exceeded) for all calls.
    // Pool: queued=3, active=0, ceiling=5 → to_spawn=3.
    //
    // Pre-fix: first create Err → return Err → status never patched.
    //   `.status.replicas` stays at whatever the previous tick wrote.
    // Post-fix: 3 warns logged, patch_job_pool_status called,
    //   `.status.replicas` = 0 (active), `.status.desiredReplicas` = 5.
    //
    // Assert: (a) reconcile returns Ok (not Err), (b) status patch
    //   was called (mock intercept or check the wp object's status).
}
```

**Shares mock infra with** [P0522](plan-0522-warn-continue-escalation.md) T2 — both need `jobs_api.create` to return `Err`. If P0522 builds `MockJobsApi` first, reuse it. If this plan lands first, put `MockJobsApi` in a `tests/common/` module so P0522 picks it up.

### T3 — `refactor(controller):` drop dead match-arm comment at ephemeral :205-210

The existing comment at [`:205-210`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs) ("SSA's patch-merge semantics don't fit — ... A 409 AlreadyExists ... is retried next tick") is good context for the 409 arm. Move/condense it onto `SpawnOutcome::NameCollision`'s doc-comment in job_common. Don't leave a duplicate at the call site.

## Exit criteria

- `/nbr .#ci` green
- `grep 'return Err(e.into())' rio-controller/src/reconcilers/builderpool/ephemeral.rs` → 0 hits in the spawn loop (the bail is gone)
- `grep 'SpawnOutcome\|try_spawn_job' rio-controller/src/reconcilers/builderpool/job_common.rs` → ≥4 hits (enum + 3 variants + fn)
- `grep 'try_spawn_job' rio-controller/src/reconcilers/builderpool/manifest.rs rio-controller/src/reconcilers/builderpool/ephemeral.rs` → ≥2 hits (both callers)
- `cargo nextest run -p rio-controller ephemeral_spawn_fail_still_patches_status` → passes
- Mutation: re-introduce `return Err(e.into())` at ephemeral Failed arm → T2's test FAILS (status not patched)
- `grep 'TODO(P0522)' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥1 hit (threshold integration point marked)
- [P0522](plan-0522-warn-continue-escalation.md) plan doc updated: T1's code block now targets the `SpawnOutcome::Failed` arm (not the raw `Err(e)` arm). Files fence updated to include `job_common.rs` if threshold goes there. — **coordinate at P0522 dispatch**

## Tracey

References existing markers:
- `r[ctrl.pool.ephemeral]` ([`controller.md:62`](../../docs/src/components/controller.md)) — T2's test verifies ephemeral reconcile behavior. The spawn-error-handling refinement is under the existing spec; spec says "spawns K8s Jobs" and doesn't specify error-handling granularity, same as P0522's relationship to `r[ctrl.pool.manifest-reconcile]`.
- `r[ctrl.pool.manifest-reconcile]` ([`controller.md:137`](../../docs/src/components/controller.md)) — T1 touches the manifest call site but doesn't change behavior (P0516 already established warn+continue; this just refactors the match into a helper).

No new markers — this is a cross-cutting refactor under two existing markers. The shared helper itself (`job_common`) isn't spec-visible.

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/job_common.rs", "action": "MODIFY", "note": "T1: NEW SpawnOutcome enum + try_spawn_job fn after :103 spawn_prerequisites. T3: move :205-210 comment here as NameCollision doc-comment"},
  {"path": "rio-controller/src/reconcilers/builderpool/ephemeral.rs", "action": "MODIFY", "note": "T1: :211-227 match → try_spawn_job call. THE FIX — :226 bail becomes warn+continue. T3: drop :205-210 comment (moved to job_common)"},
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: :360-386 match → try_spawn_job call. No behavior change (warn+continue preserved). TODO(P0522) marker at Failed arm. HOT — P0522 touches :357, P0520 touches :131/:228 (diff sections); P0295-T498/T494 touch :235/:724; P0304-T531 touches :680"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs", "action": "MODIFY", "note": "T2: ephemeral_spawn_fail_still_patches_status. MockJobsApi may go to tests/common/ if shared with P0522-T2"},
  {"path": ".claude/work/plan-0522-warn-continue-escalation.md", "action": "MODIFY", "note": "T1 exit-criterion follow-through: P0522-T1 code block retargets SpawnOutcome::Failed arm. Files fence adds job_common.rs if threshold lives there. COORDINATE at P0522 dispatch"}
]
```

```
rio-controller/src/reconcilers/builderpool/
├── job_common.rs          # T1: +SpawnOutcome +try_spawn_job
├── ephemeral.rs           # T1: :211-227 → try_spawn_job (THE FIX)
├── manifest.rs            # T1: :360-386 → try_spawn_job (refactor-only)
└── tests/ephemeral_tests.rs  # T2: status-patch test
.claude/work/plan-0522-*.md   # P0522 retarget (coordinate)
```

## Dependencies

```json deps
{"deps": [516], "soft_deps": [522], "note": "P0516 (DONE) introduced the manifest warn+continue to extract. SOFT-BLOCKS P0522: prefer this lands FIRST so P0522's threshold goes in one place (job_common or manifest's Failed arm) instead of manifest-only with ephemeral still bailing. If P0522 lands first: this plan extracts a threshold that only exists in one sibling — doable but messier. P0522 is UNIMPL on frontier; coordinator should sequence."}
```

**Depends on:** [P0516](plan-0516-manifest-quota-deadlock.md) (DONE) — the warn+continue arm at `manifest.rs:371-385` is what T1 extracts.

**Soft-blocks:** [P0522](plan-0522-warn-continue-escalation.md) (UNIMPL) — prefer this lands FIRST. P0522 adds threshold+metric to `manifest.rs` only; if the spawn-error arm is already in `job_common`, the threshold goes in one place and ephemeral gets it for free. If P0522 lands first, this plan extracts a threshold-carrying arm from one sibling that the other lacks — works, but the diff is larger and the ephemeral threshold question is open.

**Conflicts with:** `manifest.rs` is HOT. P0522 touches `:357` (SAME region as T1's `:360-386` extraction — **SERIALIZE, don't parallel**). P0520 touches `:131`/`:228`, P0295-T498 touches `:235`, P0295-T494 touches `:724-726`, P0304-T531 touches `:680` — all different sections, clean rebase. `ephemeral.rs` and `job_common.rs` are low-traffic (no top-30 collision entries). `ephemeral_tests.rs` may not exist yet — NEW if so.
