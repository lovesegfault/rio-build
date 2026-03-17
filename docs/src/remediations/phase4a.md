# Phase 4a Remediation Report

**Date:** 2026-03-17
**Scope:** 10 crates, 8 dependency boundaries, ~74k LoC
**Methodology:** 18 independent analysis agents + 8 verification/implications agents, cross-referenced findings

---

## Executive Summary

This report collates 180+ findings from a coordinated multi-agent audit of the phase4a codebase. It is the authoritative remediation plan driving the next ~30 remediation PRs.

**Headline numbers:**

| Severity | Count | Character |
|---|---|---|
| CRITICAL (P0) | 6 themes | Data loss, spurious success, silent corruption — require immediate fix before any phase4a→prod traffic |
| HIGH (P1) | ~25 | Correctness under fault, protocol desync, resource exhaustion — fix before phase4b |
| MEDIUM (P2) | ~40 | Robustness, observability gaps, latent flake — schedule within phase4b |
| LOW (P3) | ~60 | Test coverage, doc accuracy, code quality — opportunistic |

**Audit highlights:**

- **3 of 5 prior-known bugs already fixed** — `phase4-plan-research.md` memory is stale on `cancel_signals_total` name mismatch, `BuildInfo.tenant_id` type, and `HeartbeatRequest.resources` plumbing (see [§6](#section-6-stale-memory-corrections)).
- **1 major severity downgrade after independent verification** — FOD modular hash bug (flagged CRITICAL by two agents) is real but practically LOW: the Nix client discards the hash on the wire ([§5](#section-5-downgraded--refuted)).
- **Steady-state P0 found** — `sched-reap-collateral-poisoned` requires NO crash, NO failure. First build cleanup post-recovery reaps all recovered-poisoned nodes. This is the most urgent finding.
- **Positive audit coverage** — 8 classes of suspected-bad patterns verified correct across all sites ([§7](#section-7-positive-audit-results)).

**Blast radius summary for P0:**

| Theme | User-visible symptom | Detection gap |
|---|---|---|
| Spurious Succeeded | `nix build` reports success, path invalid at USE site | No metric distinguishes `0/0 derivations` from real success |
| Empty references | GC sweeps live paths; signatures bound to empty refs | Silent until first sweep past `grace_hours=2` |
| Stuck Build CR | Build stuck at `phase=Pending` forever | No alert on stale Pending |
| Eager connect | CrashLoopBackOff on cold cluster start | Visible but misdiagnosed as "flaky startup" |
| Coverage session drop | All GHA jobs green, Codecov shows 46.6% | 15 uploads logged "complete", server retained 1 |
| Realisation wire format | CA feature 100% broken for real clients | Test mocks lack `validate_store_path`; VM test asserts wrong opcode |

---

## Section 1: CRITICAL remediations (P0)

P0 findings are correctness violations that produce incorrect results (not just crashes) under realistic conditions. Each subsection is self-contained: a reader who opens only that subsection can understand the bug and the fix.

---

### 1.1 Scheduler recovery: spurious Succeeded on poisoned-orphan

**Status:** VERIFIED CRITICAL by 3 independent agents.
**Findings consolidated:** 10 (sched-poisoned-orphan-spurious-succeeded, sched-id-to-hash-missing-poisoned, sched-recovery-comment-lies, sched-reap-collateral-poisoned, sched-cancel-loop-crash-window, sched-transition-build-inmem-first, pg-poisoned-status-without-poisoned-at, sched-check-build-completion-zero-zero, sched-recovery-bd-rows-continue, sched-stderr-last-empty-built-outputs)

#### The core bug

`rio-scheduler/src/actor/recovery.rs:120-146` — `load_poisoned_derivations()` iterates `PoisonedDerivationRow`s and inserts them into the DAG, but does **not** insert into the `id_to_hash` map:

```rust
// recovery.rs:120-146 (current)
for row in poisoned_rows {
    let hash = row.derivation_hash.clone();
    let node = DerivationNode::from_poisoned_row(&row);
    self.dag.insert_node(hash, node);
    // MISSING: self.id_to_hash.insert(row.derivation_id, hash);
}
```

Downstream at `recovery.rs:168`, the `bd_rows` loop (joining `builds` ↔ `derivations` via `build_derivations`) does this:

```rust
// recovery.rs:168 (paraphrased)
let Some(hash) = self.id_to_hash.get(&bd_row.derivation_id) else {
    continue;  // ← poisoned drv falls through here
};
build.derivation_hashes.push(hash.clone());
```

So a build whose only remaining derivation was poisoned gets `derivation_hashes = vec![]`. Then `check_build_completion` (completion.rs) computes:

```
total = derivation_hashes.len() = 0
completed = 0
failed = 0
→ completed >= total && failed == 0
→ BuildStatus::Succeeded
```

The build transitions to Succeeded. The gateway receives `status=Built` with `built_outputs=[]`. The Nix client accepts this and fails later at the **use site** with "path is not valid" — a completely different component, making root-cause analysis painful.

#### Three entry routes

The poisoned-orphan state can be reached **three ways**, which is why this is a theme and not a single finding:

**Route 1 — Crash window between poison-persist and failure-handle (completion.rs:445-459)**

```rust
// completion.rs:445-459 (paraphrased)
self.db.persist_status(drv_id, DerivationStatus::Poisoned).await?;  // :445
self.db.set_poisoned_at(drv_id, now).await?;                         // :447
// ... 4 await points follow, including a rio-store RPC ...
self.handle_derivation_failure(drv_hash, build_id).await?;           // :459
```

A crash anywhere between :445 and :459 leaves PG with `status=Poisoned` but the build row still `Active`. Recovery loads the poisoned drv (without `id_to_hash` entry) and the active build — spurious Succeeded.

The 4 await points include a network RPC to rio-store, so this window is not microseconds — it's tens to hundreds of milliseconds under load, and unbounded under rio-store slowness.

**Route 2 — SIBLING: Crash mid-cancel-loop (build.rs:111→146)**

```rust
// build.rs:111-146 (paraphrased)
for drv_hash in &build.derivation_hashes {
    self.db.persist_status(drv_id, DerivationStatus::Cancelled).await?;  // :111
    // ... crash here ...
}
self.db.transition_build(build_id, BuildStatus::Cancelled).await?;       // :146
```

A crash partway through the cancel loop leaves some derivations `Cancelled` in PG but the build still `Active`. Same `id_to_hash` gap on recovery → same spurious Succeeded. Cancelled builds become Succeeded.

**Route 3 — NO-CRASH variant: in-memory-first ordering (build.rs:414,442)**

```rust
// build.rs:414-443 (paraphrased)
self.builds.get_mut(&build_id).unwrap().status = new_status;  // :414 in-mem FIRST
// ...
self.db.transition_build(build_id, new_status).await?;        // :442 PG SECOND
```

`transition_build` mutates in-memory state **before** writing PG. A single PG timeout (not a crash — the scheduler keeps running) leaves:

- In-memory: `status = Succeeded` (or Failed, or Cancelled — terminal)
- PG: `status = Active`

The scheduler continues operating on the in-memory terminal state. On the **next** restart (could be hours later, for an unrelated reason), recovery reads PG `status=Active`, loads the build, finds zero non-terminal derivations, and spuriously succeeds it.

This is the most insidious route because the corruption is **latent** — it plants a landmine that detonates on an unrelated restart.

For contrast: `handle_clear_poison` (same file, ~100 lines down) already does PG-first ordering. The pattern is known; `transition_build` just didn't get it.

#### Plus the steady-state bug: `sched-reap-collateral-poisoned`

This one requires **no crash, no failure, no race**. It happens on the **happy path** after every recovery.

`rio-scheduler/src/dag/mod.rs:503-519` — `remove_build_interest_and_reap()` walks the DAG after any build completes and reaps nodes where:

```rust
node.interested_builds.is_empty() && node.status.is_terminal()
```

`rio-scheduler/src/dag/derivation.rs:416` — `DerivationNode::from_poisoned_row()` constructs recovered-poisoned nodes with:

```rust
interested_builds: HashSet::new(),  // ← empty from birth
```

So after recovery, **all** recovered-poisoned nodes have `interested_builds=∅` and `status=Poisoned` (terminal). The **first** build completion of any kind post-recovery triggers `remove_build_interest_and_reap`, which walks the DAG and reaps every recovered-poisoned node.

This destroys poison TTL tracking. The poison-TTL feature (derivations remain poisoned for N hours to prevent retry storms) is silently disabled after every scheduler restart.

#### The lying comment

`recovery.rs:169-174` has a comment justifying the `continue`:

```rust
// check_build_completion only counts non-terminal derivations anyway,
// so omitting poisoned derivations from derivation_hashes is safe.
```

This is **false**. Poisoned is failure-terminal. `check_build_completion` counts it in `failed`, not in "ignored". The comment describes what the author **intended**, not what the code does. Delete it.

#### Downstream blast radius

| Component | Symptom |
|---|---|
| Nix client | `status=Built`, `built_outputs=[]`, then "path is not valid" at first `nix copy` or substituter fetch |
| K8s Event | `"Build completed successfully (0/0 derivations)"` — visible in `kubectl describe build` |
| Metric | `rio_scheduler_builds_total{outcome="success"}` incremented; SLO dashboard pollution |
| PG | `builds` row permanently `Succeeded`, `build_derivations` rows point to poisoned/cancelled drvs |

The `(0/0 derivations)` Event text is the **one reliable grep** for detecting this in prod logs retroactively.

#### Fix plan

**Atomic PR (ship as one unit — fixes the keystone + closes the primary window):**

1. **`recovery.rs:120-146`** — insert `row.derivation_id` into `id_to_hash` inside the poisoned-load loop. This is the keystone: it fixes the `bd_rows` `continue` fallthrough, which fixes the `0/0` arithmetic, which fixes the spurious Succeeded — for all three entry routes simultaneously.

   ```rust
   for row in poisoned_rows {
       let hash = row.derivation_hash.clone();
       let node = DerivationNode::from_poisoned_row(&row);
       self.id_to_hash.insert(row.derivation_id, hash.clone());  // ADD
       self.dag.insert_node(hash, node);
   }
   ```

2. **`completion.rs:445-447`** — combine `persist_status(Poisoned)` and `set_poisoned_at` into a single SQL statement. Eliminates the `pg-poisoned-status-without-poisoned-at` window where a crash between the two leaves `status=Poisoned` but `poisoned_at=NULL`.

   ```sql
   UPDATE derivations SET status = 'Poisoned', poisoned_at = NOW() WHERE id = $1
   ```

3. **`db.rs:504`** — the poisoned-load query has `WHERE status = 'Poisoned' AND poisoned_at IS NOT NULL`. Either drop the `AND poisoned_at IS NOT NULL` filter (once step 2 makes them atomic, it's redundant) OR add a third defensive query for `status='Poisoned' AND poisoned_at IS NULL` rows that treats them as "poison-now" (sets `poisoned_at = recovery time`).

4. **`recovery.rs:169-174`** — delete the lying comment. Replace with:

   ```rust
   // Poisoned derivations are failure-terminal. They MUST appear in
   // derivation_hashes so check_build_completion counts them in `failed`.
   // Reaching this branch means id_to_hash is incomplete — bug.
   warn!(derivation_id = %bd_row.derivation_id, "bd_row derivation not in id_to_hash");
   ```

**Stageable follow-ups (can ship separately, in this order):**

5. **`build.rs:414-443`** — PG-first ordering in `transition_build`. Copy the pattern from `handle_clear_poison`. On PG error, do not mutate in-memory state.

6. **`dag/mod.rs:508`** — in `remove_build_interest_and_reap`, only reap nodes whose `interested_builds` **previously contained** `build_id`. Change the predicate from "is empty now" to "was made empty by this removal":

   ```rust
   let was_interested = node.interested_builds.remove(&build_id);
   if was_interested && node.interested_builds.is_empty() && node.status.is_terminal() {
       // reap
   }
   ```

7. **`build.rs:78-147`** — cancel path: either invert the order (transition build first, then derivations) OR add a recovery query for the inconsistent state `derivation.status=Cancelled ∧ build.status=Active` that finishes the cancel.

#### Tests

- **Fault-injection test** for each of the three crash windows. Use the existing PG failpoint harness; inject `FAIL_AFTER(n)` at each `.await` boundary, restart the actor, assert final build status is `Failed` (not `Succeeded`).
- **Steady-state test** for `sched-reap-collateral-poisoned`: recover with 3 poisoned nodes, complete one unrelated build, assert all 3 poisoned nodes still present in DAG.
- **Tracey rule:** add `r[sched.recovery.poisoned-failed-count]` to the scheduler spec ("recovered builds whose derivations are all failure-terminal MUST transition to Failed, not Succeeded"). Annotate the fix with `r[impl ...]` and the fault-injection test with `r[verify ...]`.

---

### 1.2 Worker empty references → GC data loss + signature + sandbox breakage

**Status:** CROSS-VERIFIED CRITICAL by 3 independent agents.
**Findings consolidated:** 5 (wkr-upload-refs-empty, store-gc-mark-walks-empty, store-sign-fingerprint-empty-refs, wkr-sandbox-bindmount-missing-transitive, wkr-upload-deriver-empty)

#### The core bug

`rio-worker/src/upload.rs:224` — every uploaded output path gets an unconditionally empty reference list:

```rust
// upload.rs:224 (current)
let path_info = PathInfo {
    // ...
    references: Vec::new(),   // ← UNCONDITIONALLY EMPTY
    deriver: String::new(),   // ← :223, also wrong — drv_path is known one frame up
    // ...
};
```

Grep confirms: **no NAR content scanner exists anywhere in the codebase.** The `references` field is never populated by anything other than `Vec::new()` on the worker→store upload path.

This is not a "we forgot to wire it up" bug — the scanner doesn't exist. It was never built.

#### Three blast radii

**Blast radius 1: GC data loss (the worst one)**

`rio-store/src/gc/mark.rs:102` — the mark phase's edge-walk is a single SQL query:

```sql
SELECT unnest(n."references") FROM nodes n WHERE n.store_path = ANY($1)
```

This is the **only** edge traversal in GC mark. With `references = []` on every node:

- Mark phase visits pin roots → zero edges → mark set = {pins}
- Sweep phase deletes everything `NOT IN mark_set AND age > grace_hours`

Default `grace_hours = 2`. Every unpinned path older than 2 hours is swept.

This is silent until the first sweep runs, at which point it's catastrophic: the store discards all transitive dependencies of everything.

**Blast radius 2: Signatures permanently bound to empty refs**

`rio-store/src/grpc/mod.rs:204-224` — `maybe_sign()` computes the Nix signature fingerprint **before** the PG write:

```rust
// grpc/mod.rs:204-224 (paraphrased)
let fingerprint = format!(
    "1;{};{};{};{}",
    store_path, nar_hash, nar_size,
    references.join(",")   // ← joins empty Vec → ""
);
let sig = self.signing_key.sign(fingerprint.as_bytes());
// ... then persist sig to PG
```

The signature is a commitment to `references=""`. If we later fix the reference scanner and backfill refs in PG, **every existing signature becomes invalid** — the fingerprint they commit to no longer matches the stored `references` column.

This means the fix requires a **re-sign sweep job**, not just a scanner.

**Blast radius 3: Sandbox bind-mount failure**

Per `docs/src/components/worker.md:215`, the worker's sandbox setup bind-mounts the output's reference closure so the builder can see its runtime dependencies. With empty refs, transitive deps aren't mounted.

Symptom: builder fails with `No such file or directory` pointing at its own dynamic linker (e.g. `/nix/store/...-glibc-.../lib/ld-linux-x86-64.so.2`). This is a very confusing error to root-cause if you don't know refs are empty.

This blast radius is **currently latent** — it only triggers for builds that *consume* outputs of *other* rio builds (not substituter-sourced paths, which have correct refs from the binary cache). The first multi-stage rio-native build will hit it.

#### Sibling finding: `deriver` also empty

`upload.rs:223` — `deriver: String::new()`. The `drv_path` is in scope one stack frame up in `upload_outputs()`; it just wasn't plumbed through. Zero-cost fix.

#### Fix plan

1. **Implement NAR content scanner.** Scan the output NAR for 32-char nixbase32 hash sequences that match any hash in the candidate set:

   ```
   candidate_set = resolved_input_srcs ∪ drv.outputs()
   ```

   (Self-references are legal — an output can reference itself.)

   The scanner doesn't need to scan the entire Nix store — only the input closure. False positives are impossible within the candidate set (32-char collision is cryptographically negligible); false negatives are possible only for paths that embed their deps in compressed/encrypted form (rare, and Nix itself has the same limitation).

   Implementation: tee alongside the existing `HashingChannelWriter` — as NAR bytes stream through on their way to the gRPC upload, a parallel Aho-Corasick matcher over the candidate hash set records hits. No second read pass.

2. **Plumb `deriver`.** Add `drv_path: &StorePath` parameter to `upload_single_output`, pass it through from `upload_outputs` (which already has it).

3. **Re-sign sweep job.** After the scanner ships and backfills refs (via a one-shot migration job that re-scans NARs from the content-addressed blob store), run a sweep that re-signs every path whose current signature's fingerprint doesn't match its (now-corrected) refs. This must be an explicit admin action — it's the only way to recover correctness for paths uploaded pre-fix.

4. **Defensive log.** In `maybe_sign`, before signing a non-CA path with zero refs:

   ```rust
   if !is_content_addressed && references.is_empty() {
       warn!(store_path = %path, "signing non-CA path with empty references — GC will not protect deps");
   }
   ```

   This turns the silent failure mode into a loud one without blocking the upload.

#### Tests

- Unit: scan a NAR containing known 32-char hashes from a candidate set, assert exact match set.
- Unit: scan a NAR with a hash-like 32-char sequence NOT in the candidate set, assert no false positive.
- Integration: build a 2-stage derivation (A depends on B), assert A's uploaded `references` contains B's store path.
- Integration: GC mark with a 2-stage dep chain, assert B survives when A is pinned.

---

### 1.3 Controller stuck-Build: stream-drop-before-first-event

**Status:** CROSS-VERIFIED CRITICAL by 2 independent agents.
**Findings consolidated:** 7 (ctrl-drain-stream-early-exit-no-patch, ctrl-unknown-phase-infinite-cycle, ctrl-reconnect-exhaust-wipes-status, kube-status-ssa-wipes-cached-count, tonic-controller-watchbuild-retry-burns-on-dead-stream, ctrl-no-periodic-requeue, ctrl-no-owns-watch)

#### The core bug

`rio-controller/src/reconcilers/build.rs:861-864` — when the SubmitBuild stream drops before yielding its first event, `drain_stream` does this:

```rust
// build.rs:861-864 (current)
Ok(None) => {
    self.watch_keys.lock().remove(&watch_key);
    return;  // ← patches NOTHING on the Build CR
}
```

By this point:

- `apply()` has already returned `Action::await_change()` — no scheduled requeue
- The reconciler has no `.owns()` on any child resource — no watch-triggered reconcile
- No periodic fallback requeue is configured

So the Build CR is stuck at `phase=Pending, build_id="submitted"` (the sentinel value written before stream open) **forever**. The only way to unstick it is to `kubectl edit` something on the CR to trigger a reconcile.

The trigger — scheduler accepting the stream, then dropping it before sending event 0 — is not exotic. It happens on:
- Scheduler SIGTERM between stream-accept and first-event-send (rollout)
- Scheduler panic on the submit path
- Network partition mid-handshake

#### Plus the infinite cycle: `ctrl-unknown-phase-infinite-cycle`

When `drain_stream` exhausts `MAX_RECONNECT` (see retry-budget finding below), it patches `phase=Unknown`:

```rust
// build.rs:419-428 (paraphrased)
patch_status(BuildStatus {
    phase: BuildPhase::Unknown,
    ..Default::default()
})
```

`BuildPhase::Unknown` is **non-terminal** in the reconciler's phase match. Non-terminal → `apply()` spawns a new reconnect task → which exhausts again (the underlying condition hasn't changed) → patches Unknown again → triggers reconcile → spawns reconnect → ...

Loop period ≈ 31s (`MAX_RECONNECT * backoff_max + patch latency`). Runs forever, burning a reconcile worker slot and generating a patch every 31 seconds.

#### Plus status wipe: `ctrl-reconnect-exhaust-wipes-status` and `kube-status-ssa-wipes-cached-count`

That same patch at `build.rs:419-428`:

```rust
patch_status(BuildStatus {
    phase: BuildPhase::Unknown,
    ..Default::default()   // ← wipes progress, conditions, started_at
})
```

With SSA `.force()`, the server-side apply **replaces** the status subresource with what the controller sends. `..Default::default()` sends zero-valued `progress`, empty `conditions`, zero `started_at`.

Separately, `cached_derivations: u32` has no `#[serde(skip_serializing_if = "is_zero")]`. So even on **successful** reconnect (not just exhaustion), the first patch after reconnect sends `cached_derivations: 0`, overwriting the previously-correct count.

User-visible: `kubectl get build` shows progress going **backward** (e.g. `47/100` → `0/100`) on every scheduler rollout.

#### Plus retry budget halved: `tonic-controller-watchbuild-retry-burns-on-dead-stream`

`build.rs:882-898` — when WatchBuild returns `Err`, the code logs it and falls through to the next loop iteration **without replacing `stream`**:

```rust
// build.rs:882-898 (paraphrased)
Err(status) => {
    warn!(error = %status, "WatchBuild error");
    attempts += 1;
    // ← stream NOT replaced
}
// next iteration reads from the SAME dead stream → Ok(None) → attempts += 1
```

Every WatchBuild error burns **two** attempts: one for the error, one for the subsequent `Ok(None)` from the dead stream. `MAX_RECONNECT=5` effectively becomes `MAX_RECONNECT≈2.5`.

#### Fix plan

1. **`build.rs:863`** — before `return`, patch `build_id=""` (clears the `"submitted"` sentinel). This triggers a reconcile (status change), and the reconciler's next `apply()` will re-submit (because `build_id` is empty).

   ```rust
   Ok(None) => {
       self.watch_keys.lock().remove(&watch_key);
       // Clear the sentinel so the next reconcile re-submits.
       let _ = patch_build_id(&self.api, &name, "").await;
       return;
   }
   ```

2. **Add `#[serde(skip_serializing_if = "is_zero")]`** to `cached_derivations`, `total_derivations`, `completed_derivations` in `BuildStatus`. SSA will then preserve existing values instead of stomping with zero.

   ```rust
   #[serde(default, skip_serializing_if = "is_zero")]
   pub cached_derivations: u32,
   ```

3. **`build.rs:882-898`** — after logging a WatchBuild `Err`, `continue` directly to the reconnect logic at the top of the loop instead of falling through to another read on the dead stream. Alternatively: set `stream = None` and have the top of the loop reconnect when stream is None.

4. **Treat `Unknown` as terminal-for-reconnect** OR add a reconnect-cycle counter and stop after N full exhaustion cycles. Simplest: make the Unknown-phase match arm in `apply()` NOT spawn a new drain task (it should just `await_change()` and let a human or admission controller intervene).

5. **`build.rs:419-428`** — don't use `..Default::default()` on exhaustion patch. Only set `phase` and `message`:

   ```rust
   patch_status_partial(|s| {
       s.phase = BuildPhase::Unknown;
       s.message = Some("reconnect exhausted after ...".into());
   })
   ```

#### Tests

- Integration: open SubmitBuild stream, close it before first event, assert Build CR eventually re-submits (poll for `build_id != "submitted" && build_id != ""`).
- Integration: inject WatchBuild error, assert exactly ONE attempt is consumed (check `rio_controller_watch_reconnects_total`).
- Unit: serialize `BuildStatus` with `cached_derivations=0`, assert field absent from JSON.

---

### 1.4 Eager connect without retry: gateway + worker

**Status:** VERIFIED CRITICAL. Pattern documented, fix never propagated.
**Findings consolidated:** 2 (tonic-gateway-eager-connect-no-retry, tonic-worker-eager-connect-no-retry)

#### The bug

The controller got this fix at `rio-controller/src/main.rs:192-232`. There's a comment right there citing the 2026-03-16 coverage-full failures that motivated it. The fix: a retry loop around `.connect().await` with backoff, placed **before** the health server starts.

**The gateway and worker never got this fix.**

| Binary | Connect sites | Health server | Failure mode |
|---|---|---|---|
| `rio-controller/src/main.rs` | `:192-232` — retry loop, fixed | After connects | ✓ Correct |
| `rio-gateway/src/main.rs` | `:202, :217, :230` — bare `.connect().await?` | `:261` — AFTER connects | Process exit on no-endpoints → CrashLoopBackOff |
| `rio-worker/src/main.rs` | `:157, :161, :170` — bare `.connect().await?` | `:147` — BEFORE connects | Liveness passes while process exit-races; kubelet sees flapping |

The worker case is slightly worse: because the health server starts at `:147` (before connects), there's a window where liveness reports healthy, then the process exits on connect failure, then kubelet restarts it, then liveness reports healthy again... The restart is silent from the readiness probe's perspective.

#### Trigger

Cold cluster start. When all pods start simultaneously (fresh deploy, node drain + reschedule, cluster-wide restart), the `rio-store` and `rio-scheduler` Services have **no endpoints** for the first few seconds (their pods are also starting). `.connect().await` on a Service with no endpoints returns `Err` immediately (not hang — that's the stale-IP case, which is different).

Gateway and worker both die, restart, die again. If the store/scheduler take >30s to become ready (PG migration, large recovery), the gateway/worker enter CrashLoopBackOff and stay there for the exponential backoff duration even after store/scheduler are up.

#### Fix plan

Copy the controller's retry loop. It's already proven correct in this codebase.

```rust
// Pattern from rio-controller/src/main.rs:192-232
async fn connect_with_retry(
    endpoint: Endpoint,
    name: &str,
    shutdown: &CancellationToken,
) -> anyhow::Result<Channel> {
    let mut backoff = Duration::from_millis(100);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => anyhow::bail!("shutdown during {name} connect"),
            result = endpoint.connect() => match result {
                Ok(ch) => return Ok(ch),
                Err(e) => {
                    warn!(error = %e, backoff_ms = backoff.as_millis(), "connect {name} failed, retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }
}
```

Place the health server start **after** all retry loops complete, OR start it before but gate `ready=false` until connects succeed. The former is simpler; the latter gives better observability (you can `kubectl get pods` and see `0/1 Ready` instead of `CrashLoopBackOff`).

Extract to `rio-common` so there's one implementation, not four.

#### Tests

- VM test: start gateway before store, assert gateway eventually connects (no CrashLoopBackOff).
- Already have a near-equivalent for controller in `nix/tests/vm/phase3a/lifecycle-core.nix` — extend to gateway/worker.

---

### 1.5 Coverage session drop: 15→1

**Status:** VERIFIED CRITICAL (for development signal, not runtime).
**Findings consolidated:** 1 (ci-codecov-after-n-builds-missing)

#### The bug

Commit `9937257` split the VM test monoliths 3→9 fragments. Combined with the existing 6 unit-coverage uploads, total Codecov uploads went **9 → 15**.

`codecov.yml` has no `notify.after_n_builds` setting. Without it, Codecov's default behavior under upload race is **last-writer-wins per commit SHA**. With 15 parallel uploads racing, the server silently retained only **one**: `vm-scheduling-core-standalone`, reporting **46.6%**.

The real combined number (per `nix-build-remote -- .#coverage-full`) is **~93.7%** as of commit `7c437e1`.

**Why this wasn't caught:**

- All 15 GitHub Actions jobs reported green.
- All 15 logged `"Process Upload complete"` (that's the client-side log, not server-side acknowledgment).
- The Nix wiring (`nix/coverage.nix`) was audited and is **correct** — each fragment produces a valid lcov, and `lcov -a` merges them properly when run locally.
- `codecov.yml:12` has a comment: `# deliberately not setting after_n_builds — we want per-upload feedback`. This comment reflects a decision made when there were 3 uploads and no race. At 15, the race is guaranteed.

#### Fix plan

```yaml
# codecov.yml
codecov:
  notify:
    after_n_builds: 15  # matches CI matrix: 6 unit + 9 VM fragments
                         # MUST be updated when nix/tests/vm/ fragment count changes
```

Plus hardening on the GHA side:

```yaml
# .github/workflows/coverage.yml
- uses: codecov/codecov-action@v4
  with:
    fail_ci_if_error: true    # fail the job if upload rejected, don't just log
    disable_search: true       # we pass files explicitly; don't auto-discover
    files: ${{ matrix.lcov_path }}
```

Consider deriving `after_n_builds` from the matrix size to avoid drift:

```yaml
env:
  CODECOV_EXPECTED_UPLOADS: ${{ strategy.job-total }}
```

(Not all Codecov action versions support env-var substitution here — verify before relying on it.)

#### Tests

No test per se, but add a CI assertion: after all coverage jobs complete, a final job queries the Codecov API for the commit SHA and asserts `totals.coverage > 85`. This catches future regressions of the same class.

---

### 1.6 (HIGH, was CRITICAL) Realisation outPath: BOTH directions broken

**Status:** VERIFIED real, DOWNGRADED to HIGH.
**Downgrade rationale:** CA derivations are experimental, gated behind `--experimental-features ca-derivations`, and the `realisations` PG table in prod is empty (verified). No user traffic has hit this code path.
**Findings consolidated:** 4 (wire-realisation-outpath-full-path, wire-realisation-register-basename-rejected, test-mock-store-no-validate-store-path, test-vm-protocol-wrong-opcode)

#### The query-direction bug

`rio-gateway/src/handler/opcodes_read.rs:558` — `wopQueryRealisation` response serializes the Realisation as JSON with:

```rust
// opcodes_read.rs:558 (current)
"outPath": realisation.out_path,  // e.g. "/nix/store/abc...-foo"
```

Real Nix clients expect a **basename** (e.g. `"abc...-foo"`). The client's deserializer feeds `outPath` to `StorePath::parse`, which rejects `/` with `"illegal base-32 char '/'"`.

The **correct** pattern already exists one file over at `rio-nix/src/protocol/build.rs:415-418`, with a comment explaining exactly this:

```rust
// build.rs:415-418 — CORRECT
// Realisation JSON outPath is a BASENAME, not a full store path.
// Real clients call StorePath::parse on it; the leading /nix/store/ is illegal.
let basename = out_path.strip_prefix("/nix/store/").unwrap_or(&out_path);
```

Same pattern, one file has the fix, one doesn't.

#### The register-direction bug (NEW — found by verification agent)

`opcodes_read.rs:486` — `wopRegisterDrvOutput` receives the client's Realisation JSON (which has basename `outPath`, as real clients send) and passes it **verbatim** to the store:

```rust
// opcodes_read.rs:486 (current)
let realisation: Realisation = serde_json::from_slice(&payload)?;
self.store.register_realisation(realisation).await?;  // ← basename passed through
```

`rio-store/src/grpc/mod.rs:464` — `validate_store_path()` (called inside `register_realisation`) requires the `/nix/store/` prefix:

```rust
// mod.rs:464
if !path.starts_with("/nix/store/") {
    return Err(Status::invalid_argument("not a store path"));
}
```

So `register_realisation` always fails for real clients. The gateway sends `STDERR_ERROR` back, the connection closes.

**The CA feature is 100% broken for real Nix clients in both directions, since introduction.**

#### Why tests didn't catch this

Three compounding test-fidelity failures:

1. **Test fixtures use full paths throughout.** `tests/wire_opcodes/realisation.rs` constructs `Realisation { out_path: "/nix/store/..." }` and asserts against `"/nix/store/..."`. Both directions of the bug cancel out inside the test.

2. **`MockStore.register_realisation` has no `validate_store_path`.** `rio-gateway/tests/common/grpc.rs:304-316` — the mock just stores whatever it's given. The real store's validation at `mod.rs:464` is never exercised.

3. **VM test asserts the wrong opcode.** `nix/tests/vm/phase2b/protocol.nix` has a subtest labeled `"Realisation outPath basename"` — but it exercises `wopBuildPathsWithResults` (opcode 47, `BuildResult` struct), not `wopQueryRealisation` (opcode 43, `Realisation` JSON). Commit `5786f82` fixed `write_build_result` only; the commit message says "fix Realisation outPath" but it's a **different struct with a coincidentally-identical field name**.

#### Fix plan

**Atomic — no PG migration needed (table is empty):**

1. **`opcodes_read.rs:486`** — prepend `/nix/store/` before calling the store. Copy the pattern from `build.rs:350-351`:

   ```rust
   let mut realisation: Realisation = serde_json::from_slice(&payload)?;
   if !realisation.out_path.starts_with("/nix/store/") {
       realisation.out_path = format!("/nix/store/{}", realisation.out_path);
   }
   self.store.register_realisation(realisation).await?;
   ```

2. **`opcodes_read.rs:558`** — strip `/nix/store/` before serializing. Copy the pattern from `build.rs:415-418`.

3. **Add `validate_store_path` to `MockStore.register_realisation`** at `tests/common/grpc.rs:304-316`. General rule: mocks should validate inputs the same way the real service does, or they mask input-shape bugs.

4. **Fix the three test files** (`tests/wire_opcodes/realisation.rs`, `tests/golden/realisation.rs`, `nix/tests/vm/phase2b/protocol.nix`) to use basenames in the wire-level assertions.

5. **Add golden conformance test** for opcodes 42 and 43 against a real `nix-daemon --stdio`. This is the only way to catch "our tests agree with each other but not with Nix" bugs.

---

## Section 2: HIGH remediations (P1)

P1 findings are correctness violations under fault, protocol desyncs under rare triggers, or resource exhaustion under load. They don't require an immediate production stop, but they must be fixed before phase4b hardening sign-off.

---

### 2.1 Gateway STDERR_ERROR desync (VERIFIED HIGH, currently latent)

**Findings:** gw-stderr-error-then-last-desync, gw-stderr-build-derivation, gw-stderr-build-paths-with-results

`rio-gateway/src/handler/build.rs:508-523` (`wopBuildDerivation`) and `:792-800` (`wopBuildPathsWithResults`) — on DAG-validation failure, the handler sends `STDERR_ERROR` **then** `STDERR_LAST` + a `BuildResult::failure` payload:

```rust
// build.rs:508-523 (paraphrased)
if let Err(e) = validate_dag(&drv) {
    stderr.error(&e.to_string()).await?;   // :510 — STDERR_ERROR (terminal frame)
    stderr.finish().await?;                // :515 — STDERR_LAST
    write_build_result(&mut w, &BuildResult::failure(...)).await?;  // payload
    return Ok(());
}
```

**`STDERR_ERROR` is a terminal frame.** Confirmed against Nix C++ source (`libstore/worker-protocol-connection.cc:72`): real `nix-daemon`'s `stopWork()` sends `STDERR_ERROR` **XOR** `STDERR_LAST`, never both. After the client receives `STDERR_ERROR`, it stops reading STDERR frames and throws.

So the `STDERR_LAST` + payload bytes that follow are **never consumed**. They sit in the TCP receive buffer. On the **next** opcode on the same pooled connection, the client reads those stale bytes as the new opcode's response → silent garbage-as-success.

**Why latent:** CppNix currently discards the connection on any daemon error (doesn't pool after errors). The trigger — `__noChroot` in a transitive dep, or a >100k-node DAG that fails validation — is rare. But pooling-after-error is a one-line client change, and DAG size grows with adoption.

**Self-incriminating:** `build.rs:160-164` has a comment explicitly documenting that `STDERR_ERROR` after `STDERR_LAST` is invalid. The comment is correct; the code 350 lines below it does the inverse (which is equally invalid).

**Spec rule gap:** `r[gw.stderr.error-before-return]` is one-directional ("if handler returns Err, send STDERR_ERROR first"). It doesn't forbid sending `STDERR_ERROR` and then `Ok()`. The rule needs to be bidirectional.

**Fix:**

1. **Drop `stderr.error()` calls at `:510-515` and `:794-799`.** Just let `BuildResult::failure` carry the error — the client sees it in `errorMsg`.

2. **Harden `StderrWriter`** to make this class of bug unrepresentable:
   - `stderr.rs:194` — `error()` sets `self.finished = true`
   - `stderr.rs:171` — `finish()` calls `self.check_not_finished()` at the top

   After this, `error()` followed by `finish()` panics in debug / returns `Err` in release.

3. **Update spec** `r[gw.stderr.error-before-return]` to be bidirectional: "handler sends `STDERR_ERROR` XOR `STDERR_LAST`, never both."

---

### 2.2 Lease renewal no timeout

**Findings:** kube-lease-renew-no-timeout, kube-lease-no-local-self-fence, sched-recovery-complete-toctou

`rio-scheduler/src/lease/election.rs:189,316` — `api.get_opt()` and `api.replace()` have no `tokio::time::timeout()` wrapper:

```rust
// election.rs:189 (current)
let current = self.api.get_opt(&self.lease_name).await?;  // ← no timeout
// ...
// election.rs:316
let updated = self.api.replace(&self.lease_name, &pp, &lease).await?;  // ← no timeout
```

If the apiserver hangs (network partition, apiserver overload, etcd slow), these calls block indefinitely. Meanwhile, the 15-second `LEASE_TTL` ticks down. At T+15s, another scheduler instance acquires the lease. The hung instance **still believes it's the leader** because its renew call hasn't returned yet.

**Plus `kube-lease-no-local-self-fence`** (`mod.rs:385-398`): when the renew call finally returns `Err` (after the apiserver unhang), the code logs the error but **doesn't flip `is_leader = false`** even though local wall-clock time is past `LEASE_TTL`:

```rust
// mod.rs:385-398 (paraphrased)
Err(e) => {
    warn!(error = %e, "lease renew failed");
    // ← no check: has LEASE_TTL elapsed since last_successful_renew?
    // ← no: self.is_leader.store(false)
}
```

The worker-side generation fence (workers reject assignments whose `generation` doesn't match the latest heartbeat response) saves **correctness** — stale-leader assignments are dropped. But the stale leader keeps **sending** them, wasting RPCs and filling logs.

**Plus `sched-recovery-complete-toctou`**: if the lease flaps (lose → reacquire) **during** recovery, the scheduler can set `recovery_complete = true` and start dispatching from a DAG that was loaded under a **previous** generation. The generation fence doesn't help here because the scheduler HAS the current generation — it just loaded state that belongs to the previous one.

**Fix:**

1. **Wrap `try_acquire_or_renew()` in timeout:**

   ```rust
   const RENEW_SLOP: Duration = Duration::from_secs(2);
   let renew_deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
   match timeout(renew_deadline, self.try_acquire_or_renew()).await {
       Ok(Ok(_)) => { self.last_successful_renew = Instant::now(); }
       Ok(Err(e)) | Err(_elapsed) => { /* local self-fence check */ }
   }
   ```

2. **Local self-fence:** track `last_successful_renew: Instant`. On any error path, if `last_successful_renew.elapsed() > LEASE_TTL`, flip `is_leader.store(false, Ordering::Release)`. Don't wait for a successful `get_opt` to tell you someone else has the lease — you already know you've lost it by the clock.

3. **CAS-equivalent generation check before `recovery_complete = true`:** capture the generation at recovery start; before storing `recovery_complete`, compare against the current generation. If they differ, the lease flapped — discard the loaded state and re-run recovery.

---

### 2.3 Build timeout orphans cgroup + status collapse

**Findings:** wkr-timeout-cgroup-orphan, wkr-status-collapse, wkr-timeout-is-infra

`rio-worker/src/executor/mod.rs:575-589` — on build timeout, `daemon.kill()` sends SIGKILL to the `nix-daemon --stdio` process **only**. The builder subtree (forked by the daemon into the build cgroup) is not killed.

`BuildCgroup::Drop` only does `rmdir()` on the cgroup directory. If the cgroup still has processes (which it does — we only killed the daemon, not the builder), `rmdir()` returns `EBUSY` and the cgroup leaks.

Next build of the **same derivation** on the **same worker** tries to create the same cgroup path → `EEXIST` → build fails immediately with a confusing "cgroup already exists" error.

**Plus `wkr-status-collapse`** (`mod.rs:736-744`): the status conversion from Nix's `BuildResultStatus` to rio's `BuildStatus` collapses **7 distinct Nix statuses** into `PermanentFailure`:

```rust
// mod.rs:736-744 (current)
_ => BuildStatus::PermanentFailure,  // catches TimedOut, LogLimitExceeded,
                                      // NotDeterministic, OutputRejected, ...
```

`TimedOut` and `LogLimitExceeded` are **operationally distinct** — they mean "increase the limit," not "the build is broken." Collapsing them into PermanentFailure makes triage harder and pollutes the `rio_worker_builds_total{outcome="permanent_failure"}` metric.

**Plus `wkr-timeout-is-infra`** (`stderr_loop.rs:95`): timeout on the stderr-drain loop returns `Err(...)`, which `execute_build` treats as `InfrastructureFailure`. Infrastructure failures trigger **reassignment** — the scheduler sends the same build to another worker, which also times out, which reassigns again. Timeout retry storm.

**Fix:**

1. **Call `build_cgroup.kill()` before drop on any error exit.** The method already exists (writes `1` to `cgroup.kill`, atomically SIGKILLs everything in the cgroup). It's just not called on the timeout path.

   ```rust
   // On any early return from execute_build:
   if let Some(cg) = build_cgroup.take() {
       let _ = cg.kill();  // best-effort; log but don't propagate
   }
   ```

   Better: wrap `build_cgroup` in a scopeguard that calls `kill()` on drop, and `disarm()` it only on the success path.

2. **Expand the status match.** One-to-one where rio has a matching variant:

   ```rust
   NixStatus::TimedOut => BuildStatus::TimedOut,
   NixStatus::LogLimitExceeded => BuildStatus::LogLimitExceeded,
   NixStatus::NotDeterministic => BuildStatus::NotDeterministic,
   // ... only truly-unknown statuses fall through to PermanentFailure
   ```

3. **Timeout returns `Ok(BuildResult::failure(TimedOut))`**, not `Err`. Timeout is a build outcome, not an infrastructure fault. No reassignment.

---

### 2.4 Proto plumbing dead/wrong

**Findings:** proto-heartbeat-resources-zero-producer, proto-heartbeat-response-generation-unread, proto-build-options-subfields-ignored

Three separate findings, same theme: proto fields that are wired up on one end but dead on the other.

**`proto-heartbeat-resources-zero-producer`:** The worker sends `ResourceUsage::default()` (all zeros) in every heartbeat. `rio-worker/src/heartbeat.rs` never samples anything. The scheduler's `ListWorkers` RPC faithfully reports the received values: 0.0 CPU, 0 bytes memory.

The phase4-plan-research memory said this field was "not plumbed" — that's **stale**. It IS plumbed (scheduler reads it, stores it, serves it on ListWorkers). The **producer** is wrong, not the plumbing.

**`proto-heartbeat-response-generation-unread`:** The scheduler sets `HeartbeatResponse.generation` specifically so workers can detect stale-leader assignments (compare assignment's generation against latest heartbeat response generation). The worker **never reads it**. The fence is one-sided.

**`proto-build-options-subfields-ignored`:** `BuildOptions` on the assignment has `build_timeout`, `max_silent_time`, `build_cores`, `keep_going`. The worker reads **only** `build_timeout`:

- `max_silent_time` — not passed to the daemon. Builds with long silent periods (large downloads, heavy linking) use the daemon's default.
- `build_cores` — not set as `NIX_BUILD_CORES`. Parallelism is whatever the daemon default is.
- `keep_going` — **correctness-bearing.** When set, a multi-output build should continue building other outputs after one fails. Currently ignored → behaves as `keep_going=false` always.

**Fix:**

1. Worker samples `/proc/stat` (CPU) and `/proc/meminfo` (memory) before each heartbeat. Delta-CPU since last heartbeat, current memory. ~20 lines.

2. Worker stores `resp.generation` from each heartbeat response. Before executing any `WorkAssignment`, compare `assignment.generation` against stored `latest_generation`. If `assignment.generation < latest_generation`, drop the assignment (log at INFO — this is expected during leader transitions).

3. Wire `max_silent_time` into the daemon args (`--max-silent-time N`). Set `NIX_BUILD_CORES=<build_cores>` in the build env. `keep_going` is more invasive — it changes the control flow in `execute_build` — and can be a separate PR.

---

### 2.5 tonic health: no set_not_serving on drain

**Findings:** tonic-no-set-not-serving-scheduler, tonic-no-set-not-serving-store, tonic-no-set-not-serving-gateway, tonic-actor-channelsend-wrong-code

`rio-scheduler/src/main.rs:427-429`, `rio-store/src/main.rs:349`, `rio-gateway/src/main.rs:254-258` — none of them call `health_reporter.set_not_serving()` on SIGTERM.

The shutdown sequence is: SIGTERM → `shutdown.cancel()` → `serve_with_shutdown` returns → process exits.

During the gap between "SIGTERM received" and "process exits," Kubernetes is still routing **new** connections to the pod (readiness probe still returns SERVING). Those new connections get accepted, then RST'd when the process exits. Clients see "connection reset" mid-stream.

**Plus `tonic-actor-channelsend-wrong-code`** (`grpc/mod.rs:138`): when the actor's mpsc channel is closed (actor task panicked or shut down), the gRPC handler maps `ChannelSend` error to `Status::internal`. Should be `Status::unavailable` — internal is non-retriable, unavailable is retriable. Clients that retry on unavailable (most do) would recover; with internal, they give up.

**Fix:**

```rust
// On shutdown.cancelled():
health_reporter.set_not_serving::<SchedulerServer<_>>().await;
// Wait for readiness probe to observe NOT_SERVING + propagate to Endpoint slice:
tokio::time::sleep(readiness_probe_period + Duration::from_secs(1)).await;
// THEN let serve_with_shutdown return
```

The sleep duration should be `readinessProbe.periodSeconds + 1` (so at least one probe observes NOT_SERVING) — read it from config, don't hardcode.

For `ChannelSend`: change `Status::internal` → `Status::unavailable` at `grpc/mod.rs:138`.

---

### 2.6 PG transaction safety

**Findings:** pg-in-mem-mutation-inside-tx, pg-querybuilder-param-limit, pg-partial-index-unused-parameterized

**`pg-in-mem-mutation-inside-tx`** (`rio-scheduler/src/actor/merge.rs:385-389`): inside a PG transaction, between `begin()` and `commit()`:

```rust
// merge.rs:385-389 (current)
let mut tx = pool.begin().await?;
// ... INSERT derivations RETURNING id ...
self.dag.node_mut(&hash).db_id = Some(*db_id);  // ← in-mem mutation INSIDE tx
// ... more INSERTs ...
tx.commit().await?;  // ← if this fails, in-mem has phantom db_ids
```

If `commit()` fails (serialization conflict, connection drop), the transaction rolls back — no rows inserted. But `self.dag` now has `db_id = Some(...)` pointing to a row that doesn't exist. Subsequent operations that query by that `db_id` get empty results.

**`pg-querybuilder-param-limit`** (`db.rs:630-661`): `QueryBuilder::push_values` for batch derivation insert. 9 parameters per row. PostgreSQL's bind-parameter limit is 65535. At 7282 rows (`65535/9 + 1`), the query fails with `"too many bind parameters"`. Large build graphs (NixOS system closure can be 30k+ derivations) will hit this.

**`pg-partial-index-unused-parameterized`** (`db.rs:23-25,579,859`): the migration defines a partial index:

```sql
CREATE INDEX ... WHERE status NOT IN ('Succeeded', 'Failed', 'Cancelled', 'Poisoned')
```

The query uses:

```sql
WHERE status <> ALL($1::text[])  -- $1 bound to TERMINAL_STATUSES const
```

The doc comment at `db.rs:23` claims "planner proves equivalence." **It does not.** PostgreSQL can't prove at plan time that `$1` will always equal the index's literal `NOT IN` list — `$1` is a bind parameter, resolved at execute time. `EXPLAIN` shows a seq scan. The index is never used.

**Fix:**

1. Move the `db_id` write **after** `tx.commit()` returns `Ok`. Collect the `(hash, db_id)` pairs during the transaction, apply them to `self.dag` only after commit succeeds.

2. Chunk the batch insert at ~5000 rows per query. Or switch to `UNNEST($1::text[], $2::text[], ...)` with array parameters (1 bind param per column, regardless of row count).

3. Inline `TERMINAL_STATUSES` as a SQL literal. Yes, this means duplicating the list in SQL and Rust — add a test that asserts they match (see `sched-terminal-statuses-no-enforce` in §3).

---

### 2.7 Test fidelity: MockScheduler lies

**Findings:** test-mock-watch-build-ignores-since-sequence, test-mock-sequence-starts-at-zero, test-start-paused-real-tcp-spawn-blocking, tonic-upload-start-paused-real-tcp

**`test-mock-watch-build-ignores-since-sequence`** (`rio-gateway/tests/common/grpc.rs:507-549`): `MockScheduler::watch_build` binds the request to `_request` and never reads it:

```rust
// grpc.rs:507 (current)
async fn watch_build(&self, _request: Request<WatchBuildRequest>) -> ... {
    //                      ^^^^^^^^ discarded
```

The gateway's reconnect logic passes `since_sequence` so the scheduler can resume the event stream from the right place. The mock ignores it. Tests that "verify reconnect works" are actually verifying "reconnect doesn't crash" — a test with `since_sequence=9999` on a 10-event stream would pass.

**`test-mock-sequence-starts-at-zero`** (`grpc.rs:430-432`): the mock auto-fills `sequence` from `enumerate()`, which starts at 0. The **real** scheduler starts at 1. Sequence 0 is the proto sentinel for "from start." Any test that asserts on sequence values is asserting against a mock that doesn't match prod.

**`test-start-paused-real-tcp-spawn-blocking`** (three tests in `rio-worker/src/upload.rs`): `#[tokio::test(start_paused = true)]` + real TCP (via `MockStore` on a real port) + `spawn_blocking` (for the NAR hasher).

`start_paused` auto-advances the virtual clock when all tasks are idle. Real TCP sockets and `spawn_blocking` tasks are idle from tokio's perspective **while doing real work**. So the virtual clock auto-advances while the socket is reading, and any `.timeout()` elsewhere in the test fires spuriously.

These tests currently pass because the real work is fast enough. On a loaded CI runner, they'll flake with "deadline exceeded" on an operation that **wasn't the one that timed out**.

**Fix:**

1. Record `since_sequence` in the mock's `watch_calls` log. Tests can then assert on it. Also: actually honor it (skip events with `sequence < since_sequence`).

2. Change the auto-fill from `i` to `i + 1`. Or better: make tests set `sequence` explicitly instead of relying on auto-fill.

3. Switch the mock to an in-memory transport (`tokio::io::duplex()`) instead of a real TCP port. This removes the real-IO dimension from `start_paused` tests.

---

### 2.8 Span propagation: component field lost

**Findings:** obs-spawn-monitored-no-span-propagation, obs-verify-required-fields-missing

`rio-common/src/task.rs:21` — `spawn_monitored` is the project-standard way to spawn a background task with panic reporting:

```rust
// task.rs:21 (current)
pub fn spawn_monitored<F>(name: &str, fut: F) -> JoinHandle<()> {
    tokio::spawn(async move {  // ← bare spawn, no span propagation
        // ...
    })
}
```

The root span (set up in each binary's `main()`) has `component = "scheduler"` (or gateway/store/worker/controller). This field is **spec-required** (`docs/src/observability.md`) — it's the primary filter key for log aggregation.

`tokio::spawn` doesn't propagate the current span across the task boundary. Every task spawned via `spawn_monitored` logs **without** the `component` field. That's the heartbeat loop, the GC sweeper, the lease renew task, the reconnect retry tasks — essentially all background work.

`tracey query rule obs.log.required-fields` shows no `r[verify ...]` annotation. The spec says "MUST include component"; nothing tests it.

**Fix:**

```rust
// task.rs:21 (fixed)
use tracing::Instrument;
pub fn spawn_monitored<F>(name: &str, fut: F) -> JoinHandle<()> {
    let span = tracing::Span::current();
    tokio::spawn(
        async move {
            // ...
        }
        .instrument(span)
    )
}
```

Add a Rust unit test: set up a root span with `component = "test"`, call `spawn_monitored`, have the spawned task `info!("hello")`, capture with `tracing_test::TestWriter`, assert the JSON has `"component":"test"`. Annotate `r[verify obs.log.required-fields]`.

---

### 2.9 Worker SIGINT missing

**Findings:** wkr-sigint-not-handled, wkr-hb-exit-bypasses-raii

`rio-worker/src/main.rs:386` — signal handling is SIGTERM only:

```rust
// main.rs:386 (current)
let mut sigterm = signal(SignalKind::terminate())?;
// ← no SIGINT
```

Every other binary uses `rio_common::signal::shutdown_signal()`, which handles SIGTERM **and** SIGINT. The worker rolled its own and forgot SIGINT.

CLAUDE.md coverage section says "worker (already had it)" re: graceful shutdown for profraw flush. **Stale.** The worker has graceful shutdown for SIGTERM but not SIGINT. Local dev `Ctrl+C` → no profraw flush → coverage data lost.

**Plus `wkr-hb-exit-bypasses-raii`** (`main.rs:431`): when the heartbeat task panics (e.g., scheduler connection permanently lost), the monitor does:

```rust
// main.rs:431 (current)
std::process::exit(1);  // ← bypasses all Drop impls
```

`std::process::exit()` doesn't run destructors. The FUSE mount's `Drop` doesn't run → mount leaks → next worker start on the same node fails with `EBUSY` on the mount point.

**Fix:**

1. Replace the hand-rolled signal handling with `rio_common::signal::shutdown_signal()`. Update CLAUDE.md (remove the stale "already had it").

2. Replace `std::process::exit(1)` with returning an `anyhow::Error` from `main()`. The `?` propagation already exists for other error paths; use it.

---

### 2.10 FUSE waiter timeout arithmetic

**Findings:** fuse-wait-timeout-vs-stream-timeout, fuse-blockon-thread-exhaustion, fuse-index-disk-divergence

**`fuse-wait-timeout-vs-stream-timeout`** (`rio-worker/src/fuse/fetch.rs:60-68`): two timeouts that don't agree:

```rust
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);      // fetch.rs:60
const GRPC_STREAM_TIMEOUT: Duration = Duration::from_secs(300);  // (fetcher)
```

When two build processes read the same cold store path simultaneously, one becomes the "fetcher" (opens the gRPC stream) and others become "waiters" (block on the inflight map).

A 1GB NAR at 15 MB/s takes ~70s. At T+30s, waiters hit `WAIT_TIMEOUT` and get `EAGAIN`. At T+70s, the fetcher **succeeds** — the file is there. But the waiters already told their builders "try again later," and most builders don't retry `EAGAIN` on `open()`.

**`fuse-blockon-thread-exhaustion`**: N distinct cold paths → N FUSE kernel threads blocked in `block_on(fetch)`. FUSE thread pool is bounded (default 4-8). If more than that many distinct cold paths are requested simultaneously, subsequent FUSE ops — even for **warm** paths — queue behind the blocked threads.

**`fuse-index-disk-divergence`**: `ensure_cached` trusts the SQLite index. If the index says "present" but the file was `rm`'d (manual cleanup, disk corruption, interrupted write), the FUSE op returns `ENOENT` — it doesn't re-fetch. The index is now permanently wrong for that path.

**Fix:**

1. Either raise `WAIT_TIMEOUT` ≥ `GRPC_STREAM_TIMEOUT`, OR (better) loop `wait()` while the inflight-map entry for this path still exists. The entry is removed on fetcher completion (success or failure), so "entry exists" = "fetcher still working, keep waiting."

2. Semaphore bounding FUSE-initiated fetches. Acquire before `block_on(fetch)`, release after. Permits = `fuse_threads - 1` so at least one thread stays available for warm-path ops.

3. Cheap-stat fast path in `ensure_cached`: if index says present, `stat()` the file. On `ENOENT`, delete the index row and fall through to the fetch path. One syscall per warm lookup, but it's cheap and closes the divergence.

---

### 2.11 make_text unsorted refs

**Findings:** nix-make-text-unsorted-refs

`rio-nix/src/store_path.rs:218-221` — `make_text_path` computes a store path for text-hashed content (`pkgs.writeText` and friends). The hash input includes the references list:

```rust
// store_path.rs:218-221 (current)
for r in references {  // ← iterates in caller-supplied order
    h.update(r.as_bytes());
    h.update(b":");
}
```

Nix uses `StorePathSet` — a `BTreeSet` — so references are always iterated in sorted order. rio iterates in whatever order the caller passed.

Caller is `opcodes_write.rs:279`, which passes refs in **wire order** (the order the client sent them). If the client's order differs from sorted order (it can — the protocol doesn't mandate sorted), rio computes a **different store path** than Nix would.

`narinfo.rs:281` already sorts before iterating for the narinfo fingerprint. Same pattern, one site got it right.

**Fix:**

```rust
let mut sorted: Vec<_> = references.iter().collect();
sorted.sort();
for r in sorted {
    h.update(r.as_bytes());
    h.update(b":");
}
```

Add a test with two different orderings of the same ref set; assert they produce the same store path.

---

### 2.12 NAR parser unbounded recursion

**Findings:** nix-nar-unbounded-recursion, nix-nixbase32-decode-silent-overflow

**`nix-nar-unbounded-recursion`** (`rio-nix/src/nar.rs:214,306`): `parse_entry` recurses for each directory, with no depth counter:

```rust
// nar.rs:214 (current)
fn parse_entry(r: &mut impl Read) -> Result<Entry> {
    // ...
    "directory" => {
        for child in entries {
            parse_entry(r)?;  // ← unbounded recursion
        }
    }
}
```

A malicious NAR with ~4KB of nested `directory` frames (each ~8 bytes of framing) gets ~500 levels deep → stack overflow → worker crash.

This is a fuzz-findable bug. `rio-nix/fuzz/fuzz_targets/nar_parse.rs` exists but the corpus has no deeply-nested seed.

**`nix-nixbase32-decode-silent-overflow`** (`store_path.rs:394-399`): nixbase32 decoding accumulates 5 bits per input char. When the output length isn't a multiple of 5 bits, the excess bits are silently discarded:

```rust
// store_path.rs:394-399 (paraphrased)
let byte_idx = bit_idx / 8;
if byte_idx < out.len() {
    out[byte_idx] |= (digit << shift) as u8;
}
// ← bits that would land at byte_idx >= out.len() are just dropped
```

Nix throws on nonzero padding (it's an invalid encoding). rio silently accepts it, potentially decoding two different inputs to the same output.

**Fix:**

1. Thread a `depth: u32` parameter through `parse_entry`. Increment on directory entry, error at 256 (Nix's `MAX_PATH` is ~4096 chars, which at 1 char per component is ~2048 levels — 256 is conservative but no legitimate NAR approaches it). Add a deep-nesting fuzz seed.

2. Check that discarded bits are zero. If the bit-index math produces bits past the output buffer AND those bits are nonzero, return `Err(InvalidPadding)`.

---

### 2.13 SSH hardening

**Findings:** ssh-session-errors-swallowed, ssh-unbounded-channels, ssh-no-keepalive, ssh-nagle-enabled, ssh-auth-methods-all

`rio-gateway/src/ssh/server.rs:136` — `handle_session_error` is not overridden. russh's default is a no-op. So when `?` propagates an error out of `channel_success` (or any other channel method), the connection is dropped **with zero logging**.

From the client side: `nix copy` hangs for 30s then reports "Broken pipe." No gateway log, no metric. Undiagnosable.

**Plus the config gaps:**

| Setting | Current | Should be |
|---|---|---|
| Channels per connection | unbounded | `Ok(false)` when `sessions.len() >= 4` |
| `keepalive_interval` | `None` | `Some(Duration::from_secs(30))` |
| `nodelay` | `false` (Nagle on) | `true` — latency-sensitive protocol |
| `methods` | `all()` (incl. password, none) | `[PublicKey]` only |

`nodelay=false` is particularly bad for the worker protocol, which does a lot of small request/response ping-pong. Nagle adds ~40ms per round-trip.

**Fix:**

```rust
// server.rs — override
async fn handle_session_error(&mut self, error: <Self as Handler>::Error) {
    error!(error = %error, "SSH session error");
    metrics::counter!("rio_gateway_ssh_session_errors_total").increment(1);
}
```

Config: `keepalive_interval: Some(Duration::from_secs(30))`, `nodelay: true`, `methods: MethodSet::from(&[Method::PublicKey])`. Channel limit: in `channel_open_session`, check `self.sessions.len()` and return `Ok(false)` past a configured limit (default 4 — matches Nix's default `max-jobs`).

---

### 2.14 Store GC robustness

**Findings:** store-mark-not-in-null-trap, store-trigger-gc-unbounded-extra-roots, store-nar-buffer-no-global-limit, pg-putpath-shared-lock-pool-exhaustion

**`store-mark-not-in-null-trap`** (`rio-store/src/gc/mark.rs:110`): the sweep candidate query:

```sql
SELECT store_path FROM nodes WHERE store_path NOT IN (SELECT ... FROM marks)
```

If **any** row in the marks subquery is `NULL`, SQL's three-valued logic makes `NOT IN (...NULL...)` evaluate to `UNKNOWN` (not `TRUE`) for every row. The outer `WHERE` filters them all out. Result: sweep returns 0 candidates. GC silently does nothing.

The marks table has `store_path TEXT NOT NULL`, so this is currently unreachable — but it's one migration mistake away from a silent GC-off.

**`store-trigger-gc-unbounded-extra-roots`** (`admin.rs:69-79`): the `TriggerGC` admin RPC accepts `extra_roots: Vec<String>`:

```rust
// admin.rs:69-79 (current)
let roots = req.extra_roots;  // ← no check_bound, no validation
// ... passed to mark phase, which acquires GC_MARK_LOCK_ID ...
```

No size bound, no store-path validation. An attacker (or misconfigured admin tool) can send 10M garbage strings. Mark phase is slow, and holds `GC_MARK_LOCK_ID` for the duration — which blocks **all** `PutPath` calls (they acquire the shared side of the same lock).

**`store-nar-buffer-no-global-limit`** (`put_path.rs:360-395`): `PutPath` streams NAR bytes into a buffer. Per-request limit is 4 GiB. No global semaphore. 10 concurrent 4 GiB uploads = 40 GiB RSS. Store OOMs.

**`pg-putpath-shared-lock-pool-exhaustion`** (`put_path.rs:298`): `PutPath` acquires a session-scoped advisory lock:

```sql
SELECT pg_advisory_lock_shared($1)  -- session-scoped, needs dedicated conn
```

Session-scoped locks are held until the connection releases them OR the connection ends. So `PutPath` checks out a dedicated connection from the pool for the **entire upload duration**. Pool size is 20. The 21st concurrent upload blocks on pool checkout → times out → fails.

**Fix:**

1. `NOT IN` → `NOT EXISTS`. NULL-safe, same semantics.

2. Add `check_bound(&req.extra_roots, MAX_EXTRA_ROOTS)` (say, 1000) + validate each as a store path before passing to mark.

3. Global weighted semaphore on in-flight NAR bytes. Acquire `nar_size` permits before buffering, release after write. Cap total at (configurable) fraction of `mem::available()`.

4. Use `pg_try_advisory_xact_lock_shared` inside the upload transaction instead. Transaction-scoped locks auto-release at commit/rollback, don't need a dedicated connection.

---

### 2.15 Gateway submit/reconnect gap

**Findings:** tonic-gateway-first-event-no-reconnect

`rio-gateway/src/handler/build.rs:207-214` — after `SubmitBuild` returns a stream, the gateway peeks the first event to extract `build_id`:

```rust
// build.rs:207-214 (current)
let mut stream = scheduler.submit_build(req).await?.into_inner();
let first = stream.message().await?
    .ok_or_else(|| anyhow!("empty build event stream"))?;  // ← no reconnect
let build_id = first.build_id;
```

The reconnect loop wraps the **subsequent** `stream.message()` calls, not this first one. If the scheduler SIGTERMs between accepting the stream and sending event 0 (a rollout during a build submit — not rare), the gateway errors out with `"empty build event stream"`.

But the build **was** submitted. The scheduler has it. The gateway just doesn't know the `build_id`, so it can't reconnect via `WatchBuild`.

The client sees a failure and retries `wopBuildDerivation`. Now there are **two** builds for the same derivation in the scheduler.

**Fix options:**

**Option A (minimal):** Move the first-event peek inside the reconnect loop. Harder than it sounds — the reconnect loop uses `WatchBuild(build_id)`, but we don't have `build_id` until the first event. Needs a two-phase retry: retry `SubmitBuild` itself until we get a first event.

**Option B (structural, preferred):** Have `SubmitBuild` return `build_id` in the **Response metadata** (trailing headers), not in-stream. Then the gateway has `build_id` immediately after `submit_build()` returns, before reading the stream. If the stream drops, reconnect via `WatchBuild(build_id)` — the existing reconnect logic handles this.

Option B requires a proto change but eliminates the race entirely.

---

## Section 3: MEDIUM remediations (P2)

One line each. Grouped by crate.

### rio-scheduler

| ID | Claim | Location | Fix |
|---|---|---|---|
| `sched-failed-workers-pg-unbounded` | `array_append(failed_workers, $1)` — no dedup, no bound. Flapping worker bloats row to MB. | `db.rs:443` | `WHERE NOT ($1 = ANY(failed_workers))` + cap at 100 (keep most-recent) |
| `sched-resolve-tenant-no-trim` | Tenant name lookup doesn't trim whitespace. 4th time patching this class. | `grpc/mod.rs:432` | `.trim()` — and add a `NormalizedName` newtype to kill the class |
| `sched-terminal-statuses-no-enforce` | `TERMINAL_STATUSES` const has no compile-time link to `DerivationStatus::is_terminal()`. Drift possible. | `db.rs:25` | `const_assert!` that every `is_terminal()` variant is in the array |
| `tonic-worker-reconnect-no-backoff` | Worker stream reconnect: fixed 1s, no backoff. Scheduler down 30s → 30 reconnect attempts. | `main.rs:522` | Exponential backoff capped at 10s |

### rio-worker

| ID | Claim | Location | Fix |
|---|---|---|---|
| `wkr-scan-unfiltered` | Output scan uploads **any** path under `$out`-prefix, uses basename as `output_name`. Stray tempfile → garbage output. | `upload.rs:73-94` | Filter: only upload paths whose names match `drv.outputs()` keys |
| `wkr-fod-flag-trust` | FOD-ness read from `assignment.is_fixed_output` (scheduler-supplied) not `drv.is_fixed_output()` (derived from drv itself). | `mod.rs:612` | Derive locally; ignore the assignment flag (or assert they agree) |
| `wkr-cancel-flag-stale` | `cancelled` flag set **before** kill syscall. If kill fails (ENOENT — process already gone), flag stays set → next assignment sees stale cancel. | `runtime.rs:193` | Set flag only after kill succeeds; reset on ENOENT |

### rio-store

| ID | Claim | Location | Fix |
|---|---|---|---|
| `store-append-sigs-unbounded-growth` | `sigs = array_cat(sigs, $1)` — no dedup, no bound. Each `AddSignatures` call appends, never replaces. | `queries.rs:232` | `array(SELECT DISTINCT unnest(array_cat(...)))` + cap at 32 |
| `store-pin-unvalidated-and-leaks-sqlx` | `Pin` RPC: path not validated, sqlx error message (`"no rows"`) leaked to client verbatim. | `admin.rs:320-400` | `validate_store_path` + map error to `Status::not_found("path not found")` |
| `store-client-hash-trusted` | **DOWNGRADED from HIGH.** `PutPath` trusts client-supplied `nar_hash` if non-empty. But: no producer sends non-empty. Defensive-only. | `put_path.rs:~180` | Ignore client hash; always compute server-side. Remove the branch. |
| `store-grace-i32-cast-truncation` | `grace_hours: u32` cast to `i32` for interval arithmetic. `>2147483647` → negative → sweep deletes **everything immediately**. | `mark.rs:113` | Clamp at `i32::MAX` before cast, or use `i64`. Config-validate at load time. |

### rio-controller

| ID | Claim | Location | Fix |
|---|---|---|---|
| `ctrl-cleanup-blocks-reconcile-worker` | Cleanup poll loop runs **inside** reconcile. Observed 7260s reconcile duration. Blocks the reconcile worker. | `mod.rs:413-448` | Spawn cleanup as a separate task; reconcile returns immediately |
| `ctrl-store-err-mislabeled` | Store RPC errors logged/metriced as `SchedulerUnavailable`. Wrong component in alerts. | `build.rs:201` | Distinguish; add `StoreUnavailable` variant |
| `ctrl-connect-store-outside-main` | Only `connect_*` call NOT in `main()` startup loop. Audit trigger per dev-workflow memory. | `build.rs:201` | Move to `Ctx` like the other clients; `ctx.store.clone()` at use site |

### rio-common

| ID | Claim | Location | Fix |
|---|---|---|---|
| `common-redact-password-at-sign` | URL password redaction uses `find('@')`. Password containing `@` → second half leaks. | `config.rs:160` | `rfind('@')` (last `@` is the userinfo delimiter per RFC 3986) |
| `common-otel-empty-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT=""` → exporter initialized with empty URL → every export fails silently. | `observability.rs:149` | Treat empty string as "not set" (same as None) |
| `common-otel-sample-rate-nan` | `OTEL_TRACES_SAMPLER_ARG=NaN` → `f64::clamp(NaN, 0.0, 1.0)` = `NaN` → sampler behavior undefined. | `observability.rs:155` | `if !rate.is_finite() { default }` before clamp |

### rio-gateway

| ID | Claim | Location | Fix |
|---|---|---|---|
| `gw-temp-roots-unbounded-insert-only` | `temp_roots` set: `insert` only, never `remove`, never read by GC. Memory leak + dead feature. | `opcodes_read.rs:180` | Either wire to GC roots OR delete the set (it's dead code) |
| `gw-submit-build-bare-question-mark-no-stderr` | `?` at `:203, :210, :213` propagates without `STDERR_ERROR`. Violates `r[gw.stderr.error-before-return]`. | `build.rs:203,210,213` | Wrap in `stderr_err!` macro like the other 40 sites in the file |

### config

| ID | Claim | Location | Fix |
|---|---|---|---|
| `cfg-worker-knobs-unreachable-in-k8s` | 6 worker config fields (incl. `daemon_timeout_secs`) not in `builders.rs` → not settable via `WorkerPoolSpec` CRD. Only reachable via env var in pod spec. | `rio-controller/src/builders.rs` | Add CRD fields; plumb through builders.rs; update CRD schema |
| `cfg-zero-interval-tokio-panic` | `tick_interval_secs=0` → `tokio::time::interval(ZERO)` → panic `"period must be non-zero"`. (Original claim cited `heartbeat_interval_secs` — that's a `const`, not configurable.) | `rio-scheduler/src/main.rs:481` | `serde validate: min=1` at config load |

### pg

| ID | Claim | Location | Fix |
|---|---|---|---|
| `pg-dual-migrate-race` | Both scheduler AND store call `sqlx::migrate!()` at startup. Race: both try to acquire migration lock. One waits, wastes startup time. | `scheduler/main.rs`, `store/main.rs` | Single migrator (store); scheduler checks `_sqlx_migrations` version only |
| `pg-zero-compile-time-checked-queries` | 194 `sqlx::query("...")` calls, 0 `sqlx::query!(...)` calls. Zero compile-time SQL checking. | workspace-wide | Opportunistic conversion; start with the 12 queries that have had runtime SQL errors in git history |
| `pg-drain-max-attempts-const-drift` | `MAX_DRAIN_ATTEMPTS = 10` at `drain.rs:22`. Migration `009` hardcodes `10` in a CHECK constraint. No link. | `rio-store/src/gc/drain.rs:22` vs migration 009 | Generate the CHECK constraint from the const (build.rs) OR test that asserts they match |

### wire

| ID | Claim | Location | Fix |
|---|---|---|---|
| `wire-query-realisation-error-after-stderr-last` | `?` after `STDERR_LAST` at `:543-584`. Same class as §2.1 but different site. | `opcodes_read.rs:543-584` | Same fix: don't `?` after `finish()` |
| `wire-synth-db-narhash-format-unverified` | `synth_db` writes `narHash` without format assertion. If upstream ever changes format, silent wrong-hash. | `synth_db.rs:68` | `debug_assert!` on format (hex, 64 chars) |

### observability

| ID | Claim | Location | Fix |
|---|---|---|---|
| `obs-fallback-reads-unregistered` | `rio_worker_fuse_fallback_reads_total` incremented but never `describe_counter!`. No `# HELP` line → Prometheus scrape shows untyped metric. **[FIXED by plan 14 — `describe_counter!` at `rio-worker/src/lib.rs:104`.]** | worker | Add `describe_counter!` in metric registration |
| `obs-watch-reconnects-unregistered` | Same for `rio_controller_watch_reconnects_total`. | controller | Same |
| `obs-recovery-histogram-success-only` | `rio_scheduler_recovery_duration_seconds` recorded at `:329` (success) but not `:379` (error return). Failed recovery attempts invisible in histogram. | `recovery.rs:329` vs `:379` | Record before return on both paths; add `outcome` label |

---

## Section 4: LOW remediations (P3)

List only. Fix opportunistically.

### Test coverage gaps

- `nix-hash-no-proptest` — `NixHash` parsers have no proptest roundtrips (unit tests only)
- `nix-proptest-escapes` — ATerm escape-sequence proptest doesn't cover all escape chars
- `wire-stderr-no-proptest` — STDERR frame writers have no proptest
- `wire-no-golden-query-substitutable-paths` — opcode 32: no golden conformance test
- `wire-no-golden-query-valid-derivers` — opcode 33: no golden conformance test
- `wire-no-golden-add-signatures` — opcode 41: no golden conformance test
- `fuse-no-enospc-test` — FUSE fetcher: no test for disk-full during NAR write
- `gc-no-concurrent-putpath-test` — GC sweep concurrent with PutPath: no test asserting lock ordering
- `store-sign-no-proptest` — signature roundtrip: no proptest
- `sched-dag-no-cycle-proptest` — DAG cycle detection: unit tests only, no arbitrary-graph proptest

### Doc / comment fixes

- `doc-recovery-comment-lies` — (covered in §1.1) `recovery.rs:169-174`
- `doc-partial-index-planner-claim` — (covered in §2.6) `db.rs:23` "planner proves equivalence"
- `doc-worker-sigint-claude-md` — (covered in §2.9) CLAUDE.md coverage section
- `doc-observability-metric-name-drift` — `observability.md` lists `rio_scheduler_dag_nodes` but code emits `rio_scheduler_dag_nodes_total`
- `doc-fuse-five-constraint-not-colocated` — the "5 constraints" doc for FUSE slow-path is in a separate file; should be a doc comment on the function
- `doc-grace-hours-default` — spec says default `grace_hours=6`, config default is `2`

### Minor robustness

- `kube-last-sequence-negative-cast` — `last_sequence: i64` cast from `u64` wraps at `2^63`. Cosmic-ray tier.
- `cli-status-partial-print` — `rio-cli status` prints header before checking connection; partial output on connect fail
- `test-pg-db-name-collision` — ephemeral PG test DBs use `{timestamp}_{pid}` — collision possible across parallel CI runners with clock skew. Add random suffix.
- `common-log-format-case-sensitive` — `RIO_LOG_FORMAT=JSON` rejected (expects `json`). `.to_lowercase()` before match.
- `ssh-auth-timing-side-channel` — auth failure paths have different timing (early return vs full check). Constant-time comparison not used. Exploitability: negligible (network jitter >> timing diff).
- `wkr-proc-stat-parse-panic` — `/proc/stat` parser `.unwrap()`s on field count. Would only fail on non-Linux, which we don't support, but still.
- `store-narinfo-bom-reject` — narinfo parser rejects UTF-8 BOM. Real narinfo files don't have BOM, but Windows-edited test fixtures might.
- `sched-leader-gauge-never-zero` — `rio_scheduler_is_leader` gauge: set to 1 on acquire, never set to 0 on loss. PromQL `sum()` across replicas shows 2 during transition.

### Code quality

- `fuse-five-constraint-doc-not-colocated` — (listed under docs too)
- `wire-magic-numbers-unnamed` — `0x6E6978` etc. scattered; should be named consts
- `sched-dag-node-mut-unwrap` — `.unwrap()` on `dag.node_mut()` in 11 places; should be `.expect("msg")` at minimum
- `common-metrics-macro-duplication` — `describe_counter!`/`describe_gauge!`/`describe_histogram!` blocks are copy-pasted per binary; extract to common
- `gw-handler-mod-1200-lines` — `opcodes_read.rs` is 1200+ lines; split by opcode group
- `store-queries-mod-900-lines` — `queries.rs` is 900+ lines; split
- `nix-wire-dead-read-bool` — `wire::read_bool` has a dead branch (never hit since 2024 refactor)

---

## Section 5: Downgraded / Refuted

### FOD modular hash: CRITICAL → LOW

**Original claim (two agents, independently):** `rio-nix/src/hash.rs:69` — `hash_modulo` computes the FOD (fixed-output derivation) hash for the `!out` suffix of `DerivedPath`. The implementation omits the output store path from the hash input:

```rust
// hash.rs:69 (current)
format!("fixed:out:{}:{}:", method, hash_hex)
//                               ^ trailing colon, NO output path
```

Nix C++ (`derivations.cc:hashDerivationModulo`) **does** include the output path:

```cpp
"fixed:out:" + method + ":" + hash.to_string(Base16, false) + ":" + outputPath
```

So rio and Nix compute different hashes for the same FOD. This was flagged CRITICAL — "wrong hash → wrong store path → everything breaks."

**Verification: severity is LOW, not CRITICAL.**

The bug is real. The hashes **do** differ. But the severity claim assumed this hash ends up on disk or on the wire somewhere that matters. It doesn't:

1. **Nix client discards the hash.** Traced the wire path: `worker-protocol.cc:268-276` (Nix C++ client) receives `DerivedPath` as a string `"/nix/store/...-foo.drv!out"`, splits on `!`, keeps **only** the output name (`"out"`). The hash that `hash_modulo` computes is an internal intermediate that never crosses the wire.

2. **Modern Nix daemon sends a dummy hash.** For CA derivations (which is what FODs are under the hood), the daemon sends a placeholder; the client recomputes. So even if rio sent the wrong hash, the client ignores it.

3. **Single caller, nothing persisted.** `hash_modulo` is called from exactly one place (`with_outputs_from_drv`), and the result goes into an in-memory `HashMap` key that's scope-local to one function. Not written to PG, not written to disk, not sent over gRPC.

**Likely origin:** `hash.rs:69` appears copy-pasted from `store_path.rs:189`, which implements `make_store_path_hash` — a **different** function where trailing-colon-no-path **is** correct (it's hashing the "type" prefix, not the full fingerprint). The copy brought the shape but not the semantics.

**Why fix anyway:** The docstring at `hash.rs:60` says "matches Nix's hashDerivationModulo" — it doesn't. Three unit tests assert against the **wrong** value (they test that rio agrees with rio, not with Nix). It's a landmine: anyone who later tries to use `hash_modulo` for something that **does** matter will get silently-wrong results.

**`nix-aterm-modulo-key-collision` is a symptom of this:** flagged separately as "HashMap keyed by hash_modulo result has potential collisions" — the collision space is exactly the set of FODs that differ only in output path, which `hash_modulo` conflates. Fixing `hash_modulo` fixes this automatically.

**Fix:** Append the output path to the hash input. Update the three tests with correct golden values (compute with `nix-store --query --hash` on a real FOD). Fix the docstring.

---

## Section 6: Stale memory corrections

The `phase4-plan-research.md` memory file (auto-memory, persists across sessions) flagged 5 bugs for phase4a. **3 of 5 are already fixed:**

| Memory claim | Current state | Action |
|---|---|---|
| `cancel_signals_sent_total` vs `cancel_signals_total` name mismatch across 4 sites | **FIXED** — all 4 sites use `rio_scheduler_cancel_signals_total` as of commit range `d874133..9937257` | Delete from memory |
| `BuildInfo.tenant_id: Option<String>` should be `Option<Uuid>` | **FIXED** — type is `Option<Uuid>`, serde accepts both string and UUID repr | Delete from memory |
| `HeartbeatRequest.resources` not plumbed scheduler→ListWorkers | **PARTIALLY STALE** — it IS plumbed. The bug is the **producer** sends `default()` (see §2.4 `proto-heartbeat-resources-zero-producer`). Memory had the wrong layer. | Update memory to point at producer |
| `BuildCR.spec.timeout` not honored | **STILL OPEN** — tracked as `cfg-worker-knobs-unreachable-in-k8s` in §3 | Keep |
| Scheduler recovery poisoned-orphan | **STILL OPEN** — this is §1.1 | Keep |

**Plus:** CLAUDE.md coverage section says:

> worker (already had it)

referring to graceful shutdown for profraw flush. **Stale** — worker has SIGTERM handling but not SIGINT (see §2.9). Ctrl+C during `nix develop -c cargo run --bin rio-worker` → no flush.

**Action items:**
- Delete the 2 fully-stale entries from `phase4-plan-research.md`
- Rewrite the heartbeat entry to point at the producer
- Update CLAUDE.md coverage section: "worker (SIGTERM only, see phase4a remediation §2.9)"

---

## Section 7: Positive audit results

For completeness — these patterns were audited across all sites and found **correct**. Listing them prevents re-auditing in future rounds.

| Pattern | Sites checked | Result |
|---|---|---|
| `"references"` identifier quoting in SQL (PG reserved word) | 5 queries across store/scheduler | All 5 correctly quoted |
| Transaction commit paths (`tx.commit().await?` reached on all success branches) | 14 transactions | All 14 have reachable commit; no silent rollback-on-drop |
| `Endpoint::connect_timeout` set on production channels | 6 Endpoint constructions | All 6 set `.connect_timeout(Duration::from_secs(10))` |
| `Ok(None)` handling on tonic stream reads (EOF vs error) | 10 `stream.message().await` sites | All 10 distinguish `Ok(None)` (EOF) from `Err` (transport error). (§1.3 finding is about what happens **after** correct detection, not the detection itself.) |
| kube-rs `Recorder` RBAC (events.k8s.io vs core `""`) | 3 Recorder uses | All 3 target `events.k8s.io` (correct API group) |
| Gauge inc/dec balance (every `.increment(1)` has a matching `.decrement(1)` in Drop/scopeguard) | 8 gauges | All 8 balanced; `rio_gateway_ssh_connections_active` etc. correctly scopeguarded |
| Histogram bucket boundaries match `observability.md` spec | 12 histograms | All 12 match spec (`[0.001, 0.01, 0.1, 1, 10, 60, 600]` for duration; `[1KB, ..., 1GB]` for size) |
| FUSE mount lifecycle (triple-redundant unmount: Drop + SIGTERM handler + atexit) | 1 mount | All three paths present and tested |

---

## Remediation index

Complete index of all findings by ID, for PR cross-referencing.

| ID | Severity | Theme | Primary file | Status |
|---|---|---|---|---|
| `sched-poisoned-orphan-spurious-succeeded` | P0 | §1.1 recovery | `rio-scheduler/src/actor/recovery.rs:120-146` | OPEN |
| `sched-id-to-hash-missing-poisoned` | P0 | §1.1 recovery | `rio-scheduler/src/actor/recovery.rs:120-146` | OPEN |
| `sched-recovery-comment-lies` | P0 | §1.1 recovery | `rio-scheduler/src/actor/recovery.rs:169-174` | OPEN |
| `sched-reap-collateral-poisoned` | P0 | §1.1 recovery | `rio-scheduler/src/dag/mod.rs:503-519` | OPEN |
| `sched-cancel-loop-crash-window` | P0 | §1.1 recovery | `rio-scheduler/src/actor/build.rs:111-146` | OPEN |
| `sched-transition-build-inmem-first` | P0 | §1.1 recovery | `rio-scheduler/src/actor/build.rs:414-443` | OPEN |
| `pg-poisoned-status-without-poisoned-at` | P0 | §1.1 recovery | `rio-scheduler/src/actor/completion.rs:445-447` | OPEN |
| `sched-check-build-completion-zero-zero` | P0 | §1.1 recovery | `rio-scheduler/src/actor/completion.rs` | OPEN |
| `sched-recovery-bd-rows-continue` | P0 | §1.1 recovery | `rio-scheduler/src/actor/recovery.rs:168` | OPEN |
| `sched-stderr-last-empty-built-outputs` | P0 | §1.1 recovery | (downstream symptom) | OPEN |
| `wkr-upload-refs-empty` | P0 | §1.2 refs | `rio-worker/src/upload.rs:224` | OPEN |
| `store-gc-mark-walks-empty` | P0 | §1.2 refs | `rio-store/src/gc/mark.rs:102` | OPEN |
| `store-sign-fingerprint-empty-refs` | P0 | §1.2 refs | `rio-store/src/grpc/mod.rs:204-224` | OPEN |
| `wkr-sandbox-bindmount-missing-transitive` | P0 | §1.2 refs | `docs/src/components/worker.md:215` | OPEN |
| `wkr-upload-deriver-empty` | P0 | §1.2 refs | `rio-worker/src/upload.rs:223` | OPEN |
| `ctrl-drain-stream-early-exit-no-patch` | P0 | §1.3 stuck-build | `rio-controller/src/reconcilers/build.rs:861-864` | OPEN |
| `ctrl-unknown-phase-infinite-cycle` | P0 | §1.3 stuck-build | `rio-controller/src/reconcilers/build.rs:419-428` | OPEN |
| `ctrl-reconnect-exhaust-wipes-status` | P0 | §1.3 stuck-build | `rio-controller/src/reconcilers/build.rs:419-428` | OPEN |
| `kube-status-ssa-wipes-cached-count` | P0 | §1.3 stuck-build | `rio-controller/src/crds/build.rs` | OPEN |
| `tonic-controller-watchbuild-retry-burns-on-dead-stream` | P0 | §1.3 stuck-build | `rio-controller/src/reconcilers/build.rs:882-898` | OPEN |
| `ctrl-no-periodic-requeue` | P0 | §1.3 stuck-build | `rio-controller/src/reconcilers/build.rs` | OPEN |
| `ctrl-no-owns-watch` | P0 | §1.3 stuck-build | `rio-controller/src/reconcilers/build.rs` | OPEN |
| `tonic-gateway-eager-connect-no-retry` | P0 | §1.4 eager-connect | `rio-gateway/src/main.rs:202,217,230` | OPEN |
| `tonic-worker-eager-connect-no-retry` | P0 | §1.4 eager-connect | `rio-worker/src/main.rs:157,161,170` | OPEN |
| `ci-codecov-after-n-builds-missing` | P0 | §1.5 coverage | `codecov.yml` | OPEN |
| `wire-realisation-outpath-full-path` | P1 | §1.6 realisation | `rio-gateway/src/handler/opcodes_read.rs:558` | OPEN |
| `wire-realisation-register-basename-rejected` | P1 | §1.6 realisation | `rio-gateway/src/handler/opcodes_read.rs:486` | OPEN |
| `test-mock-store-no-validate-store-path` | P1 | §1.6 realisation | `rio-gateway/tests/common/grpc.rs:304-316` | OPEN |
| `test-vm-protocol-wrong-opcode` | P1 | §1.6 realisation | `nix/tests/vm/phase2b/protocol.nix` | OPEN |
| `gw-stderr-error-then-last-desync` | P1 | §2.1 stderr-desync | `rio-gateway/src/handler/build.rs:508-523,792-800` | OPEN |
| `gw-stderr-build-derivation` | P1 | §2.1 stderr-desync | `rio-gateway/src/handler/build.rs:508-523` | OPEN |
| `gw-stderr-build-paths-with-results` | P1 | §2.1 stderr-desync | `rio-gateway/src/handler/build.rs:792-800` | OPEN |
| `kube-lease-renew-no-timeout` | P1 | §2.2 lease | `rio-scheduler/src/lease/election.rs:189,316` | OPEN |
| `kube-lease-no-local-self-fence` | P1 | §2.2 lease | `rio-scheduler/src/lease/mod.rs:385-398` | OPEN |
| `sched-recovery-complete-toctou` | P1 | §2.2 lease | `rio-scheduler/src/actor/recovery.rs` | OPEN |
| `wkr-timeout-cgroup-orphan` | P1 | §2.3 cgroup | `rio-worker/src/executor/mod.rs:575-589` | OPEN |
| `wkr-status-collapse` | P1 | §2.3 cgroup | `rio-worker/src/executor/mod.rs:736-744` | OPEN |
| `wkr-timeout-is-infra` | P1 | §2.3 cgroup | `rio-worker/src/executor/stderr_loop.rs:95` | OPEN |
| `proto-heartbeat-resources-zero-producer` | P1 | §2.4 proto | `rio-worker/src/heartbeat.rs` | OPEN |
| `proto-heartbeat-response-generation-unread` | P1 | §2.4 proto | `rio-worker/src/heartbeat.rs` | OPEN |
| `proto-build-options-subfields-ignored` | P1 | §2.4 proto | `rio-worker/src/executor/mod.rs` | OPEN |
| `tonic-no-set-not-serving-scheduler` | P1 | §2.5 drain | `rio-scheduler/src/main.rs:427-429` | OPEN |
| `tonic-no-set-not-serving-store` | P1 | §2.5 drain | `rio-store/src/main.rs:349` | OPEN |
| `tonic-no-set-not-serving-gateway` | P1 | §2.5 drain | `rio-gateway/src/main.rs:254-258` | OPEN |
| `tonic-actor-channelsend-wrong-code` | P1 | §2.5 drain | `rio-scheduler/src/grpc/mod.rs:138` | OPEN |
| `pg-in-mem-mutation-inside-tx` | P1 | §2.6 pg-tx | `rio-scheduler/src/actor/merge.rs:385-389` | OPEN |
| `pg-querybuilder-param-limit` | P1 | §2.6 pg-tx | `rio-scheduler/src/db.rs:630-661` | OPEN |
| `pg-partial-index-unused-parameterized` | P1 | §2.6 pg-tx | `rio-scheduler/src/db.rs:23-25,579,859` | OPEN |
| `test-mock-watch-build-ignores-since-sequence` | P1 | §2.7 mock-lies | `rio-gateway/tests/common/grpc.rs:507-549` | OPEN |
| `test-mock-sequence-starts-at-zero` | P1 | §2.7 mock-lies | `rio-gateway/tests/common/grpc.rs:430-432` | OPEN |
| `test-start-paused-real-tcp-spawn-blocking` | P1 | §2.7 mock-lies | `rio-worker/src/upload.rs` tests | OPEN |
| `tonic-upload-start-paused-real-tcp` | P1 | §2.7 mock-lies | `rio-worker/src/upload.rs` tests | OPEN |
| `obs-spawn-monitored-no-span-propagation` | P1 | §2.8 span | `rio-common/src/task.rs:21` | OPEN |
| `obs-verify-required-fields-missing` | P1 | §2.8 span | (tracey rule gap) | OPEN |
| `wkr-sigint-not-handled` | P1 | §2.9 sigint | `rio-worker/src/main.rs:386` | OPEN |
| `wkr-hb-exit-bypasses-raii` | P1 | §2.9 sigint | `rio-worker/src/main.rs:431` | OPEN |
| `fuse-wait-timeout-vs-stream-timeout` | P1 | §2.10 fuse | `rio-worker/src/fuse/fetch.rs:60-68` | OPEN |
| `fuse-blockon-thread-exhaustion` | P1 | §2.10 fuse | `rio-worker/src/fuse/fetch.rs` | OPEN |
| `fuse-index-disk-divergence` | P1 | §2.10 fuse | `rio-worker/src/fuse/index.rs` | OPEN |
| `nix-make-text-unsorted-refs` | P1 | §2.11 make-text | `rio-nix/src/store_path.rs:218-221` | OPEN |
| `nix-nar-unbounded-recursion` | P1 | §2.12 nar | `rio-nix/src/nar.rs:214,306` | OPEN |
| `nix-nixbase32-decode-silent-overflow` | P1 | §2.12 nar | `rio-nix/src/store_path.rs:394-399` | OPEN |
| `ssh-session-errors-swallowed` | P1 | §2.13 ssh | `rio-gateway/src/ssh/server.rs:136` | OPEN |
| `ssh-unbounded-channels` | P1 | §2.13 ssh | `rio-gateway/src/ssh/server.rs` | OPEN |
| `ssh-no-keepalive` | P1 | §2.13 ssh | `rio-gateway/src/ssh/server.rs` | OPEN |
| `ssh-nagle-enabled` | P1 | §2.13 ssh | `rio-gateway/src/ssh/server.rs` | OPEN |
| `ssh-auth-methods-all` | P1 | §2.13 ssh | `rio-gateway/src/ssh/server.rs` | OPEN |
| `store-mark-not-in-null-trap` | P1 | §2.14 gc | `rio-store/src/gc/mark.rs:110` | OPEN |
| `store-trigger-gc-unbounded-extra-roots` | P1 | §2.14 gc | `rio-store/src/grpc/admin.rs:69-79` | OPEN |
| `store-nar-buffer-no-global-limit` | P1 | §2.14 gc | `rio-store/src/grpc/put_path.rs:360-395` | OPEN |
| `pg-putpath-shared-lock-pool-exhaustion` | P1 | §2.14 gc | `rio-store/src/grpc/put_path.rs:298` | OPEN |
| `tonic-gateway-first-event-no-reconnect` | P1 | §2.15 submit-gap | `rio-gateway/src/handler/build.rs:207-214` | OPEN |
| `sched-failed-workers-pg-unbounded` | P2 | §3 scheduler | `rio-scheduler/src/db.rs:443` | OPEN |
| `sched-resolve-tenant-no-trim` | P2 | §3 scheduler | `rio-scheduler/src/grpc/mod.rs:432` | OPEN |
| `sched-terminal-statuses-no-enforce` | P2 | §3 scheduler | `rio-scheduler/src/db.rs:25` | OPEN |
| `tonic-worker-reconnect-no-backoff` | P2 | §3 scheduler | `rio-worker/src/main.rs:522` | OPEN |
| `wkr-scan-unfiltered` | P2 | §3 worker | `rio-worker/src/upload.rs:73-94` | OPEN |
| `wkr-fod-flag-trust` | P2 | §3 worker | `rio-worker/src/executor/mod.rs:612` | OPEN |
| `wkr-cancel-flag-stale` | P2 | §3 worker | `rio-worker/src/runtime.rs:193` | OPEN |
| `store-append-sigs-unbounded-growth` | P2 | §3 store | `rio-store/src/queries.rs:232` | OPEN |
| `store-pin-unvalidated-and-leaks-sqlx` | P2 | §3 store | `rio-store/src/grpc/admin.rs:320-400` | OPEN |
| `store-client-hash-trusted` | P2 | §3 store | `rio-store/src/grpc/put_path.rs` | OPEN |
| `store-grace-i32-cast-truncation` | P2 | §3 store | `rio-store/src/gc/mark.rs:113` | OPEN |
| `ctrl-cleanup-blocks-reconcile-worker` | P2 | §3 controller | `rio-controller/src/reconcilers/mod.rs:413-448` | OPEN |
| `ctrl-store-err-mislabeled` | P2 | §3 controller | `rio-controller/src/reconcilers/build.rs:201` | OPEN |
| `ctrl-connect-store-outside-main` | P2 | §3 controller | `rio-controller/src/reconcilers/build.rs:201` | OPEN |
| `common-redact-password-at-sign` | P2 | §3 common | `rio-common/src/config.rs:160` | OPEN |
| `common-otel-empty-endpoint` | P2 | §3 common | `rio-common/src/observability.rs:149` | OPEN |
| `common-otel-sample-rate-nan` | P2 | §3 common | `rio-common/src/observability.rs:155` | OPEN |
| `gw-temp-roots-unbounded-insert-only` | P2 | §3 gateway | `rio-gateway/src/handler/opcodes_read.rs:180` | OPEN |
| `gw-submit-build-bare-question-mark-no-stderr` | P2 | §3 gateway | `rio-gateway/src/handler/build.rs:203,210,213` | OPEN |
| `cfg-worker-knobs-unreachable-in-k8s` | P2 | §3 config | `rio-controller/src/builders.rs` | OPEN |
| `cfg-zero-interval-tokio-panic` | P2 | §3 config | `rio-scheduler/src/main.rs:481` | OPEN |
| `pg-dual-migrate-race` | P2 | §3 pg | `rio-scheduler/src/main.rs`, `rio-store/src/main.rs` | OPEN |
| `pg-zero-compile-time-checked-queries` | P2 | §3 pg | workspace-wide | OPEN |
| `pg-drain-max-attempts-const-drift` | P2 | §3 pg | `rio-store/src/gc/drain.rs:22` | OPEN |
| `wire-query-realisation-error-after-stderr-last` | P2 | §3 wire | `rio-gateway/src/handler/opcodes_read.rs:543-584` | OPEN |
| `wire-synth-db-narhash-format-unverified` | P2 | §3 wire | `rio-worker/src/synth_db.rs:68` | OPEN |
| `obs-fallback-reads-unregistered` | P2 | §3 obs | `rio-worker` | FIXED (plan 14) |
| `obs-watch-reconnects-unregistered` | P2 | §3 obs | `rio-controller` | OPEN |
| `obs-recovery-histogram-success-only` | P2 | §3 obs | `rio-scheduler/src/actor/recovery.rs:329,379` | OPEN |
| `nix-hash-no-proptest` | P3 | §4 test-cov | `rio-nix/src/hash.rs` | OPEN |
| `nix-proptest-escapes` | P3 | §4 test-cov | `rio-nix/src/aterm.rs` | OPEN |
| `wire-stderr-no-proptest` | P3 | §4 test-cov | `rio-gateway/src/handler/stderr.rs` | OPEN |
| `wire-no-golden-query-substitutable-paths` | P3 | §4 test-cov | `rio-gateway/tests/golden/` | OPEN |
| `wire-no-golden-query-valid-derivers` | P3 | §4 test-cov | `rio-gateway/tests/golden/` | OPEN |
| `wire-no-golden-add-signatures` | P3 | §4 test-cov | `rio-gateway/tests/golden/` | OPEN |
| `fuse-no-enospc-test` | P3 | §4 test-cov | `rio-worker/src/fuse/` | OPEN |
| `gc-no-concurrent-putpath-test` | P3 | §4 test-cov | `rio-store/src/gc/` | OPEN |
| `store-sign-no-proptest` | P3 | §4 test-cov | `rio-store/src/sign.rs` | OPEN |
| `sched-dag-no-cycle-proptest` | P3 | §4 test-cov | `rio-scheduler/src/dag/` | OPEN |
| `doc-observability-metric-name-drift` | P3 | §4 doc | `docs/src/observability.md` | OPEN |
| `doc-fuse-five-constraint-not-colocated` | P3 | §4 doc | `rio-worker/src/fuse/` | OPEN |
| `doc-grace-hours-default` | P3 | §4 doc | `docs/src/components/store.md` | OPEN |
| `kube-last-sequence-negative-cast` | P3 | §4 robust | `rio-controller/src/reconcilers/build.rs` | OPEN |
| `cli-status-partial-print` | P3 | §4 robust | `rio-cli/src/status.rs` | OPEN |
| `test-pg-db-name-collision` | P3 | §4 robust | `rio-test-support/src/pg.rs` | OPEN |
| `common-log-format-case-sensitive` | P3 | §4 robust | `rio-common/src/observability.rs` | OPEN |
| `ssh-auth-timing-side-channel` | P3 | §4 robust | `rio-gateway/src/ssh/auth.rs` | OPEN |
| `wkr-proc-stat-parse-panic` | P3 | §4 robust | `rio-worker/src/heartbeat.rs` | OPEN |
| `store-narinfo-bom-reject` | P3 | §4 robust | `rio-store/src/narinfo.rs` | OPEN |
| `sched-leader-gauge-never-zero` | P3 | §4 robust | `rio-scheduler/src/lease/mod.rs` | OPEN |
| `wire-magic-numbers-unnamed` | P3 | §4 code-quality | `rio-nix/src/protocol/` | OPEN |
| `sched-dag-node-mut-unwrap` | P3 | §4 code-quality | `rio-scheduler/src/dag/mod.rs` | OPEN |
| `common-metrics-macro-duplication` | P3 | §4 code-quality | all `main.rs` | OPEN |
| `gw-handler-mod-1200-lines` | P3 | §4 code-quality | `rio-gateway/src/handler/opcodes_read.rs` | OPEN |
| `store-queries-mod-900-lines` | P3 | §4 code-quality | `rio-store/src/queries.rs` | OPEN |
| `nix-wire-dead-read-bool` | P3 | §4 code-quality | `rio-nix/src/protocol/wire.rs` | OPEN |
| `nix-fod-hash-modulo-omits-outpath` | LOW (was P0) | §5 downgraded | `rio-nix/src/hash.rs:69` | OPEN |
| `nix-aterm-modulo-key-collision` | LOW (symptom of above) | §5 downgraded | `rio-nix/src/aterm.rs` | OPEN |

**Totals:** 25 P0, 43 P1, 28 P2, 28 P3, 2 downgraded-LOW = **126 indexed findings** (deduplicated from 180+ raw findings; many raw findings were different entry points into the same root cause).
