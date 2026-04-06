# Remediation 01: Poisoned-orphan spurious Succeeded

**Parent:** [`phase4a.md` §1.1](../phase4a.md#11-scheduler-recovery-spurious-succeeded-on-poisoned-orphan)
**Severity:** P0 — incorrect results (spurious Succeeded), no alerting, silent until `nix copy` at use site
**Findings:** 10 (sched-poisoned-orphan-spurious-succeeded, sched-id-to-hash-missing-poisoned, sched-recovery-comment-lies, sched-reap-collateral-poisoned, sched-cancel-loop-crash-window, sched-transition-build-inmem-first, pg-poisoned-status-without-poisoned-at, sched-check-build-completion-zero-zero, sched-recovery-bd-rows-continue, sched-stderr-last-empty-built-outputs)

---

## Entry-route map

Three independent routes deposit the same corrupt PG state; one keystone fix renders all three harmless at recovery time. A fourth finding (reap-collateral) is **steady-state** — no crash needed — and ships in the same atomic PR because its one-line fix is the other half of making poisoned nodes survive a restart cycle.

| Route | Where state goes wrong | PG state left behind | `id_to_hash` gap? | Fixed by |
|---|---|---|---|---|
| 1. Poison-persist crash window | `completion.rs:445→459` (4 awaits incl. rio-store RPC) | drv `status='poisoned'`, build `status='active'` | Yes — poisoned loop doesn't insert | Keystone + atomic-poisoned SQL |
| 2. Cancel-loop crash window | `build.rs:78→146` (per-drv persist, then build persist) | some drvs `status='cancelled'`, build `status='active'` | Yes — Cancelled is in `TERMINAL_STATUSES`, never loaded at all | Keystone alone does NOT fix — see §3.3 |
| 3. In-mem-first PG timeout | `build.rs:414` mutates before `build.rs:442` PG write; PG timeout ≠ crash | build `status='active'` forever; drvs terminal (from completion handlers that DID land) | Yes — terminal drvs filtered by `load_nonterminal_derivations` | Keystone + orphan-guard already present (`recovery.rs:310`); PG-first closes the mint |
| Reap-collateral (no-crash) | `dag/mod.rs:507` reaps ALL empty+terminal, not just newly-emptied | n/a — in-mem only | n/a | `was_interested` predicate |

**Key insight for Route 2:** the keystone fix inserts `poisoned` rows into `id_to_hash`. It does **not** insert `cancelled` rows — those are filtered by `TERMINAL_STATUSES` at `db.rs:25` and have no dedicated load query. Route 2 needs its own fix (§3.3); the keystone makes it Failed-not-Succeeded only for the **poisoned** variant of the crash window, not the cancelled variant.

---

## 1. Atomic PR — keystone + immediate-window closure

One commit. Landing any subset leaves either a still-exploitable window or dead code. Scope:

- `recovery.rs`: insert poisoned `derivation_id` into `id_to_hash`; replace lying comment at the `bd_rows` fallthrough
- `db.rs`: new `persist_poisoned()` combining status+timestamp; drop `AND poisoned_at IS NOT NULL` filter (with `COALESCE` fallback)
- `completion.rs`: two callsites swap `persist_status(Poisoned)+set_poisoned_at` → `persist_poisoned()`; new best-effort wrapper
- `dag/mod.rs`: `remove_build_interest_and_reap` only reaps nodes where `remove(&build_id)` returned `true`

### 1.1 `recovery.rs` — keystone fix

**Current** (`rio-scheduler/src/actor/recovery.rs:119-146`):

```rust
let ttl_secs = crate::state::POISON_TTL.as_secs_f64();
for row in poisoned_rows {
    // Expired-at-load: clear in PG, don't insert. [...]
    if row.elapsed_secs > ttl_secs {
        info!(drv_hash = %row.drv_hash, elapsed_secs = row.elapsed_secs,
              "poison already past TTL at recovery — clearing");
        let hash: crate::state::DrvHash = row.drv_hash.into();
        if let Err(e) = self.db.clear_poison(&hash).await {
            warn!(drv_hash = %hash, error = %e,
                  "clear_poison for expired-at-load failed; next recovery will retry");
        }
        continue;
    }
    let state = match DerivationState::from_poisoned_row(row) {
        Ok(s) => s,
        Err((drv_hash, _)) => {
            warn!(drv_hash = %drv_hash, "invalid poisoned drv_path in PG, skipping");
            continue;
        }
    };
    self.dag.insert_recovered_node(state);
}
```

**Diff** — capture `derivation_id` before `row` is moved into `from_poisoned_row`, mirror the `drv_rows` loop pattern at `recovery.rs:88-105`:

```diff
 let ttl_secs = crate::state::POISON_TTL.as_secs_f64();
 for row in poisoned_rows {
+    let derivation_id = row.derivation_id;
     // Expired-at-load: clear in PG, don't insert. [...]
     if row.elapsed_secs > ttl_secs {
         info!(drv_hash = %row.drv_hash, elapsed_secs = row.elapsed_secs,
               "poison already past TTL at recovery — clearing");
         let hash: crate::state::DrvHash = row.drv_hash.into();
         if let Err(e) = self.db.clear_poison(&hash).await {
             warn!(drv_hash = %hash, error = %e,
                   "clear_poison for expired-at-load failed; next recovery will retry");
         }
         continue;
     }
     let state = match DerivationState::from_poisoned_row(row) {
         Ok(s) => s,
         Err((drv_hash, _)) => {
             warn!(drv_hash = %drv_hash, "invalid poisoned drv_path in PG, skipping");
             continue;
         }
     };
+    let hash = state.drv_hash.clone();
+    // r[impl sched.recovery.poisoned-failed-count]
+    // Keystone: without this, the bd_rows join below falls through
+    // on `continue` for poisoned derivations → build.derivation_hashes
+    // stays empty → check_build_completion sees 0/0 → Succeeded.
+    id_to_hash.insert(derivation_id, hash.clone());
     self.dag.insert_recovered_node(state);
 }
```

**Why `derivation_id` capture must precede the expired-at-load branch:** it doesn't strictly need to — the expired branch `continue`s before the insert. But placing it at loop-top matches the `drv_rows` loop structure and eliminates a future-refactor footgun where someone moves the expired check below `from_poisoned_row`.

**Why `let hash = state.drv_hash.clone()` and not `row.drv_hash.clone()`:** `row` has already been moved into `from_poisoned_row`. `state.drv_hash` is the parsed `DrvHash` newtype (correct type for the map value); `row.drv_hash` was a `String`.

### 1.2 `recovery.rs` — replace lying comment

**Current** (`rio-scheduler/src/actor/recovery.rs:167-184`):

```rust
for (build_id, drv_id) in &bd_rows {
    let Some(hash) = id_to_hash.get(drv_id) else {
        // Derivation is terminal (not in our non-terminal
        // load). That's fine — it doesn't need interested_
        // builds tracking (it's done). BuildInfo's
        // derivation_hashes set will be smaller than it was,
        // but check_build_completion only counts non-
        // terminal derivations anyway.
        continue;
    };
    if let Some(state) = self.dag.node_mut(hash) {
        state.interested_builds.insert(*build_id);
    }
    build_drv_hashes
        .entry(*build_id)
        .or_default()
        .insert(hash.clone());
}
```

**Diff:**

```diff
 for (build_id, drv_id) in &bd_rows {
     let Some(hash) = id_to_hash.get(drv_id) else {
-        // Derivation is terminal (not in our non-terminal
-        // load). That's fine — it doesn't need interested_
-        // builds tracking (it's done). BuildInfo's
-        // derivation_hashes set will be smaller than it was,
-        // but check_build_completion only counts non-
-        // terminal derivations anyway.
+        // Derivation is success-terminal (Completed) OR in a
+        // terminal state we don't load (Cancelled,
+        // DependencyFailed). Poisoned IS loaded (separate query
+        // above) and IS in id_to_hash — if we hit this branch
+        // for a poisoned drv, the keystone insert is broken.
+        //
+        // check_build_completion uses build.derivation_hashes.len()
+        // as the denominator. Every drv that falls through here
+        // shrinks that denominator. For a build whose ONLY
+        // remaining drv is here: total=0, completed=0, failed=0
+        // → 0>=0 && 0==0 → spurious Succeeded. The orphan-guard
+        // at :310 below catches total=0; but a build with ONE
+        // Completed drv + ONE fall-through has total=1,
+        // completed=1 → Succeeded, and the guard doesn't fire.
+        // That case is correct iff the fall-through was genuinely
+        // Completed. It is WRONG if the fall-through was
+        // Cancelled (Route 2 in remediation 01). See §3.3.
+        warn!(build_id = %build_id, derivation_id = %drv_id,
+              "bd_row derivation not in id_to_hash — success-terminal or unloaded-terminal");
         continue;
     };
```

**Why `warn!` not `debug!`:** after the keystone fix, this branch is **expected** for Completed derivations (crash-after-last-completion case handled at `recovery.rs:278-319`). The warn is diagnostic noise for that case — but Route 2 (cancelled-orphan) also lands here, and we want that in logs until §3.3 ships. Downgrade to `debug!` in the §3.3 PR once Cancelled is handled.

### 1.3 `db.rs` — combined `persist_poisoned`

**New method** (insert after `set_poisoned_at` at `db.rs:467`, which becomes dead code — delete it in this same PR):

```rust
// r[impl sched.poison.ttl-persist]
/// Atomically set `status='poisoned'` AND `poisoned_at=now()`.
///
/// Replaces the previous two-call sequence (`update_derivation_status`
/// then `set_poisoned_at`) which had a crash window: status='poisoned'
/// but poisoned_at=NULL. Rows in that state were invisible to
/// `load_poisoned_derivations` (filtered by `poisoned_at IS NOT NULL`)
/// — poison TTL tracking silently broken for those rows.
///
/// `assigned_worker_id` is NULLed: a poisoned derivation has no
/// assignment. Matches the in-mem `state.assigned_worker = None` the
/// caller does (actually the caller doesn't — but it should; that's
/// a separate finding).
pub async fn persist_poisoned(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE derivations \
         SET status = 'poisoned', poisoned_at = now(), \
             assigned_worker_id = NULL, updated_at = now() \
         WHERE drv_hash = $1",
    )
    .bind(drv_hash.as_str())
    .execute(&self.pool)
    .await
    .map(|_| ())
}
```

**Delete** `set_poisoned_at` at `db.rs:459-467`. Its only callers are the two sites replaced in §1.5. The roundtrip test at `db.rs:1583-1610` (`test_poisoned_at_roundtrip`) must migrate to `persist_poisoned` — same assertions, one call instead of two.

### 1.4 `db.rs:504` — drop the `poisoned_at IS NOT NULL` filter

**Current** (`db.rs:498-509`):

```rust
sqlx::query_as(
    r#"
    SELECT derivation_id, drv_hash, drv_path, pname, system,
           failed_workers,
           EXTRACT(EPOCH FROM (now() - poisoned_at))::float8 AS elapsed_secs
    FROM derivations
    WHERE status = 'poisoned' AND poisoned_at IS NOT NULL
    "#,
)
```

**Decision: DROP the filter.** Justification:

1. **After §1.3 lands, new rows can never have `status='poisoned' ∧ poisoned_at IS NULL`.** The atomic UPDATE makes that state unreachable via normal operation.
2. **Existing rows in that state are the bug's victims, not garbage.** They were poisoned by `completion.rs:445` and then the scheduler crashed before `:447`. Filtering them out is what CAUSED the spurious Succeeded — the derivation disappears from recovery entirely, the `bd_rows` join falls through, `total` shrinks.
3. **A third query is more code for a state that is now unreachable.** The "treat as poison-now" semantics can be achieved in-query with `COALESCE`.

**But:** dropping the filter alone breaks deserialization. `PoisonedDerivationRow.elapsed_secs: f64` — if `poisoned_at IS NULL`, `EXTRACT(... - NULL)` is `NULL`, and sqlx fails the row. Fix in the same diff:

```diff
 sqlx::query_as(
     r#"
     SELECT derivation_id, drv_hash, drv_path, pname, system,
            failed_workers,
-           EXTRACT(EPOCH FROM (now() - poisoned_at))::float8 AS elapsed_secs
+           COALESCE(
+               EXTRACT(EPOCH FROM (now() - poisoned_at))::float8,
+               0.0
+           ) AS elapsed_secs
     FROM derivations
-    WHERE status = 'poisoned' AND poisoned_at IS NOT NULL
+    WHERE status = 'poisoned'
     "#,
 )
```

`elapsed_secs = 0.0` → `from_poisoned_row` computes `poisoned_at = Instant::now() - 0s = now` → fresh full-TTL. This is **conservative** — a drv that was poisoned 30 minutes ago and then crash-orphaned gets a TTL reset to `now + POISON_TTL` instead of `(crash_time - 30min) + POISON_TTL`. Slight over-poisoning. Acceptable: the alternative (spurious Succeeded) is catastrophic; an extra ~24h of poison is a nuisance.

**Update the doc comment** at `db.rs:485-494` (`/// Rows with poisoned_at IS NULL are pre-009 orphans — skip them`) to describe the new COALESCE semantics.

### 1.5 `completion.rs` — new wrapper + callsite swap

**New best-effort wrapper** (insert after `persist_status` at `completion.rs:30`):

```rust
/// Best-effort atomic persist of `status='poisoned'` + `poisoned_at=now()`.
/// Single SQL UPDATE — no crash window between the two columns.
/// Logs error!, never returns it (same semantics as `persist_status`).
pub(super) async fn persist_poisoned(&self, drv_hash: &DrvHash) {
    if let Err(e) = self.db.persist_poisoned(drv_hash).await {
        error!(drv_hash = %drv_hash, error = %e,
               "failed to persist poisoned status+timestamp");
    }
}
```

**Callsite 1** — `poison_and_cascade` at `completion.rs:445-449`:

```diff
-        self.persist_status(drv_hash, DerivationStatus::Poisoned, None)
-            .await;
-        if let Err(e) = self.db.set_poisoned_at(drv_hash).await {
-            error!(drv_hash = %drv_hash, error = %e, "failed to persist poisoned_at");
-        }
+        self.persist_poisoned(drv_hash).await;
         self.unpin_best_effort(drv_hash).await;
```

**Callsite 2** — `handle_permanent_failure` at `completion.rs:601-605`:

```diff
-        self.persist_status(drv_hash, DerivationStatus::Poisoned, None)
-            .await;
-        if let Err(e) = self.db.set_poisoned_at(drv_hash).await {
-            error!(drv_hash = %drv_hash, error = %e, "failed to persist poisoned_at");
-        }
+        self.persist_poisoned(drv_hash).await;
         self.unpin_best_effort(drv_hash).await;
```

Two awaits → one await. The remaining crash window between in-mem `state.transition(Poisoned)` at `:433`/`:590` and this PG write is the ordinary "in-mem ahead of PG" window that every `persist_status` caller has — recovery already handles it correctly for poisoned (after the keystone fix lands, the poisoned row loads, gets `id_to_hash`, and `build_summary` counts it in `failed`).

### 1.6 `dag/mod.rs` — reap only newly-emptied nodes

**Current** (`rio-scheduler/src/dag/mod.rs:503-519`):

```rust
pub fn remove_build_interest_and_reap(&mut self, build_id: Uuid) -> usize {
    let mut to_reap = Vec::new();

    for (hash, state) in &mut self.nodes {
        state.interested_builds.remove(&build_id);
        if state.interested_builds.is_empty() && state.status().is_terminal() {
            to_reap.push(hash.clone());
        }
    }

    let reaped = to_reap.len();
    for hash in to_reap {
        self.remove_node(&hash);
    }

    reaped
}
```

**Diff:**

```diff
 pub fn remove_build_interest_and_reap(&mut self, build_id: Uuid) -> usize {
     let mut to_reap = Vec::new();

     for (hash, state) in &mut self.nodes {
-        state.interested_builds.remove(&build_id);
-        if state.interested_builds.is_empty() && state.status().is_terminal() {
+        // HashSet::remove returns true iff the element was present.
+        // Only reap nodes that THIS call emptied. Recovered-poisoned
+        // nodes have interested_builds=∅ from birth (from_poisoned_row
+        // at state/derivation.rs:416) — without this guard, the FIRST
+        // build completion post-recovery reaps every one of them,
+        // silently disabling poison-TTL tracking.
+        let was_interested = state.interested_builds.remove(&build_id);
+        if was_interested
+            && state.interested_builds.is_empty()
+            && state.status().is_terminal()
+        {
             to_reap.push(hash.clone());
         }
     }
```

**Why this belongs in the atomic PR:** the keystone fix makes recovered-poisoned nodes visible to `build_summary` (via `interested_builds.insert(*build_id)` at `recovery.rs:178`). But — wait. Re-read `recovery.rs:177-179`:

```rust
if let Some(state) = self.dag.node_mut(hash) {
    state.interested_builds.insert(*build_id);
}
```

After the keystone fix, `id_to_hash.get(drv_id)` succeeds for poisoned derivations, so this block DOES run, and `interested_builds` is no longer empty-from-birth for poisoned nodes that still have an Active build interested in them. **Good** — that half of reap-collateral is fixed by keystone alone.

But `from_poisoned_row` is also called for derivations whose build ALREADY terminated (the normal case — `poison_and_cascade` at `:458-460` transitions the build to Failed before the scheduler crashes). Those have no Active build in `load_nonterminal_builds`, so no `bd_rows` entry, so `interested_builds` stays empty. **These are the ones reap-collateral destroys.** They're the ones poison-TTL is supposed to protect. The `was_interested` guard is still needed.

**This is why it's in the atomic PR:** shipping keystone alone fixes spurious-Succeeded but leaves poison-TTL broken for the common case (derivation poisoned, build failed, scheduler restarted, next unrelated build completes → poisoned node reaped → next submit of the same drv dispatches immediately, retry storm).

---

## 2. Tracey rule

Tracey marker: `r[sched.recovery.poisoned-failed-count]` — see [`scheduler.md`](../../components/scheduler.md) (recovery section, after `r[sched.recovery.gate-dispatch]`). Recovered builds whose derivations include failure-terminal states MUST count those in `failed`, not omit them from the denominator; `load_poisoned_derivations` rows are inserted into the recovery-time `id_to_hash` map so `check_build_completion` sees `failed > 0`.

**Annotation sites:**

- `// r[impl sched.recovery.poisoned-failed-count]` → on the `id_to_hash.insert` line in `recovery.rs` (§1.1 diff)
- `// r[verify sched.recovery.poisoned-failed-count]` → on the three new tests in §4.1

---

## 3. Stageable follow-up PRs

Separate commits. Order matters: §3.1 before §3.3 (both touch `build.rs`, and §3.3's cancel-loop inversion depends on understanding the PG-first pattern).

### 3.1 `build.rs:408-446` — PG-first ordering in `transition_build`

**Current** (`rio-scheduler/src/actor/build.rs:408-446`):

```rust
pub(super) async fn transition_build(
    &mut self,
    build_id: Uuid,
    new_state: BuildState,
) -> Result<TransitionOutcome, ActorError> {
    if let Some(build) = self.builds.get_mut(&build_id) {
        if let Err(e) = build.transition(new_state) {      // ← :414 in-mem FIRST
            debug!(/* ... */);
            return Ok(TransitionOutcome::Rejected);
        }
        if new_state.is_terminal() {
            let duration = build.submitted_at.elapsed();
            metrics::histogram!("rio_scheduler_build_duration_seconds")
                .record(duration.as_secs_f64());
        }
    }
    let error_summary = self
        .builds
        .get(&build_id)
        .and_then(|b| b.error_summary.as_deref());
    self.db
        .update_build_status(build_id, new_state, error_summary)  // ← :442 PG SECOND
        .await?;
    Ok(TransitionOutcome::Applied)
}
```

**Diff** — adopt the `handle_clear_poison` pattern (`completion.rs:477-497`): validate transition without mutating, write PG, THEN mutate in-mem:

```diff
 pub(super) async fn transition_build(
     &mut self,
     build_id: Uuid,
     new_state: BuildState,
 ) -> Result<TransitionOutcome, ActorError> {
-    if let Some(build) = self.builds.get_mut(&build_id) {
-        if let Err(e) = build.transition(new_state) {
-            debug!(/* ... */);
-            return Ok(TransitionOutcome::Rejected);
-        }
-        if new_state.is_terminal() {
-            let duration = build.submitted_at.elapsed();
-            metrics::histogram!("rio_scheduler_build_duration_seconds")
-                .record(duration.as_secs_f64());
-        }
-    }
-    let error_summary = self
-        .builds
-        .get(&build_id)
-        .and_then(|b| b.error_summary.as_deref());
-    self.db
-        .update_build_status(build_id, new_state, error_summary)
-        .await?;
-    Ok(TransitionOutcome::Applied)
+    // Validate WITHOUT mutating. BuildInfo::can_transition is the
+    // same predicate as .transition() but read-only. If it doesn't
+    // exist yet, add it — state/build.rs has the state machine.
+    let Some(build) = self.builds.get(&build_id) else {
+        // Build not in map. Write PG anyway (idempotent if already
+        // terminal there). Matches old behavior.
+        self.db.update_build_status(build_id, new_state, None).await?;
+        return Ok(TransitionOutcome::Applied);
+    };
+    if !build.can_transition(new_state) {
+        debug!(build_id = %build_id, from = ?build.state(), to = ?new_state,
+               "build transition rejected; skipping DB update + side effects");
+        return Ok(TransitionOutcome::Rejected);
+    }
+    let error_summary = build.error_summary.clone();
+
+    // PG FIRST. If this fails, in-mem is untouched — next
+    // check_build_completion will retry the transition. A PG timeout
+    // here no longer plants a latent-corruption landmine (Route 3).
+    self.db
+        .update_build_status(build_id, new_state, error_summary.as_deref())
+        .await?;
+
+    // In-mem SECOND. Now guaranteed to succeed (can_transition
+    // checked above; actor is single-owner, no interleaving).
+    let build = self.builds.get_mut(&build_id).expect("checked above");
+    build.transition(new_state).expect("validated by can_transition");
+    if new_state.is_terminal() {
+        let duration = build.submitted_at.elapsed();
+        metrics::histogram!("rio_scheduler_build_duration_seconds")
+            .record(duration.as_secs_f64());
+    }
+    Ok(TransitionOutcome::Applied)
 }
```

**New helper needed** in `state/build.rs`: `BuildInfo::can_transition(&self, to: BuildState) -> bool`. Extract the validation logic from `transition()` — should be ~5 lines (match on `(self.state, to)` pairs). The state machine is already defined there per `// r[impl sched.build.state]`.

**Subtle semantic change:** old code wrote PG even if `builds.get_mut` returned `None` (the `if let Some` didn't early-return). New code preserves that — the `else` branch writes PG with `error_summary=None`. This matters for `handle_cancel_build` at `build.rs:145-147` which calls `update_build_status` directly (bypassing `transition_build`) — no change needed there.

### 3.2 `dag/mod.rs:483-494` — apply same fix to `remove_build_interest`

Sibling of §1.6. `remove_build_interest` (no `_and_reap`) at `dag/mod.rs:483-494` has the same unguarded `remove`:

```rust
pub fn remove_build_interest(&mut self, build_id: Uuid) -> Vec<DrvHash> {
    let mut orphaned = Vec::new();
    for (hash, state) in &mut self.nodes {
        state.interested_builds.remove(&build_id);
        if state.interested_builds.is_empty() && !state.status().is_terminal() {
            orphaned.push(hash.clone());
        }
    }
    orphaned
}
```

This one pushes **non-terminal** orphans (for ready-queue removal at `build.rs:128-131`). Recovered-poisoned nodes ARE terminal, so they don't match. But the same `was_interested` guard is correct-by-construction defensive — apply it anyway for symmetry. Low risk, low value. **Stageable, not blocking.**

### 3.3 `build.rs:78-147` — cancel-loop crash window (Route 2)

**Problem:** the loop at `:78-114` transitions each sole-interest in-flight derivation to Cancelled and persists it (`persist_status` at `:111`), THEN at `:145-147` transitions+persists the build. Crash between `:111` and `:147` leaves `derivations.status='cancelled'` + `builds.status='active'`.

Cancelled is in `TERMINAL_STATUSES` (`db.rs:25`). Recovery:
- `load_nonterminal_derivations` filters it out (not loaded at all)
- `load_poisoned_derivations` only loads `status='poisoned'` (not cancelled)
- `bd_rows` join falls through on `continue` — **even after the keystone fix**
- build gets `derivation_hashes ⊂ original`, missing the cancelled ones

If the cancelled drv was the build's ONLY drv: `total=0` → orphan-guard at `recovery.rs:310` fires, build skipped (good — but stays Active forever). If the build had ONE Completed + ONE now-cancelled: `total=1, completed=1, failed=0` → **spurious Succeeded**.

**Two fix options, pick one:**

**Option A — invert the order (preferred):**

```diff
+    // PG-first: mark the BUILD as cancelled BEFORE touching
+    // derivations. A crash mid-derivation-loop now leaves PG with
+    // build=Cancelled + some derivations still Assigned/Running.
+    // Recovery loads the Cancelled build? No — load_nonterminal_builds
+    // filters terminal. The derivations load as non-terminal with no
+    // interested build (bd_rows has no row for a terminal build).
+    // They sit in the DAG as orphaned non-terminal nodes →
+    // remove_build_interest returns them → ready_queue.remove. They
+    // never dispatch (interested_builds empty → not in any build's
+    // denominator). Next completion's reap won't touch them (not
+    // terminal). They leak until restart. ANNOYING but CORRECT — no
+    // spurious Succeeded.
+    //
+    // TODO(phase4b): recovery sweep for non-terminal derivations
+    // whose only bd_row points to a terminal build → transition
+    // Cancelled. Closes the leak.
+    self.db
+        .update_build_status(build_id, BuildState::Cancelled, None)
+        .await?;
+    if let Some(build) = self.builds.get_mut(&build_id)
+        && let Err(e) = build.transition(BuildState::Cancelled)
+    {
+        error!(build_id = %build_id, current = ?build.state(), error = %e,
+               "cancel transition rejected AFTER PG write — in-mem/PG diverged");
+        // Don't return — finish the derivation cancel loop. PG is
+        // already committed; bailing here leaves workers burning CPU.
+    }
+
     for (drv_hash, drv_path, worker_id) in &to_cancel {
         // [... unchanged: transition, send CancelSignal, persist ...]
     }

-    // [... delete the original transition block at :136-147 ...]
```

**Option B — recovery-side repair query:**

Add a fourth recovery query: `SELECT derivation_id, drv_hash FROM derivations WHERE status='cancelled' AND derivation_id IN (SELECT derivation_id FROM build_derivations WHERE build_id = ANY($active_build_ids))`. Load those into `id_to_hash` too. `build_summary` at `dag/mod.rs:609` already counts Cancelled in `failed` — so once the node is in the DAG with `interested_builds` populated, `check_build_completion` sees `failed > 0` → Failed, not Succeeded.

**Preference: Option A.** It closes the window at the source. Option B is a game of whack-a-mole — the next terminal-status-we-forgot-to-load repeats the bug. Option A's leak (orphaned non-terminal drvs in the DAG) is a resource-growth nuisance, not a correctness bug, and the `TODO(phase4b)` sweep is straightforward.

---

## 4. Tests

### 4.1 Unit tests — one per entry route + steady-state

All in `rio-scheduler/src/actor/tests/recovery.rs`. Follow the existing two-phase pattern (`test_recovery_loads_poisoned_derivations` at `:862-927`): actor A sets up state and dies, actor B recovers, assert final state.

**Route 1 — crash in poison-persist window:**

```rust
// r[verify sched.recovery.poisoned-failed-count]
/// Route 1: crash between persist_status(Poisoned) and the build
/// transition to Failed. PG has drv status='poisoned', poisoned_at SET
/// (the atomic persist_poisoned landed), build status='active'.
/// Recovery must load the poisoned drv into id_to_hash → bd_rows join
/// succeeds → build_summary counts it in `failed` → build → Failed.
///
/// Simulation: don't actually crash mid-await. Directly write the
/// inconsistent PG state that a crash WOULD leave, then recover. The
/// actor's completion path is deterministic; we're testing recovery's
/// interpretation of the state, not the crash itself.
#[tokio::test]
async fn test_recovery_poisoned_orphan_build_fails_not_succeeds() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // Phase 1: actor A merges a single-drv build, dispatches it, then
    // we directly write the crash-window PG state and kill actor A.
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_worker(&handle, "r1-w", "x86_64-linux", 1).await?;
        let _ev = merge_single_node(&handle, build_id, "r1-drv", PriorityClass::Scheduled).await?;
        let _ = worker_rx.recv().await.expect("assignment");
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Simulate crash-after-persist_poisoned-before-handle_derivation_failure:
    // drv is poisoned+timestamped, build is still active.
    sqlx::query("UPDATE derivations SET status='poisoned', poisoned_at=now() WHERE drv_hash=$1")
        .bind("r1-drv")
        .execute(&db.pool)
        .await?;
    // Build row: leave as-is (status='active' from merge).
    let (build_status,): (String,) =
        sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
            .bind(build_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(build_status, "active", "precondition: build still active in PG");

    // Phase 2: fresh actor B recovers.
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // THE assertion: build must be Failed, not Succeeded.
    // Before the keystone fix: Succeeded (total=0, failed=0).
    // After: Failed (total=1, failed=1).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "recovered build with only-poisoned drv MUST be Failed, got state={}",
        status.state
    );

    // Belt-and-suspenders: PG should reflect the transition.
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
            .bind(build_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(pg_status, "failed", "PG build status must follow in-mem");

    Ok(())
}
```

**Route 1 variant — `poisoned_at IS NULL` (the old two-call crash window):**

```rust
// r[verify sched.recovery.poisoned-failed-count]
/// Route 1 sub-variant: PRE-atomic-persist_poisoned crash window.
/// PG has status='poisoned' but poisoned_at IS NULL. After the
/// db.rs:504 filter drop + COALESCE, this row loads with elapsed=0
/// → treated as freshly-poisoned → id_to_hash populated → Failed.
///
/// This test proves the COALESCE fallback works AND that existing
/// bad rows in prod PG (from before this fix shipped) will be
/// interpreted correctly by a post-fix scheduler.
#[tokio::test]
async fn test_recovery_poisoned_null_timestamp_loads_via_coalesce() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_worker(&handle, "r1b-w", "x86_64-linux", 1).await?;
        let _ev = merge_single_node(&handle, build_id, "r1b-drv", PriorityClass::Scheduled).await?;
        let _ = worker_rx.recv().await.expect("assignment");
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // The OLD crash window: status set, poisoned_at NOT set.
    sqlx::query("UPDATE derivations SET status='poisoned', poisoned_at=NULL WHERE drv_hash=$1")
        .bind("r1b-drv")
        .execute(&db.pool)
        .await?;

    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Row must load (COALESCE → elapsed=0) and count as failed.
    let drv = handle.debug_query_derivation("r1b-drv").await?
        .expect("poisoned_at=NULL row must load after COALESCE fix");
    assert_eq!(drv.status, DerivationStatus::Poisoned);

    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Failed as i32);

    Ok(())
}
```

**Route 3 — latent landmine (PG-first ordering):**

```rust
/// Route 3: transition_build mutates in-mem BEFORE PG. A PG timeout
/// (not a crash) leaves in-mem=Succeeded, PG=Active. The scheduler
/// KEEPS RUNNING. On the NEXT restart, recovery loads Active + 0
/// non-terminal derivations → orphan-guard at recovery.rs:310 OR
/// spurious Succeeded if the build also has Completed drvs.
///
/// This test is for the §3.1 PG-first fix. It's a REGRESSION test —
/// it passes today (because the orphan-guard at :310 happens to catch
/// the total=0 case) but would FAIL if someone removes that guard.
/// Ship with §3.1, not the atomic PR.
#[tokio::test]
async fn test_transition_build_pg_failure_leaves_inmem_untouched() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    let (handle, _task) = setup_actor(db.pool.clone());
    let mut worker_rx = connect_worker(&handle, "r3-w", "x86_64-linux", 1).await?;
    let _ev = merge_single_node(&handle, build_id, "r3-drv", PriorityClass::Scheduled).await?;
    let _ = worker_rx.recv().await.expect("assignment");

    // Close pool to force update_build_status failure.
    db.pool.close().await;

    // Complete successfully. transition_build will fail the PG write.
    // BEFORE §3.1: in-mem transitions to Succeeded, PG write errors,
    //              complete_build propagates the error but in-mem is
    //              already mutated → query_status returns Succeeded.
    // AFTER §3.1:  PG write fails FIRST, in-mem untouched →
    //              query_status returns Active.
    complete_success(&handle, "r3-w", &test_drv_path("r3-drv")).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "PG failure in transition_build must leave in-mem Active, \
         not mutate-then-fail (Route 3 landmine)"
    );

    Ok(())
}
```

**Steady-state — reap-collateral:**

```rust
// r[verify sched.recovery.poisoned-failed-count]
/// Steady-state, NO crash: recover with 3 poisoned derivations (all
/// from builds that already terminated — no Active build in bd_rows).
/// Complete one UNRELATED build. All 3 poisoned nodes must survive.
///
/// Before the was_interested guard: the unrelated build's
/// remove_build_interest_and_reap walks the DAG, sees 3 nodes with
/// interested_builds=∅ (from_poisoned_row default) + status=Poisoned
/// (terminal) → reaps all 3. Poison-TTL silently disabled.
#[tokio::test]
async fn test_reap_does_not_collateral_damage_recovered_poisoned() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Phase 1: poison 3 derivations whose builds have ALREADY
    // terminated (Failed). The normal poison_and_cascade path does
    // this: drv → Poisoned, build → Failed, all in PG.
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_worker(&handle, "reap-w", "x86_64-linux", 3).await?;
        for tag in ["reap-p1", "reap-p2", "reap-p3"] {
            let _ev = merge_single_node(&handle, Uuid::new_v4(), tag, PriorityClass::Scheduled).await?;
            let _ = worker_rx.recv().await.expect("assignment");
            complete_failure(
                &handle, "reap-w", &test_drv_path(tag),
                rio_proto::types::BuildResultStatus::PermanentFailure, "perm",
            ).await?;
        }
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Precondition: 3 poisoned rows in PG, their builds all Failed.
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM derivations WHERE status='poisoned'"
    ).fetch_one(&db.pool).await?;
    assert_eq!(n, 3, "precondition: 3 poisoned rows");

    // Phase 2: recover. The 3 poisoned nodes load with
    // interested_builds=∅ (their builds are Failed → not in
    // load_nonterminal_builds → no bd_rows → :178 never fires).
    let (handle, _task) = setup_actor(db.pool.clone());
    let mut worker_rx = connect_worker(&handle, "reap-w2", "x86_64-linux", 1).await?;
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    for tag in ["reap-p1", "reap-p2", "reap-p3"] {
        let drv = handle.debug_query_derivation(tag).await?
            .expect("poisoned drv must be recovered");
        assert_eq!(drv.status, DerivationStatus::Poisoned);
    }

    // Phase 3: submit + complete an UNRELATED build. Its
    // schedule_terminal_cleanup → CleanupTerminalBuild →
    // remove_build_interest_and_reap fires.
    let unrelated = Uuid::new_v4();
    let _ev = merge_single_node(&handle, unrelated, "reap-unrelated", PriorityClass::Scheduled).await?;
    let _ = worker_rx.recv().await.expect("assignment");
    complete_success(&handle, "reap-w2", &test_drv_path("reap-unrelated")).await?;

    // Force cleanup NOW (TERMINAL_CLEANUP_DELAY is tokio::time::sleep
    // — advance time or send CleanupTerminalBuild directly).
    handle.send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: unrelated }).await?;
    barrier(&handle).await;

    // THE assertion: all 3 poisoned nodes still present.
    // Before was_interested guard: all 3 reaped (they match
    // "empty && terminal" even though `unrelated` was never
    // in their interested_builds).
    for tag in ["reap-p1", "reap-p2", "reap-p3"] {
        let drv = handle.debug_query_derivation(tag).await?;
        assert!(
            drv.is_some(),
            "poisoned node {tag} must survive unrelated-build reap"
        );
        assert_eq!(drv.unwrap().status, DerivationStatus::Poisoned);
    }

    Ok(())
}
```

### 4.2 DAG unit test — `remove_build_interest_and_reap` in isolation

In `rio-scheduler/src/dag/tests.rs`:

```rust
#[test]
fn test_reap_only_newly_emptied() {
    let mut dag = DerivationDag::new();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();

    // Node 1: interested in build_a, terminal. SHOULD reap on remove(a).
    let mut n1 = make_state("reap-yes");
    n1.interested_builds.insert(build_a);
    force_status(&mut n1, DerivationStatus::Completed);
    dag.insert_recovered_node(n1);

    // Node 2: interested in NOTHING, terminal. Should NOT reap on
    // remove(a) — it was already empty, build_a was never here.
    // This is the recovered-poisoned case.
    let mut n2 = make_state("reap-no-was-empty");
    force_status(&mut n2, DerivationStatus::Poisoned);
    dag.insert_recovered_node(n2);

    // Node 3: interested in build_a AND build_b, terminal. Should NOT
    // reap on remove(a) — still has build_b.
    let mut n3 = make_state("reap-no-still-interested");
    n3.interested_builds.insert(build_a);
    n3.interested_builds.insert(build_b);
    force_status(&mut n3, DerivationStatus::Completed);
    dag.insert_recovered_node(n3);

    let reaped = dag.remove_build_interest_and_reap(build_a);

    assert_eq!(reaped, 1, "only node 1 should reap");
    assert!(dag.node(&"reap-yes".into()).is_none());
    assert!(dag.node(&"reap-no-was-empty".into()).is_some(),
            "already-empty nodes must NOT be collaterally reaped");
    assert!(dag.node(&"reap-no-still-interested".into()).is_some());
}
```

### 4.3 DB unit test — `persist_poisoned` atomicity

Update `db.rs:1583-1610` (`test_poisoned_at_roundtrip`) to use `persist_poisoned` instead of `update_derivation_status` + `set_poisoned_at`. Add a new assertion that a single call sets BOTH columns:

```rust
#[tokio::test]
async fn test_persist_poisoned_atomic() -> anyhow::Result<()> {
    let db = /* ... test db setup ... */;
    let drv_hash: DrvHash = "atomic-poison".into();
    /* ... insert a derivation row with status='running' ... */

    db.persist_poisoned(&drv_hash).await?;

    let (status, has_ts, worker): (String, bool, Option<String>) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL, assigned_worker_id \
         FROM derivations WHERE drv_hash=$1"
    ).bind("atomic-poison").fetch_one(&db.pool).await?;

    assert_eq!(status, "poisoned");
    assert!(has_ts, "poisoned_at must be set in the same statement");
    assert!(worker.is_none(), "assigned_worker_id must be NULLed");
    Ok(())
}
```

---

## 5. Verification

### 5.1 Unit — proves the atomic PR is correct

```bash
nix develop -c cargo nextest run -p rio-scheduler \
  test_recovery_poisoned_orphan_build_fails_not_succeeds \
  test_recovery_poisoned_null_timestamp_loads_via_coalesce \
  test_reap_does_not_collateral_damage_recovered_poisoned \
  test_reap_only_newly_emptied \
  test_persist_poisoned_atomic
```

All five must pass. `test_recovery_poisoned_orphan_build_fails_not_succeeds` is the red→green for the keystone: it **fails on `main`** with `assertion failed: left: 3 (Succeeded), right: 4 (Failed)`.

Full scheduler suite (existing tests must not regress — `test_recovery_loads_poisoned_derivations` at `:862` in particular exercises the same code path):

```bash
nix develop -c cargo nextest run -p rio-scheduler
```

### 5.2 VM — catches regression end-to-end

`vm-lifecycle-recovery-k3s` already kills the leader mid-build and asserts `rio_scheduler_recovery_total{outcome="success"} 1` (`nix/tests/scenarios/lifecycle.nix:1066`). It does NOT assert that builds don't spuriously succeed — the current `recoverySlowDrv` is designed to SURVIVE the failover and complete legitimately.

**Add a negative assertion** in the `recovery` subtest after the `sched_metric_wait` at `:1068`:

```python
# Regression guard for remediation 01: after recovery, no build
# should have transitioned to Succeeded with zero output_paths.
# The (0/0 derivations) K8s event text is the one reliable grep
# — see remediations/phase4a.md §1.1 "Downstream blast radius".
k3s.succeed(
    "! kubectl get events -n rio --field-selector reason=BuildSucceeded "
    "-o jsonpath='{.items[*].message}' | grep -q '(0/0 derivations)'"
)
```

This catches ALL three routes (they all produce the same `0/0` event). It's cheap (one kubectl call, no additional build). It's in the right place (after recovery, before the post-recovery build submission).

```bash
nix-build-remote --no-nom --dev -- -L .#checks.x86_64-linux.vm-lifecycle-recovery-k3s
```

### 5.3 Tracey

```bash
nix develop -c tracey query rule sched.recovery.poisoned-failed-count
```

Must show: 1 spec site (`scheduler.md`), 1 impl site (`recovery.rs` keystone insert), 3 verify sites (the three `r[verify ...]` tests in §4.1).

---

## 6. Risk

### 6.1 Prod PG already has spuriously-Succeeded rows

**The fix does not retroactively repair PG.** Builds that ALREADY went through the spurious-Succeeded path have `builds.status='succeeded'` + `finished_at` set. Recovery's `load_nonterminal_builds` filters terminal — these rows are invisible to the scheduler forever. The Nix client that received `status=Built, built_outputs=[]` has already moved on (or failed at the use site).

**Repair query — run ONCE, manually, before deploying the fix:**

```sql
-- Find spurious-Succeeded candidates: build is Succeeded, but EVERY
-- linked derivation is failure-terminal (poisoned/cancelled/dependency_failed).
-- A legitimately-Succeeded build has at least one 'completed' derivation.
SELECT b.build_id, b.tenant_id, b.finished_at,
       array_agg(d.drv_hash) AS drv_hashes,
       array_agg(d.status) AS drv_statuses
FROM builds b
JOIN build_derivations bd ON bd.build_id = b.build_id
JOIN derivations d ON d.derivation_id = bd.derivation_id
WHERE b.status = 'succeeded'
GROUP BY b.build_id, b.tenant_id, b.finished_at
HAVING bool_and(d.status IN ('poisoned', 'cancelled', 'dependency_failed'));
```

For each row returned: `UPDATE builds SET status='failed', error_summary='retroactive: spurious Succeeded via poisoned-orphan (remediation 01)' WHERE build_id=$1`. The Build CR in k8s (if still present) will NOT auto-update — the controller only watches Active builds. Operator must manually `kubectl patch` or accept the stale CR.

**Do we NEED the repair?** Probably not — these builds are already done from the client's perspective, and the client either crashed at use-site or the output happened to already be in a substituter. The repair is for accounting hygiene (SLO dashboards, `rio_scheduler_builds_total{outcome="success"}` pollution). Run the SELECT, eyeball the count, decide.

### 6.2 `COALESCE(..., 0.0)` gives existing NULL-timestamp rows a fresh 24h TTL

Intentional and conservative (§1.4). Worst case: a derivation that was poisoned right before a crash, then ignored for a week (because it was filtered out), now gets a fresh 24h TTL when the fix deploys. Operators who want it cleared immediately can `ClearPoison` via admin RPC. No correctness impact.

### 6.3 `was_interested` guard changes reap behavior for OTHER empty-terminal nodes

The guard is strictly narrower than the old predicate. Any node the new code reaps, the old code also reaped. The ONLY behavior change is: nodes that were ALREADY `interested_builds=∅ ∧ terminal` before this call are now preserved.

**Are there legitimate cases where such nodes SHOULD be reaped?** Only one: a node that was reaped by build A's cleanup, then re-inserted by a new merge (fresh, `interested_builds` populated by that merge), then the new build completes and cleans up. In that case `was_interested` is `true` (the new build's id WAS in the set) → reaps correctly. No false-negatives.

**Memory growth?** Poisoned nodes now survive until TTL expiry (24h) → `handle_tick` clears them via `dag.remove_node` (`completion.rs` ClearPoison path). Bounded. This is the INTENDED behavior — it was broken before.

### 6.4 PG-first in `transition_build` changes error semantics

**Before §3.1:** `transition_build` returns `Err` only on PG failure, but by then in-mem is already mutated. Callers (`complete_build` at `build.rs:327-329`, `transition_build_to_failed`) propagate the `Err` via `?` — but the BuildCompleted event at `:350` has NOT fired yet (it's after the `?`). So the old code already skipped events on PG failure; it just left in-mem wrong.

**After §3.1:** `Err` means neither PG nor in-mem changed. `check_build_completion` will retry on the next completion (it's re-entrant — `:299` guards on `is_terminal`). Strictly better. But: if the build has no more derivations completing (this WAS the last one), there's no trigger for retry. The build stays Active in-mem AND in PG (consistent, at least). Next scheduler restart's recovery will re-run `check_build_completion` at `:318` and retry. Acceptable — this is the "PG is down" scenario, and the scheduler has bigger problems.

### 6.5 Deploy sequencing

The atomic PR is backward-compatible with existing PG data (COALESCE handles old NULL rows). No migration needed. Safe to deploy scheduler-first without coordinating with worker/gateway/controller — this is scheduler-internal state management.

§3.1 and §3.3 are also scheduler-only. No cross-component sequencing.
