# Remediation 08: Lease renewal timeout + local self-fence + recovery TOCTOU

**Parent:** [§2.2 Lease renewal no timeout](../phase4a.md#22-lease-renewal-no-timeout)
**Findings:** `kube-lease-renew-no-timeout`, `kube-lease-no-local-self-fence`, `sched-recovery-complete-toctou`
**Priority:** P1 (HIGH)
**tracey:** `r[sched.lease.k8s-lease]`, `r[sched.lease.generation-fence]`, `r[sched.recovery.gate-dispatch]`

---

## Problem

Three related liveness/safety gaps in the lease loop. All share a root cause: **the lease loop trusts the apiserver to return promptly and trusts its own `is_leader` flag to be authoritative.** Neither holds under network partition.

### Finding 1: Renewal has no timeout

`rio-scheduler/src/lease/election.rs:189` (`api.get_opt(...).await?`) and `:316` (`api.replace(...).await`) have no deadline. kube-rs does not install a default request timeout — these await as long as the underlying hyper connection stays open. An apiserver that accepts the TCP connection but never replies (overloaded etcd, half-open socket, SYN-accepted-then-stall proxy) will hang the call past `LEASE_TTL`.

The loop is structured around `tokio::time::interval(RENEW_INTERVAL)` with `MissedTickBehavior::Skip`, but `interval.tick()` is only awaited at the **top** of the loop. Once `try_acquire_or_renew().await` is entered, the interval can't preempt it. Timeline:

| T | Event |
|---|---|
| T+0 | Leader's `interval.tick()` fires → calls `try_acquire_or_renew()` |
| T+0 | `api.get_opt(...)` sends GET, apiserver never replies |
| T+15 | `LEASE_TTL` elapses. Standby's observed-record clock hits TTL → steals. **Two replicas now have `is_leader=true`.** |
| T+15..∞ | Hung leader keeps dispatching. Workers reject (stale gen in heartbeat) but the RPCs go out. |
| T+120? | OS TCP keepalive (if enabled) or apiserver recovery finally breaks the socket → `Err(e)` → lands in Finding 2. |

### Finding 2: No local self-fence on the error arm

`rio-scheduler/src/lease/mod.rs:385-398` — when `try_acquire_or_renew()` finally returns `Err`, the code logs and retries:

```rust
Err(e) => {
    // DON'T flip is_leader here. We might still hold
    // the lease (apiserver is down for EVERYONE; no
    // one can acquire). ...
    warn!(error = %e, "lease renew failed (transient?); retrying next tick");
}
```

The comment's reasoning is correct for the **first** failed tick (T+5s: one failure, lease still has 10s). It is wrong after `LEASE_TTL` has elapsed locally. At that point we **know** — from our own monotonic clock, no apiserver round-trip required — that if any other replica can reach the apiserver, it has already stolen. The only world where we're still the rightful leader is one where **nobody** can reach the apiserver, in which case dispatch is pointless anyway (workers can't be rescheduled, gateway can't do service discovery, cluster is degraded).

**On the comment's "apiserver is down for EVERYONE" argument:** the fix is correct precisely **because** of the asymmetric-partition case. If scheduler-A is partitioned from the apiserver but **not** from workers:

- A keeps `is_leader=true` → keeps sending `WorkAssignment`s to workers.
- B (still reachable) has stolen the lease, bumped gen, is also sending assignments.
- Workers see B's gen in heartbeat → reject A's assignments (`r[sched.lease.generation-fence]` saves **correctness**).
- But A is now a pure noise generator: `rio_scheduler_assignments_sent_total` climbs on both, logs fill with worker-side "stale generation" warnings, `ListWorkers` from A shows phantom Assigned states.

Flipping `is_leader=false` locally after TTL trades a narrow unavailability window (symmetric partition: nobody dispatches for a few ticks until apiserver recovers — but nobody could do useful work anyway) for eliminating the split-brain noise in the much more common asymmetric case (node NIC flap, pod-level netpol misconfiguration, apiserver LB kicking one client).

### Finding 3: Recovery-complete TOCTOU on lease flap

`rio-scheduler/src/actor/recovery.rs:367,389` — `handle_leader_acquired` unconditionally stores `recovery_complete = true` on both the Ok and Err arms:

```rust
Ok(()) => {
    // ... sweep_stale_live_pins ...
    self.recovery_complete.store(true, Ordering::Release);  // :367
}
Err(e) => {
    // ... clear DAG ...
    self.recovery_complete.store(true, Ordering::Release);  // :389
}
```

There is no check that we are **still** the same leader that started recovery. Consider a flap during a slow recovery (10k-derivation DAG, PG under load — the module doc at `recovery.rs:11-14` explicitly acknowledges multi-second recovery):

| T | Lease task (own goroutine) | Actor command loop |
|---|---|---|
| T+0 | Acquire: gen 1→2, `is_leader=true`, send `LeaderAcquired` | — |
| T+1 | — | Starts `handle_leader_acquired` → `recover_from_pg()` begins loading from PG (gen-2's view) |
| T+4 | Renew fails (409 — flap, or apiserver 5xx burst) → **lose transition** → `is_leader=false`, `recovery_complete=false` (`mod.rs:359,363`) | Still inside `recover_from_pg()` |
| T+9 | Re-acquire: gen 2→3, `is_leader=true`, send **second** `LeaderAcquired` | Still inside `recover_from_pg()` |
| T+12 | — | `recover_from_pg()` returns → **`recovery_complete.store(true)`** at :367 |
| T+12 | — | `self.dispatch_ready().await` at `actor/mod.rs:557` — **dispatches from the gen-2 DAG with gen-3 stamped on assignments** |
| T+12 | — | Pops second `LeaderAcquired` from channel → clears DAG, re-loads |

The lease loop's `recovery_complete.store(false)` at T+4 is **clobbered** by the actor's unconditional `store(true)` at T+12. For one dispatch pass, we ship assignments whose derivation state was loaded from PG at T+1 but which carry generation 3. The worker-side generation fence **does not save us here** — the generation on the wire is current; it's the DAG that's stale.

Consequences: duplicate dispatch (gen-3 leader may have already dispatched these during T+9..T+12), or dispatch of derivations whose PG status changed during the gap. Idempotent upserts mostly absorb this but it's a correctness smell and will show up as spurious `retry_count` increments.

---

## Fix

### 1. Wrap `try_acquire_or_renew()` callsite in `tokio::time::timeout`

**Where:** The callsite is `rio-scheduler/src/lease/mod.rs:273` (inside `run_lease_loop`'s tick loop), not inside `election.rs`. Wrapping at the callsite catches **both** `get_opt` (election.rs:189) and `replace` (election.rs:316) with one deadline, and keeps `election.rs` a pure I/O shell (easier to mock-test).

**Deadline:** `RENEW_INTERVAL - RENEW_SLOP` = 5s − 2s = **3s**. Rationale: the loop must get three attempts in before `LEASE_TTL` (3 × 5s = 15s). Each attempt must finish **before** the next `interval.tick()` would fire; 3s leaves slack for the select/match overhead and avoids the degenerate case where timeout = interval (next tick races the timeout). 3s is generous for a single Lease GET+PUT — p99 on a healthy apiserver is <100ms.

```diff
--- a/rio-scheduler/src/lease/mod.rs
+++ b/rio-scheduler/src/lease/mod.rs
@@ -62,6 +62,13 @@ const LEASE_TTL: Duration = Duration::from_secs(15);

 /// Renewal interval. LEASE_TTL / 3 per K8s convention.
 const RENEW_INTERVAL: Duration = Duration::from_secs(5);
+
+/// Slack between renew timeout and RENEW_INTERVAL. Each renew
+/// attempt must return BEFORE the next interval tick would fire;
+/// otherwise a hung apiserver burns multiple ticks on one call.
+/// 3s deadline for a Lease GET+PUT is generous (healthy p99 <100ms)
+/// while still giving 3 attempts before LEASE_TTL.
+const RENEW_SLOP: Duration = Duration::from_secs(2);
```

```diff
@@ -259,6 +266,7 @@ pub async fn run_lease_loop(
     );

     let mut was_leading = false;
+    let mut last_successful_renew = Instant::now();
     let mut interval = tokio::time::interval(RENEW_INTERVAL);
     // Skip: if one renewal is slow (apiserver busy), don't fire
     // twice immediately. The lease TTL is 15s; we have slack.
@@ -270,8 +278,15 @@ pub async fn run_lease_loop(
             _ = interval.tick() => {}
         }

-        match election.try_acquire_or_renew().await {
-            Ok(result) => {
+        let renew_deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
+        match tokio::time::timeout(renew_deadline, election.try_acquire_or_renew()).await {
+            Ok(Ok(result)) => {
+                // Successful round-trip (apiserver answered). Even
+                // Standby/Conflict reset the self-fence clock — we
+                // KNOW the apiserver state, we just don't hold the
+                // lease. The clock tracks "am I blind", not "am I
+                // leader".
+                last_successful_renew = Instant::now();
```

### 2. Local self-fence on the error arm

Track `last_successful_renew: Instant` (diff above) and check it when a renew **does not** succeed. Collapse the two failure modes — `Ok(Err(kube_err))` (apiserver returned 5xx / connection refused) and `Err(Elapsed)` (our timeout fired) — into one arm: both mean "we have no fresh knowledge of the Lease object."

```diff
@@ -382,20 +397,46 @@ pub async fn run_lease_loop(

                 was_leading = now_leading;
             }
-            Err(e) => {
-                // K8s API error. Transient — retry next tick. If
-                // this persists past LEASE_TTL, our lease expires
-                // and another replica acquires. That's CORRECT
-                // behavior for "this replica's K8s connectivity
-                // is broken."
+            outcome @ (Ok(Err(_)) | Err(_)) => {
+                // Either apiserver returned an error (Ok(Err)) or
+                // our timeout fired before it answered (Err(Elapsed)).
+                // Both mean: no fresh view of the Lease object.
+                match &outcome {
+                    Ok(Err(e)) => {
+                        warn!(error = %e, "lease renew failed (apiserver error); retrying next tick");
+                    }
+                    Err(_) => {
+                        warn!(deadline = ?renew_deadline, "lease renew TIMED OUT (apiserver hung?); retrying next tick");
+                    }
+                    Ok(Ok(_)) => unreachable!(),
+                }
                 //
-                // DON'T flip is_leader here. We might still hold
-                // the lease (apiserver is down for EVERYONE; no
-                // one can acquire). Flipping would stop dispatch
-                // even though we're the only viable leader. Let
-                // the TTL expiry decide.
-                warn!(error = %e, "lease renew failed (transient?); retrying next tick");
+                // Local self-fence: if LEASE_TTL has elapsed since
+                // the last SUCCESSFUL round-trip, flip is_leader
+                // locally. At this point, any replica that CAN reach
+                // the apiserver has already stolen (observed-record
+                // TTL = our TTL; same clock).
+                //
+                // The old "DON'T flip — apiserver down for EVERYONE"
+                // argument is wrong once elapsed > TTL. In the
+                // symmetric-partition case (nobody reaches apiserver)
+                // flipping costs nothing: workers can't be scheduled
+                // anyway. In the asymmetric case (WE are partitioned,
+                // peer is not) NOT flipping makes us a stale-assignment
+                // noise generator. Worker-side generation fence
+                // (r[sched.lease.generation-fence]) saves correctness
+                // either way; this fence saves ops sanity.
+                if was_leading && last_successful_renew.elapsed() > LEASE_TTL {
+                    warn!(
+                        blind_for = ?last_successful_renew.elapsed(),
+                        "LOCAL SELF-FENCE: no successful renew in > LEASE_TTL, stepping down locally"
+                    );
+                    state.is_leader.store(false, Ordering::Relaxed);
+                    state.recovery_complete.store(false, Ordering::Relaxed);
+                    metrics::counter!("rio_scheduler_lease_lost_total").increment(1);
+                    was_leading = false;
+                    // No spawn_patch_deletion_cost: can't reach apiserver.
+                    // The peer (if leading) patched its OWN pod.
+                }
             }
         }
     }
```

**Note on `Ordering::Release`:** §2.2 suggests `Release` on `is_leader.store(false)`. The existing lose-transition at `mod.rs:359` uses `Relaxed`, and the field doc at `mod.rs:134-139` explains why (`is_leader` is a standalone flag; `generation` is the fence-carrying atomic). Keep `Relaxed` for consistency — the pairing is `generation` Release/Acquire, not `is_leader`.

**Reset of `last_successful_renew`:** on every `Ok(Ok(_))`, **including** `Standby` and `Conflict`. The clock measures "time since I last saw the apiserver respond," not "time since I last renewed successfully as leader." A standby that gets `Ok(Ok(Standby))` every 5s is perfectly healthy; its self-fence clock should never fire.

### 3. Recovery TOCTOU: generation snapshot + re-check before store

**Chosen approach:** capture `self.generation.load()` at the top of `handle_leader_acquired`, re-check before the `recovery_complete.store(true)`. Preferred over "re-check `is_leader`" because:

- `is_leader` can be **true again** by the time recovery finishes (flap: lose→reacquire in the middle). Re-checking `is_leader` would pass, and we'd still dispatch stale state.
- Generation is monotone (`fetch_add` on every acquire, `fetch_max` from PG). If `gen_after != gen_at_entry`, the lease flapped **at least once** during recovery — the DAG we loaded is bound to the wrong generation. Unambiguous.
- The second `LeaderAcquired` (sent by the re-acquire at T+9 in the timeline) is already queued in the actor's mpsc channel. Discarding this recovery's result and returning early lets the actor loop pop that command next and re-run recovery cleanly.

One subtlety: `recover_from_pg()` itself calls `self.generation.fetch_max(...)` at `recovery.rs:242-246` when seeding from PG high-water. Capture `gen_at_entry` **before** calling `recover_from_pg()`, and re-read **after** — if the only difference is the `fetch_max` bump, that's fine (it means PG had a higher gen than the lease, which is the "Lease annotation got reset" case the code already handles). The check we want is: **did the lease task do another `fetch_add` while we were busy?** `fetch_max` monotone-bumps to a known target; `fetch_add` always increments. Compare against `gen_at_entry.max(pg_target)` — or, simpler, have `recover_from_pg()` return the generation it seeded to and compare against the current load.

Simplest correct form:

```diff
--- a/rio-scheduler/src/actor/recovery.rs
+++ b/rio-scheduler/src/actor/recovery.rs
@@ -343,8 +343,19 @@ impl DagActor {
     /// inconsistent). MergeDag from a standby-period SubmitBuild
     /// would queue in the mpsc channel and get processed after.
     pub(super) async fn handle_leader_acquired(&mut self) {
+        // Snapshot generation BEFORE recovery. If the lease flaps
+        // (lose→reacquire) while recover_from_pg() is running, the
+        // lease loop will fetch_add again AND will have cleared
+        // recovery_complete (mod.rs:363). An unconditional store(true)
+        // here would clobber that clear and let dispatch_ready fire
+        // with a DAG loaded under the OLD generation but stamped with
+        // the NEW one — worker-side gen fence can't catch that.
+        let gen_at_entry = self.generation.load(std::sync::atomic::Ordering::Acquire);
+
         match self.recover_from_pg().await {
             Ok(()) => {
+                // recover_from_pg may have fetch_max'd generation from
+                // PG high-water. Re-read AFTER for the staleness check.
                 // Stale-pin cleanup: ...
                 match self.db.sweep_stale_live_pins().await {
                     ...
                 }
-                self.recovery_complete
-                    .store(true, std::sync::atomic::Ordering::Release);
+                let gen_now = self.generation.load(std::sync::atomic::Ordering::Acquire);
+                // gen_now > gen_at_entry by MORE than the PG seed bump
+                // → lease task fetch_add'd → flap. The re-acquire
+                // already sent a fresh LeaderAcquired (queued in our
+                // mpsc). Discard this recovery; let the next
+                // LeaderAcquired re-run it. DON'T set recovery_complete
+                // — the lease loop's clear at T+lose stays in effect.
+                //
+                // "More than the PG seed bump" check: recover_from_pg
+                // only fetch_max's to (pg_max + 1). If gen_at_entry
+                // was already >= that (common — lease loop's fetch_add
+                // runs FIRST), fetch_max is a no-op and gen_now ==
+                // gen_at_entry. Any difference at all means either
+                // (a) PG seeded higher (fine, no flap) or (b) lease
+                // fetch_add'd (flap). We can't distinguish from here
+                // without plumbing the PG target back — so be
+                // conservative: ANY change = discard. False-positive
+                // cost is one extra recovery pass; false-negative
+                // cost is dispatching stale state.
+                if gen_now != gen_at_entry {
+                    warn!(
+                        gen_at_entry, gen_now,
+                        "generation changed during recovery — lease flapped or PG \
+                         seeded higher; DISCARDING this recovery (next LeaderAcquired \
+                         will retry)"
+                    );
+                    metrics::counter!("rio_scheduler_recovery_total", "outcome" => "discarded_flap")
+                        .increment(1);
+                    // Clear the partial state we loaded. The next
+                    // LeaderAcquired's recover_from_pg() will do this
+                    // again but do it here too so any Tick that
+                    // sneaks in before the next LeaderAcquired sees
+                    // a consistent (empty) DAG.
+                    self.dag = DerivationDag::new();
+                    self.ready_queue.clear();
+                    self.builds.clear();
+                    self.build_events.clear();
+                    self.build_sequences.clear();
+                    return;
+                }
+                self.recovery_complete
+                    .store(true, std::sync::atomic::Ordering::Release);
             }
             Err(e) => {
                 // ... (same gen check on the Err arm — unconditional
                 // store(true) here has the same TOCTOU) ...
+                let gen_now = self.generation.load(std::sync::atomic::Ordering::Acquire);
+                if gen_now != gen_at_entry {
+                    warn!(gen_at_entry, gen_now, error = %e,
+                          "recovery failed AND generation changed — discarding");
+                    metrics::counter!("rio_scheduler_recovery_total", "outcome" => "discarded_flap")
+                        .increment(1);
+                    return; // DAG already cleared below → move clears above the check
+                }
                 error!(...);
                 // ... existing clear + store(true) ...
             }
         }
     }
```

**Also gate the immediate dispatch at `actor/mod.rs:557`:** `self.dispatch_ready().await` runs right after `handle_leader_acquired()` returns. `dispatch_ready()` already checks `is_leader && recovery_complete` (`dispatch.rs:18,27`) so if we `return` early without setting `recovery_complete=true`, the dispatch is a no-op. No change needed there — just noting that the existing gate makes the early-return safe.

---

## Tests

### Unit: `try_acquire_or_renew` timeout fires on hung apiserver

`ApiServerVerifier` (`rio-test-support/src/kube_mock.rs`) currently responds immediately to each scenario. For hang injection, the simplest approach is to **exploit the existing behaviour at kube_mock.rs:75-76**: "When scenarios are exhausted, the task returns — any further request hangs." Pass an **empty** scenario list → the verifier task returns immediately → the mock's `Handle` has no responder → the client's request pends forever. Wrap in the production-code timeout.

New test in `rio-scheduler/src/lease/mod.rs`'s `tests` module (not `election.rs` — the timeout is at the `run_lease_loop` callsite):

```rust
/// Apiserver accepts the connection but never responds. The renew
/// timeout must fire within RENEW_INTERVAL - RENEW_SLOP. Without
/// the timeout wrapper, this test would hang until the outer
/// tokio::test timeout (proving the bug).
#[tokio::test]
async fn renew_timeout_fires_on_hung_apiserver() {
    let (client, verifier) = ApiServerVerifier::new();
    // Empty scenario list: verifier task exits immediately, mock
    // Handle has nobody to send_response → client request pends.
    let _task = verifier.run(vec![]);

    let mut election = LeaderElection::new(
        client, "default", "rio-sched".into(), "us".into(),
        Duration::from_secs(15),
    );

    let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
    let started = Instant::now();
    let result = tokio::time::timeout(deadline, election.try_acquire_or_renew()).await;

    assert!(result.is_err(), "timeout should fire, got {:?}", result);
    // Prove it was OUR timeout, not some inner kube-rs deadline.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= deadline && elapsed < deadline + Duration::from_millis(500),
        "timeout fired at {elapsed:?}, expected ~{deadline:?}"
    );
}
```

This exercises the **mechanism** (timeout wrapper works). To exercise the **loop integration** (self-fence flips `is_leader` after TTL), extract the loop body into a `fn tick(&mut election, &state, &mut was_leading, &mut last_successful_renew) -> ...` that can be driven from a test without spawning the full loop. Alternative: `#[tokio::test(start_paused = true)]` + `tokio::time::advance` — but per `lang-gotchas.md`, paused time + real TCP (even mock-tower) causes spurious deadline-exceeded on the wrong op. Extracting the body is cleaner.

### Unit: local self-fence flips after TTL of failures

With the loop body extracted:

```rust
#[tokio::test]
async fn self_fence_flips_is_leader_after_ttl_of_failures() {
    // Pre-seed: was_leading=true, last_successful_renew = 20s ago
    // (past LEASE_TTL=15s). Feed one Err tick → assert is_leader
    // flipped false, recovery_complete cleared, was_leading=false.
    let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
    state.is_leader.store(true, Ordering::Relaxed);
    state.recovery_complete.store(true, Ordering::Relaxed);

    let mut was_leading = true;
    let last_renew = Instant::now() - Duration::from_secs(20);

    // Drive one tick with an Err result.
    lease_tick_err(&state, &mut was_leading, last_renew);

    assert!(!state.is_leader.load(Ordering::Relaxed), "self-fence should flip");
    assert!(!state.recovery_complete.load(Ordering::Relaxed));
    assert!(!was_leading);
}

#[tokio::test]
async fn self_fence_does_not_flip_before_ttl() {
    // Same, but last_successful_renew = 10s ago (< LEASE_TTL).
    // One Err tick → is_leader stays true (transient blip).
    // ...
    assert!(state.is_leader.load(Ordering::Relaxed), "within TTL → no flip");
}
```

### Unit: recovery TOCTOU — flap during recovery discards stale DAG

This needs the actor's PG dependency injected (it already is — `self.db` is a trait object in tests). Strategy: make `recover_from_pg`'s DB calls return a future that **we control** (oneshot channel as a gate), so the test can interleave:

```rust
#[tokio::test]
async fn recovery_discards_on_generation_bump_mid_flight() {
    // 1. Construct DagActor with a mock DB whose load_nonterminal_builds()
    //    awaits a oneshot gate before returning.
    // 2. Spawn handle_leader_acquired() on a local task.
    // 3. While it's blocked on the gate: bump generation (simulating
    //    lease loop's fetch_add on re-acquire) AND clear recovery_complete
    //    (simulating lease loop's lose-transition clear).
    // 4. Release the gate → recovery finishes.
    // 5. Assert: recovery_complete is STILL false (early-return didn't
    //    clobber), DAG is empty (discarded), metric outcome=discarded_flap
    //    incremented.

    let gen = Arc::new(AtomicU64::new(2));
    let recovery_complete = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = oneshot::channel();
    let db = MockDb::new().with_builds_gate(gate_rx);
    let mut actor = DagActor::test_builder()
        .with_generation(Arc::clone(&gen))
        .with_recovery_flag(Arc::clone(&recovery_complete))
        .with_db(db)
        .build();

    let handle = tokio::spawn(async move {
        actor.handle_leader_acquired().await;
        actor
    });

    // Give the spawned task a tick to reach the gate.
    tokio::task::yield_now().await;

    // Simulate lease flap: lose (clear) + reacquire (bump gen).
    recovery_complete.store(false, Ordering::Relaxed);
    gen.fetch_add(1, Ordering::Release); // 2 → 3

    // Release recovery.
    gate_tx.send(()).unwrap();
    let actor = handle.await.unwrap();

    assert!(
        !recovery_complete.load(Ordering::Acquire),
        "TOCTOU: recovery_complete clobbered lease loop's clear — \
         would dispatch gen-2 DAG with gen-3 stamps"
    );
    assert_eq!(actor.dag.iter_nodes().count(), 0, "stale DAG discarded");
    // assert metric rio_scheduler_recovery_total{outcome=discarded_flap} == 1
}
```

**Negative control:** same test, but **don't** bump the generation → assert `recovery_complete == true` and DAG populated. Proves the check doesn't false-positive on a clean recovery.

### Fault-injection: no dispatch from stale DAG

Extend the TOCTOU test above with a dispatch probe: after the early-return, call `actor.dispatch_ready().await` explicitly and assert no `WorkAssignment` was sent (mock worker channel stays empty). This is the end-to-end "NO dispatch from stale DAG" assertion.

---

## Verification

### Existing VM coverage

**`vm-le-stability-k3s`** (`nix/tests/scenarios/leader-election.nix`, composed in `nix/tests/default.nix:281-290`) exercises leader election with two scheduler replicas on k3s. Subtests:

- `antiAffinity` — precondition: replicas on different k3s nodes.
- `lease-acquired` — metric `rio_scheduler_lease_acquired_total >= 1` (acquire transition body fires).
- `stable-leadership` — `leaseTransitions` flat over 60s (no flip-flop; the cdb70c2 regression guard).
- `graceful-release` — SIGTERM leader → `step_down()` → standby acquires in <10s.
- `failover` — `kubectl delete --force` leader → standby acquires, `leaseTransitions` +1.

**`vm-le-build-k3s`** — `build-during-failover`: a 60s build survives a leader kill mid-flight.

### Gap: no apiserver-hang coverage

**None of the above exercise the apiserver-hang path.** Every subtest kills a **scheduler** pod; none degrades the **apiserver**. Grep of `nix/tests/scenarios/leader-election.nix` for `iptables`, `DROP`, `6443`, `netem` — zero matches. The existing tests validate `election.rs`'s optimistic-concurrency logic (409 handling, observed-record) but not `mod.rs`'s error arm.

### New VM subtest: `apiserver-partition`

Add a fragment to `nix/tests/scenarios/leader-election.nix` that partitions the leader's node from the apiserver (port 6443) via iptables, waits > `LEASE_TTL`, and asserts both the self-fence and the standby takeover:

```python
# r[verify sched.lease.k8s-lease]
#   apiserver-partition: leader's node blocked from :6443 → local
#   self-fence flips is_leader within LEASE_TTL → standby acquires.
#   Pre-fix: leader's renew hangs indefinitely → is_leader stays true
#   → rio_scheduler_lease_lost_total never increments on the
#   partitioned pod → test fails on the metric assertion.
with subtest("apiserver-partition: local self-fence fires within TTL"):
    leader = leader_pod()
    # Resolve which k3s node runs the leader pod → which VM Machine
    # to inject the iptables rule on. (k3s-server hosts the apiserver,
    # so partition the AGENT if the leader is there — partitioning
    # the server from itself is nonsensical. If the leader is on the
    # server, this subtest skips with a diagnostic; antiAffinity means
    # we have a 50% shot per run. Alternatively: force leader onto
    # agent first via `kubectl delete` + wait.)
    leader_node = kubectl(
        f"get pod {leader} -o jsonpath='{{.spec.nodeName}}'"
    ).strip()
    if leader_node == "k3s-server":
        # Force leader to agent: delete the server-resident pod,
        # wait for failover, re-query.
        kubectl(f"delete pod {leader} --grace-period=0 --force")
        k3s_server.wait_until_succeeds(
            f"h=$(k3s kubectl -n {ns} get lease rio-scheduler-leader "
            f"-o jsonpath='{{.spec.holderIdentity}}'); "
            f"test -n \"$h\" && test \"$h\" != '{leader}'",
            timeout=45,
        )
        leader = leader_pod()
        leader_node = kubectl(
            f"get pod {leader} -o jsonpath='{{.spec.nodeName}}'"
        ).strip()
    assert leader_node == "k3s-agent", f"leader still on server: {leader_node}"

    # Snapshot lost_total BEFORE partition (via pods/proxy — works
    # while the agent is reachable; we partition outbound 6443, not
    # inbound metrics).
    lost_before = metric_value(
        parse_prometheus(k3s_server.succeed(
            f"k3s kubectl get --raw "
            f"'/api/v1/namespaces/{ns}/pods/{leader}:9091/proxy/metrics'"
        )),
        "rio_scheduler_lease_lost_total",
    ) or 0.0

    # Partition: block outbound to apiserver (6443) from the agent.
    # DROP not REJECT: REJECT sends ICMP unreachable → kube client
    # gets a fast connection-refused → Err arm fires immediately
    # (tests self-fence but NOT the timeout wrapper). DROP makes
    # the socket hang → exercises the tokio::time::timeout path.
    k3s_agent.succeed("iptables -I OUTPUT -p tcp --dport 6443 -j DROP")

    try:
        # Standby (on server, unpartitioned) should steal within
        # LEASE_TTL + one poll = ~20s. 30s budget for TCG variance.
        k3s_server.wait_until_succeeds(
            f"h=$(k3s kubectl -n {ns} get lease rio-scheduler-leader "
            f"-o jsonpath='{{.spec.holderIdentity}}'); "
            f"test -n \"$h\" && test \"$h\" != '{leader}'",
            timeout=30,
        )

        # LOCAL self-fence: partitioned leader's lost_total incremented.
        # Scrape via pods/proxy — kubelet→pod traffic is intra-node,
        # unaffected by the 6443 OUTPUT rule. This is the assertion
        # that FAILS pre-fix: without the timeout + self-fence, the
        # partitioned scheduler still believes is_leader=true and
        # never increments this counter.
        #
        # Wait up to LEASE_TTL + RENEW_INTERVAL + slack = ~25s from
        # partition start for the self-fence to fire.
        k3s_server.wait_until_succeeds(
            f"lost=$(k3s kubectl get --raw "
            f"'/api/v1/namespaces/{ns}/pods/{leader}:9091/proxy/metrics' "
            f"| grep '^rio_scheduler_lease_lost_total' | awk '{{print $2}}'); "
            f"test \"${{lost:-0}}\" != \"{int(lost_before)}\"",
            timeout=30,
        )
    finally:
        k3s_agent.succeed("iptables -D OUTPUT -p tcp --dport 6443 -j DROP")

    # Recovery: after unblock, partitioned pod's next tick succeeds
    # (Standby), is_leader stays false (correct — someone else holds).
    # Wait for 2/2 Ready before the next subtest.
    k3s_server.wait_until_succeeds(
        f"k3s kubectl -n {ns} wait --for=condition=Available "
        f"deploy/rio-scheduler --timeout=60s",
        timeout=90,
    )
```

Add to `vm-le-stability-k3s`'s subtest list in `nix/tests/default.nix` after `failover`. Budget: +~60s (30s partition wait + 30s recovery).

**Caveat:** kubelet on the partitioned agent also loses apiserver connectivity → it may mark the pod NotReady after its own node-status grace period (~40s default). The 30s timeout on the metric wait should land before that. If it races, add `--node-status-update-frequency` tuning to the k3s-agent config, or scrape via the pod's hostPort instead of `pods/proxy`.

---

## Spec updates

`docs/src/components/scheduler.md:496` currently says:

> **Transient API errors:** On apiserver errors, the loop logs a warning and retries on the next tick without flipping `is_leader`.

Update to reflect the self-fence:

> **Transient API errors:** Each renew attempt is bounded by a `RENEW_INTERVAL - 2s` deadline. On error or timeout, the loop logs a warning and retries on the next tick. `is_leader` is **not** flipped for the first few failures (the lease may still be held; one or two apiserver hiccups are expected). If `LEASE_TTL` elapses with no successful round-trip, the loop flips `is_leader=false` locally — by that point any unpartitioned replica has already stolen, and continuing to dispatch would only generate stale-generation noise at workers.

Run `tracey bump` after editing — `r[sched.lease.k8s-lease]` text changes meaningfully.

---

## Checklist

- [ ] Add `RENEW_SLOP` const + `tokio::time::timeout` wrapper at `lease/mod.rs:273`
- [ ] Add `last_successful_renew: Instant` tracking, reset on every `Ok(Ok(_))`
- [ ] Collapse `Ok(Err)` | `Err(Elapsed)` into one error arm with self-fence check
- [ ] Snapshot `generation` at `handle_leader_acquired` entry; early-return if changed before `store(true)` (both Ok and Err arms)
- [ ] Clear DAG on early-return so interleaved Ticks see consistent state
- [ ] Unit: `renew_timeout_fires_on_hung_apiserver` (empty-scenario verifier trick)
- [ ] Unit: `self_fence_flips_is_leader_after_ttl_of_failures` + negative (`_before_ttl`)
- [ ] Unit: `recovery_discards_on_generation_bump_mid_flight` + negative (clean recovery)
- [ ] Unit: stale-DAG dispatch probe (no assignment sent after discard)
- [ ] VM subtest `apiserver-partition` in `leader-election.nix`, add to `vm-le-stability-k3s`
- [ ] Spec update `scheduler.md:496` + `tracey bump`
- [ ] `nix develop -c cargo nextest run -p rio-scheduler`
- [ ] `nix-build-remote --no-nom --dev -- -L .#checks.x86_64-linux.vm-le-stability-k3s`
