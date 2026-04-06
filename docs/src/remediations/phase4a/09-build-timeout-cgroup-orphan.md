# 09 — Build timeout orphans cgroup, collapses status, triggers reassignment storm

**Parent:** [`phase4a.md` §2.3](../phase4a.md#23-build-timeout-orphans-cgroup--status-collapse)
**Findings:** `wkr-timeout-cgroup-orphan` (P1), `wkr-status-collapse` (P1), `wkr-timeout-is-infra` (P1), `wkr-cancel-flag-stale` (P2)

---

## Summary

Four distinct bugs with one precipitating event (build timeout):

| Finding | Effect | Root |
|---|---|---|
| `wkr-timeout-cgroup-orphan` | Builder process survives timeout → cgroup leaks `EBUSY` → next build of same drv on same worker fails `EEXIST` | `daemon.kill()` kills only the direct child; builder is a grandchild forked by nix-daemon |
| `wkr-timeout-is-infra` | Timeout → `Err(ExecutorError)` → runtime reports `InfrastructureFailure` → scheduler reassigns → new worker also times out → storm | `stderr_loop.rs:95` maps `Elapsed` to `Err` instead of `Ok(failure)` |
| `wkr-status-collapse` | Even if timeout reached the status match, `TimedOut` → `_` arm → `PermanentFailure` (wrong metric label, wrong triage signal) | `mod.rs:743` catch-all collapses 7 distinct Nix statuses |
| `wkr-cancel-flag-stale` | Cancel arrives before cgroup exists → `ENOENT` on kill, flag stays `true` → unrelated later `Err` misclassified as `Cancelled` | `runtime.rs:193` sets flag unconditionally before kill; never cleared on failed kill |

**Causality chain these fixes break:** timeout → `Err` path (fix 3) → `InfrastructureFailure` (fix 2 would have saved this even without fix 3) → reassignment storm. Separately: timeout → daemon killed, builder orphaned (fix 1) → `EBUSY` leak → `EEXIST` on next assignment.

---

## 1. `wkr-timeout-cgroup-orphan` — kill the cgroup tree, not just the daemon

### Current flow (`rio-worker/src/executor/mod.rs:552-589`)

```rust
let build_result = run_daemon_build(...).await;   // ← timeout returns Err here
// ...
if let Err(e) = daemon.kill().await { ... }       // ← kills nix-daemon ONLY (direct child)
match tokio::time::timeout(2s, daemon.wait()) { ... }  // ← reaps nix-daemon, NOT the builder
drop(build_cgroup);                               // ← Drop::rmdir → EBUSY (builder still alive)
let build_result = build_result?;                 // ← NOW propagates Err
```

The `?`-exit analysis between cgroup creation (line 471) and cleanup (line 589):

| Line | `?` source | Cgroup state at exit | Leak? |
|---|---|---|---|
| 472 | `BuildCgroup::create` fails | never created | no |
| 475 | `daemon.id()` is `None` (died at spawn) | created, **empty** (add_process not reached) | no — Drop rmdir succeeds |
| 478 | `add_process` fails | created, **empty** (write to cgroup.procs failed) | no — Drop rmdir succeeds |
| 595 | `build_result?` | **already past line 589** — cleanup ran | only if cleanup itself leaked |

So the **only** orphan path runs through lines 575–589 with a live builder. No scopeguard is strictly necessary to close the bug — an inline `kill()` suffices. A guard is still recommended as defense against future `?` insertions (see hardening below).

### Fix: inline `build_cgroup.kill()` + bounded drain before `drop`

`BuildCgroup::kill()` (cgroup.rs:180) already exists — writes `1` to `cgroup.kill`, kernel walks the tree and SIGKILLs everything. It's used by cancel (`try_cancel_build`) but never by the timeout/completion path.

`cgroup.kill` is asynchronous from userspace's perspective: the write returns immediately, but `cgroup.procs` doesn't empty until the kernel has delivered SIGKILL and the processes have actually exited. The existing `cancel-cgroup-kill` VM test observes cgroup-gone in <1.5s, but rmdir in the same tick would still race. We need a bounded poll.

**Diff** (`rio-worker/src/executor/mod.rs`, after line 584, before `drop(build_cgroup)` at line 589):

```rust
    match tokio::time::timeout(Duration::from_secs(2), daemon.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "daemon.wait() failed after kill"),
        Err(_) => tracing::warn!("daemon did not exit within 2s after kill (possible zombie)"),
    }

    // daemon.kill() above SIGKILLs the nix-daemon process only. The
    // builder is a GRANDCHILD (forked by the daemon during wopBuildDerivation)
    // and is not in the daemon's process group — it lives on in the
    // cgroup. On the success path the builder has already exited (build
    // finished → daemon sent STDERR_LAST → we got here); on the timeout/
    // error path it's still running a `sleep 3600` or a stuck compiler.
    //
    // cgroup.kill walks the tree: SIGKILLs everything, including sub-
    // cgroups the daemon may have created. Idempotent — writing "1" to
    // an empty cgroup is a no-op — so we call it unconditionally rather
    // than branching on build_result.is_err().
    //
    // r[impl worker.cgroup.kill-on-teardown]
    if let Err(e) = build_cgroup.kill() {
        // ENOENT shouldn't happen (we hold the BuildCgroup, Drop hasn't
        // run); EACCES would mean delegation is broken. Log and fall
        // through — rmdir will fail EBUSY and warn again, but we don't
        // want to upgrade a successful build to an error here.
        tracing::warn!(error = %e, "build_cgroup.kill() failed");
    }
    // cgroup.kill is async: write returns before procs are gone. Poll
    // cgroup.procs until empty or 2s elapsed (same budget as daemon.wait
    // above; SIGKILL → exit is ~ms, 2s is vast headroom for a zombie-
    // reparented tree). Sync read on blocking pool — 200 iterations of
    // a single-line procfs read, negligible.
    let cgroup_path_for_poll = build_cgroup.path().to_path_buf();
    let drained = tokio::task::spawn_blocking(move || {
        for _ in 0..200 {
            match std::fs::read_to_string(cgroup_path_for_poll.join("cgroup.procs")) {
                Ok(s) if s.trim().is_empty() => return true,
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                // ENOENT: cgroup already gone (shouldn't happen — we
                // hold the BuildCgroup — but treat as drained).
                Err(_) => return true,
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if !drained {
        tracing::warn!(
            cgroup = %build_cgroup.path().display(),
            "cgroup still has processes 2s after cgroup.kill; rmdir will EBUSY"
        );
    }

    // build_cgroup drops here. rmdir succeeds if the drain above emptied
    // it; otherwise Drop warns EBUSY + leaks (cleared on pod restart).
    drop(build_cgroup);
```

### Hardening: scopeguard for future `?` insertions

Not required to close the current bug (see exit-path table above), but cheap insurance. Pattern matches the existing `_build_guard` at line 289.

Insert immediately after line 478 (`add_process` succeeds — the cgroup now has a process):

```rust
    build_cgroup
        .add_process(daemon_pid)
        .map_err(|e| ExecutorError::Cgroup(format!("add daemon to cgroup: {e}")))?;

    // Kill-guard: any `?` between here and the explicit drop at the
    // bottom of this function fires this. The explicit kill() + drain
    // + drop below remain the PRIMARY path (they wait for drain; this
    // guard doesn't). scopeguard::guard not defer! — we need to hand
    // it the PathBuf, not borrow build_cgroup.
    let cgroup_kill_path = build_cgroup.path().to_path_buf();
    let cgroup_kill_guard = scopeguard::guard(cgroup_kill_path, |p| {
        // Best-effort. No drain — we're in Drop, can't await. The
        // BuildCgroup's own Drop runs right after this and will EBUSY
        // if the SIGKILL hasn't landed yet; that's the existing leak
        // path, just now with the kill attempted.
        let _ = std::fs::write(p.join("cgroup.kill"), "1");
    });
```

Then **defuse** it just before the explicit `drop(build_cgroup)`:

```rust
    // Defuse: explicit kill+drain above already ran; guard is redundant.
    scopeguard::ScopeGuard::into_inner(cgroup_kill_guard);
    drop(build_cgroup);
```

---

## 2. `wkr-status-collapse` — expand the Nix→proto status match

### Current state

**Nix side** (`rio-nix/src/protocol/build.rs:36-52`) — 15 variants:

```rust
Built, Substituted, AlreadyValid,          // success (is_success() → true)
PermanentFailure, InputRejected, OutputRejected, TransientFailure,
CachedFailure, TimedOut, MiscFailure, DependencyFailed,
LogLimitExceeded, NotDeterministic, ResolvesToAlreadyValid, NoSubstituters
```

**Proto side** (`rio-proto/proto/types.proto:257-270`) — 12 variants (incl. `UNSPECIFIED`):

```proto
UNSPECIFIED, BUILT, SUBSTITUTED, ALREADY_VALID,
PERMANENT_FAILURE, TRANSIENT_FAILURE, CACHED_FAILURE, DEPENDENCY_FAILED,
LOG_LIMIT_EXCEEDED, OUTPUT_REJECTED, INFRASTRUCTURE_FAILURE, CANCELLED
```

**Missing from proto:** `TimedOut`, `NotDeterministic`, `InputRejected`.

**Current match** (`rio-worker/src/executor/mod.rs:736-744`) — reachable only when `!is_success()`:

```rust
let status = match build_result.status {
    rio_nix::protocol::build::BuildStatus::PermanentFailure => BuildResultStatus::PermanentFailure,
    rio_nix::protocol::build::BuildStatus::TransientFailure => BuildResultStatus::TransientFailure,
    _ => BuildResultStatus::PermanentFailure,   // ← catches 9 variants
};
```

The `_` arm catches: `InputRejected`, `OutputRejected`, `CachedFailure`, `TimedOut`, `MiscFailure`, `DependencyFailed`, `LogLimitExceeded`, `NotDeterministic`, `NoSubstituters`. Five of these have proto equivalents that are simply not mapped.

### Fix part A: add proto variants

**Diff** (`rio-proto/proto/types.proto`, after line 269):

```proto
enum BuildResultStatus {
  BUILD_RESULT_STATUS_UNSPECIFIED = 0;
  BUILD_RESULT_STATUS_BUILT = 1;
  BUILD_RESULT_STATUS_SUBSTITUTED = 2;
  BUILD_RESULT_STATUS_ALREADY_VALID = 3;
  BUILD_RESULT_STATUS_PERMANENT_FAILURE = 4;
  BUILD_RESULT_STATUS_TRANSIENT_FAILURE = 5;
  BUILD_RESULT_STATUS_CACHED_FAILURE = 6;
  BUILD_RESULT_STATUS_DEPENDENCY_FAILED = 7;
  BUILD_RESULT_STATUS_LOG_LIMIT_EXCEEDED = 8;
  BUILD_RESULT_STATUS_OUTPUT_REJECTED = 9;
  BUILD_RESULT_STATUS_INFRASTRUCTURE_FAILURE = 10;
  BUILD_RESULT_STATUS_CANCELLED = 11;
  // Build exceeded configured timeout. Operationally: "raise the limit
  // or fix the build," NOT "retry on another worker" — the same inputs
  // will time out again. Scheduler treats as permanent-no-reassign.
  BUILD_RESULT_STATUS_TIMED_OUT = 12;
  // nix-daemon's determinism check failed (--check or repeat builds).
  // Operationally distinct from PermanentFailure: the build "works"
  // but isn't reproducible. Triage = find the nondeterminism source.
  BUILD_RESULT_STATUS_NOT_DETERMINISTIC = 13;
  // Derivation inputs rejected by nix-daemon (corrupt .drv, missing
  // reference, unsupported builder). NOT the same as OutputRejected
  // (which is post-build hash mismatch).
  BUILD_RESULT_STATUS_INPUT_REJECTED = 14;
}
```

Field numbers 12–14 are unused (checked: highest is 11). Proto3 enum additions are wire-backward-compatible (unknown values decode as the zero variant on old receivers, but we deploy scheduler and worker together via the same Helm chart so skew is bounded to one rollout).

### Fix part B: extract + expand the match

Pull the conversion into a free function so it's unit-testable without spinning up an executor. Replace `mod.rs:736-744`:

```rust
// rio-worker/src/executor/mod.rs — new free function near the bottom,
// after sanitize_build_id
/// Map a Nix daemon BuildStatus (failure path only — caller has already
/// branched on is_success()) to the proto BuildResultStatus reported to
/// the scheduler.
///
/// Exhaustive: no `_` arm. Adding a new BuildStatus variant in rio-nix
/// is a compile error here until the mapping decision is made.
///
/// r[impl worker.status.nix-to-proto]
pub(crate) fn nix_failure_to_proto(
    nix: rio_nix::protocol::build::BuildStatus,
) -> BuildResultStatus {
    use rio_nix::protocol::build::BuildStatus as Nix;
    match nix {
        // Success variants: caller branched on is_success(), these are
        // unreachable. Return Built anyway (not a panic — if the caller
        // contract is ever violated, a wrong-but-success status is less
        // damaging than a worker crash mid-build).
        Nix::Built | Nix::Substituted | Nix::AlreadyValid | Nix::ResolvesToAlreadyValid => {
            debug_assert!(false, "nix_failure_to_proto called with success status {nix:?}");
            BuildResultStatus::Built
        }

        // 1:1 mappings — proto variant exists with identical semantics.
        Nix::PermanentFailure => BuildResultStatus::PermanentFailure,
        Nix::TransientFailure => BuildResultStatus::TransientFailure,
        Nix::CachedFailure => BuildResultStatus::CachedFailure,
        Nix::DependencyFailed => BuildResultStatus::DependencyFailed,
        Nix::LogLimitExceeded => BuildResultStatus::LogLimitExceeded,
        Nix::OutputRejected => BuildResultStatus::OutputRejected,
        Nix::InputRejected => BuildResultStatus::InputRejected,
        Nix::TimedOut => BuildResultStatus::TimedOut,
        Nix::NotDeterministic => BuildResultStatus::NotDeterministic,

        // Intentional collapse: MiscFailure is nix-daemon's own catch-all
        // (used when it can't classify). PermanentFailure is the honest
        // proto equivalent — "it failed, we don't know why, don't retry."
        Nix::MiscFailure => BuildResultStatus::PermanentFailure,

        // Intentional collapse: NoSubstituters means "I was asked to
        // substitute and couldn't find a substituter." Our workers run
        // with `substitute = false` (WORKER_NIX_CONF) — we never ask the
        // daemon to substitute. If we see this, something is misconfigured;
        // PermanentFailure + the error_msg is the right signal.
        Nix::NoSubstituters => BuildResultStatus::PermanentFailure,
    }
}
```

Then the call site at `mod.rs:736-744` becomes:

```rust
    } else {
        tracing::warn!(
            drv_path = %drv_path,
            status = ?build_result.status,
            error = %build_result.error_msg,
            "build failed"
        );
        ProtoBuildResult {
            status: nix_failure_to_proto(build_result.status).into(),
            error_msg: build_result.error_msg.clone(),
            ..Default::default()
        }
    };
```

### Fix part C: scheduler completion handler

`rio-scheduler/src/actor/completion.rs:140-184` matches `BuildResultStatus` exhaustively by enumeration — adding proto variants without scheduler arms would **compile** (proto enums in Rust are i32-backed, prost generates a `try_from` with an `Unspecified` fallback — new values land in the `Unspecified` arm at line 176 and get treated as **transient** = reassign). That's the exact bug we're fixing. Must add arms.

**Diff** (`rio-scheduler/src/actor/completion.rs`, extend the permanent-failure arm at line 155-162):

```rust
            rio_proto::types::BuildResultStatus::PermanentFailure
            | rio_proto::types::BuildResultStatus::CachedFailure
            | rio_proto::types::BuildResultStatus::DependencyFailed
            | rio_proto::types::BuildResultStatus::LogLimitExceeded
            | rio_proto::types::BuildResultStatus::OutputRejected
            // TimedOut: same inputs → same timeout. Reassigning is a
            // storm. The operator fix is "raise spec.timeoutSeconds,"
            // not "try another worker."
            | rio_proto::types::BuildResultStatus::TimedOut
            // NotDeterministic: nix --check failed. Retrying doesn't
            // help — the nondeterminism is in the build itself.
            | rio_proto::types::BuildResultStatus::NotDeterministic
            // InputRejected: corrupt/invalid .drv. Same .drv on another
            // worker is still corrupt.
            | rio_proto::types::BuildResultStatus::InputRejected => {
                self.handle_permanent_failure(drv_hash, &result.error_msg, worker_id)
                    .await;
            }
```

### Fix part D: metric outcome label

`rio-worker/src/runtime.rs:395-401` buckets everything non-`Built`/non-`Cancelled` as `"failure"`. §2.3 of the parent doc calls out metric pollution specifically: `rio_worker_builds_total{outcome="permanent_failure"}` counting timeouts confuses SLI dashboards.

**Diff** (`rio-worker/src/runtime.rs:395-401`):

```rust
        let outcome = match &completion.result {
            Some(r) => match rio_proto::types::BuildResultStatus::try_from(r.status) {
                Ok(rio_proto::types::BuildResultStatus::Built) => "success",
                Ok(rio_proto::types::BuildResultStatus::Cancelled) => "cancelled",
                // Operationally distinct: means "raise the limit," not
                // "the build is broken." Separate label so SLI queries
                // can exclude these from failure-rate denominators.
                Ok(rio_proto::types::BuildResultStatus::TimedOut) => "timed_out",
                Ok(rio_proto::types::BuildResultStatus::LogLimitExceeded) => "log_limit",
                Ok(rio_proto::types::BuildResultStatus::InfrastructureFailure) => "infra_failure",
                _ => "failure",
            },
            None => "failure",
        };
```

Cardinality: 6 label values, static set — safe.

---

## 3. `wkr-timeout-is-infra` — timeout is a build outcome, not an `Err`

### Current (`rio-worker/src/executor/daemon/stderr_loop.rs:90-95`)

```rust
let build_result = tokio::time::timeout(
    build_timeout,
    read_build_stderr_loop(stdout, batcher, log_tx),
)
.await
.map_err(|_| ExecutorError::BuildFailed("build timed out".into()))??;
```

`tokio::time::timeout` returns `Result<Inner, Elapsed>`. The `.map_err` turns `Elapsed` into `Err(ExecutorError::BuildFailed)`. Both `?` propagate: outer for `Elapsed`, inner for `WireError`. The caller (`run_daemon_build`) returns `Err`, which `execute_build` propagates via `build_result?` at line 595, which `spawn_build_task` catches at `runtime.rs:334` and — absent a cancel flag — classifies as `InfrastructureFailure`. Scheduler reassigns (`completion.rs:152`).

### Why this is architecturally wrong

`ExecutorError` means "the executor couldn't do its job" — overlay mount failed, daemon wouldn't spawn, cgroup write EACCES. Those are infrastructure faults; retrying on another worker is correct.

A build timeout means "the executor did its job perfectly; the build itself didn't finish in time." That's a `BuildResult` with `status = TimedOut`. Same category as `LogLimitExceeded` — which `stderr_loop.rs:185` **already** handles correctly by returning `Ok(BuildResult::failure(LogLimitExceeded, ...))`.

### Fix

**Diff** (`rio-worker/src/executor/daemon/stderr_loop.rs:90-97`):

```rust
    // Timeout is a BUILD OUTCOME, not an executor error. Returning
    // Ok(failure) flows through execute_build's status-mapping path
    // (nix_failure_to_proto → BuildResultStatus::TimedOut), which the
    // scheduler treats as permanent-no-reassign. Returning Err would
    // land in runtime.rs's InfrastructureFailure arm → reassignment
    // storm (same build, same inputs, same timeout, forever).
    //
    // The inner Result<BuildResult, WireError> is different: a wire
    // error mid-STDERR-loop IS an executor fault (daemon died, pipe
    // corrupted) — that `?` stays.
    //
    // r[impl worker.timeout.no-reassign]
    let build_result = match tokio::time::timeout(
        build_timeout,
        read_build_stderr_loop(stdout, batcher, log_tx),
    )
    .await
    {
        Ok(inner) => inner?,
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = build_timeout.as_secs(),
                "build exceeded timeout; reporting TimedOut (no reassignment)"
            );
            BuildResult::failure(
                BuildStatus::TimedOut,
                format!("build exceeded configured timeout of {}s", build_timeout.as_secs()),
            )
        }
    };

    Ok(build_result)
```

### Interaction with fix 1

With fix 3, `run_daemon_build` returns `Ok(BuildResult { status: TimedOut })` on timeout. `execute_build` stores this in `build_result` at line 552. The cleanup sequence at 575–589 runs (daemon.kill, daemon.wait, and with fix 1 applied: cgroup.kill, drain, drop). Then line 595 `build_result?` is `Ok` → line 600 `is_success()` is `false` → line 728 else-branch → `nix_failure_to_proto(TimedOut)` → `BuildResultStatus::TimedOut`.

Without fix 1, fix 3 would still stop the reassignment storm, but the cgroup would still leak. **Both are needed.**

---

## 4. `wkr-cancel-flag-stale` — only trust the flag if the kill actually fired

### Current (`rio-worker/src/runtime.rs:187-208`)

```rust
// Set flag BEFORE kill: if there's a race where execute_build
// is reading the flag right now, we want "cancelled=true" to
// be visible by the time it sees the Err from run_daemon_build.
cancelled.store(true, Ordering::Release);

match crate::cgroup::kill_cgroup(cgroup_path) {
    Ok(()) => { ...; true }
    Err(e) => {
        // cgroup gone (race with Drop) or kernel too old ...
        tracing::warn!(...);
        true   // ← flag stays true even though nothing was killed
    }
}
```

### The race

The cancel registry entry is inserted at `runtime.rs:261` — **before** `execute_build` runs. The cgroup is created at `executor/mod.rs:471` — partway through `execute_build`, after overlay setup (~seconds), drv parsing, daemon spawn.

So there's a window (overlay setup + daemon spawn, potentially seconds under FUSE stall) where a `CancelSignal` finds the registry entry, sets the flag, writes `cgroup.kill` → `ENOENT` (dir doesn't exist yet), and returns. The build proceeds.

**Scenario A (benign):** build completes normally → `Ok(ExecutionResult)` → `runtime.rs:327` arm, flag never read. No bug.

**Scenario B (the bug):** build hits an **unrelated** executor error after the failed cancel — overlay teardown fails, daemon handshake fails, FOD verify panics. `runtime.rs:334` `Err` arm reads `build_cancelled.load()` → `true` → reports `Cancelled` instead of `InfrastructureFailure`. Operator sees "cancelled" in the Build CR status, assumes user action, never investigates the real infra fault.

**Scenario C (design-acknowledged, unchanged by this fix):** cancel arrives early, `ENOENT`, build proceeds and succeeds. The cancel is silently lost. `docs/src/components/worker.md:361` documents this as acceptable: "logged and ignored; the cancel is lost and the build proceeds (harmless: a cancel mid-setup will be retried by the scheduler's backstop timeout if needed)." This fix doesn't change scenario C — only scenario B.

### Fix: clear flag on `ENOENT`, keep set-before-kill ordering

The set-before-kill ordering is load-bearing (comment at line 187-192) for the **happy** path: kill succeeds → daemon dies → stdout EOF → `run_daemon_build` returns `Err` → by the time `runtime.rs:342` reads the flag, it must be `true`. Reversing to kill-then-set would narrow that window.

So: set first, kill, **undo on `ENOENT`**. `ENOENT` is the only errno that definitively means "kill did not happen" — other errors (EACCES, EINVAL) are ambiguous and we leave the flag set (conservative: misclassifying infra-as-cancelled is bad, but misclassifying cancel-as-infra just means a spurious warn).

**Diff** (`rio-worker/src/runtime.rs:193-208`):

```rust
    // Set flag BEFORE kill: if there's a race where execute_build
    // is reading the flag right now, we want "cancelled=true" to
    // be visible by the time it sees the Err from run_daemon_build.
    // The kill → stdout EOF → Err path has some latency (kernel
    // delivers SIGKILL, process dies, pipe closes, tokio wakes);
    // setting the flag first gives us a wider window.
    cancelled.store(true, std::sync::atomic::Ordering::Release);

    match crate::cgroup::kill_cgroup(cgroup_path) {
        Ok(()) => {
            tracing::info!(drv_path, cgroup = %cgroup_path.display(), "build cancelled via cgroup.kill");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Cgroup doesn't exist — cancel arrived before execute_build
            // reached BuildCgroup::create (overlay setup + daemon spawn
            // window). The kill DID NOT HAPPEN. Undo the flag so an
            // unrelated executor Err later isn't misclassified as
            // Cancelled (operator would see "cancelled", never
            // investigate the real fault).
            //
            // The cancel itself is lost — see worker.md:361, scheduler's
            // backstop timeout is the safety net. We could stash a
            // "deferred cancel" and have execute_build check it post-
            // cgroup-create, but the window is narrow and the backstop
            // already covers it.
            //
            // r[impl worker.cancel.flag-clear-enoent]
            cancelled.store(false, std::sync::atomic::Ordering::Release);
            tracing::debug!(
                drv_path,
                cgroup = %cgroup_path.display(),
                "cancel: cgroup not yet created (early-arrival race); flag cleared"
            );
            false
        }
        Err(e) => {
            // EACCES (delegation broken?) / EINVAL (kernel < 5.14?).
            // We don't know if the kill landed. Leave the flag set —
            // if the build IS still running and later errs, we'll
            // misclassify as Cancelled, but that's less bad than the
            // reverse (kill DID land, we clear flag, build errs from
            // the kill, we report InfrastructureFailure → reassign).
            tracing::warn!(drv_path, error = %e, "cgroup.kill failed (non-ENOENT); flag left set");
            true
        }
    }
```

---

## 5. Tests

### 5.1 Unit: exhaustive status mapping

**Location:** `rio-worker/src/executor/mod.rs` tests module (after `test_sanitize_build_id`)

```rust
    /// Every non-success Nix BuildStatus maps to a proto status. No
    /// variant hits a `_` arm — the mapping fn is exhaustive so adding
    /// a Nix variant is a compile error.
    ///
    /// Stronger than "compiles": asserts the MAPPING DECISIONS stay
    /// stable. If someone changes TimedOut → TransientFailure (which
    /// would reintroduce the reassignment storm), this test fails.
    ///
    /// r[verify worker.status.nix-to-proto]
    #[test]
    fn test_nix_failure_to_proto_is_exhaustive_and_stable() {
        use rio_nix::protocol::build::BuildStatus as Nix;
        use rio_proto::types::BuildResultStatus as Proto;

        // 1:1 mappings — each Nix failure gets its OWN proto variant.
        let one_to_one = [
            (Nix::PermanentFailure, Proto::PermanentFailure),
            (Nix::TransientFailure, Proto::TransientFailure),
            (Nix::CachedFailure,    Proto::CachedFailure),
            (Nix::DependencyFailed, Proto::DependencyFailed),
            (Nix::LogLimitExceeded, Proto::LogLimitExceeded),
            (Nix::OutputRejected,   Proto::OutputRejected),
            (Nix::InputRejected,    Proto::InputRejected),
            (Nix::TimedOut,         Proto::TimedOut),
            (Nix::NotDeterministic, Proto::NotDeterministic),
        ];
        for (nix, want) in one_to_one {
            assert_eq!(nix_failure_to_proto(nix), want, "1:1 mapping broke for {nix:?}");
        }

        // Intentional collapses — documented reasons in the fn body.
        assert_eq!(nix_failure_to_proto(Nix::MiscFailure),    Proto::PermanentFailure);
        assert_eq!(nix_failure_to_proto(Nix::NoSubstituters), Proto::PermanentFailure);
    }

    /// TimedOut must NOT map to anything the scheduler reassigns. This
    /// is the load-bearing invariant for the reassignment-storm fix.
    ///
    /// r[verify worker.timeout.no-reassign]
    #[test]
    fn test_timed_out_is_not_reassignable() {
        use rio_nix::protocol::build::BuildStatus as Nix;
        use rio_proto::types::BuildResultStatus as Proto;

        let mapped = nix_failure_to_proto(Nix::TimedOut);
        // completion.rs:151-152: these two trigger handle_transient_failure
        // (reassign). TimedOut must not be either.
        assert_ne!(mapped, Proto::TransientFailure, "TimedOut → reassign storm");
        assert_ne!(mapped, Proto::InfrastructureFailure, "TimedOut → reassign storm");
        // And it must not be Unspecified (which ALSO reassigns per
        // completion.rs:176-183).
        assert_ne!(mapped, Proto::Unspecified);
    }
```

### 5.2 Unit: cancel flag cleared on `ENOENT`

**Location:** `rio-worker/src/runtime.rs` tests module (this file has tests at line 562+)

```rust
    /// Cancel arrives before cgroup exists → kill ENOENT → flag cleared.
    /// An unrelated Err later must NOT be misclassified as Cancelled.
    ///
    /// r[verify worker.cancel.flag-clear-enoent]
    #[test]
    fn test_cancel_enoent_clears_flag() {
        let registry = CancelRegistry::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        // Path that definitely doesn't exist. tmpdir/nonexistent so
        // the test doesn't depend on /sys/fs/cgroup being mounted (CI
        // sandbox may not have cgroup v2).
        let tmp = tempfile::tempdir().unwrap();
        let fake_cgroup = tmp.path().join("not-created-yet");
        registry.write().unwrap().insert(
            "/nix/store/abc-test.drv".into(),
            (fake_cgroup, Arc::clone(&cancelled)),
        );

        let got = try_cancel_build(&registry, "/nix/store/abc-test.drv");

        // Kill was a no-op (ENOENT) → cancel did NOT happen → false.
        assert!(!got, "ENOENT cancel should return false (nothing killed)");
        // Load-bearing: flag must be FALSE so a later Err is correctly
        // classified as InfrastructureFailure, not Cancelled.
        assert!(
            !cancelled.load(Ordering::Acquire),
            "flag must be cleared on ENOENT; otherwise unrelated Err → misclassified as Cancelled"
        );
    }
```

### 5.3 VM: build timeout end-to-end

**Location:** new fragment `build-timeout` in `nix/tests/scenarios/lifecycle.nix`, composed into `vm-lifecycle-core-k3s` (or a dedicated `vm-lifecycle-timeout-k3s` if core's runtime budget is tight — core is already 10.7min per project-state.md).

**Fixture:** `drvs.mkTrivial { marker = "lifecycle-timeout"; sleepSecs = 30; }` with `Build.spec.timeoutSeconds: 5`.

```python
# r[verify worker.cgroup.kill-on-teardown]
# r[verify worker.timeout.no-reassign]
#
# build-timeout — Build.spec.timeoutSeconds shorter than the build's sleep.
#
# Asserts THREE things:
#   1. Build CR reaches phase=Failed (NOT phase=Pending-again, which would
#      mean scheduler reassigned it as InfrastructureFailure).
#   2. cgroup is GONE after failure (NOT leaked EBUSY — proves fix 1,
#      cgroup.kill fired before Drop rmdir).
#   3. Same drv, second build succeeds (NOT EEXIST on cgroup create —
#      proves the leak really is closed, not just "rmdir warned").
#
# The sleepSecs=30 vs timeoutSeconds=5 gap is wide: under TCG dispatch
# lag the timeout may fire at ~8-12s wall-clock, but the sleep is nowhere
# near done. Narrower gaps flake.
with subtest("build-timeout: spec.timeoutSeconds < build duration → TimedOut, cgroup cleaned"):
    drv_path = client.succeed(
        "nix-instantiate "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${timeoutDrv} 2>/dev/null"
    ).strip()
    client.succeed(f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}")

    k3s_server.succeed(
        "k3s kubectl apply -f - <<'EOF'\n"
        "apiVersion: rio.build/v1alpha1\n"
        "kind: Build\n"
        "metadata:\n"
        "  name: test-timeout\n"
        "  namespace: ${ns}\n"
        "spec:\n"
        f"  derivation: {drv_path}\n"
        "  timeoutSeconds: 5\n"
        "EOF"
    )

    # Wait for Building so we know the cgroup exists. Same 120s budget
    # as cancel-cgroup-kill (TCG dispatch lag variance).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} get build test-timeout "
        "-o jsonpath='{.status.phase}' | grep -qx Building",
        timeout=120,
    )

    # Resolve worker node + cgroup path — same pattern as cancel-cgroup-kill.
    # sanitize_build_id: "...lifecycle-timeout.drv" → "*lifecycle-timeout_drv".
    worker_node = k3s_server.succeed(
        "k3s kubectl -n ${ns} get pod default-workers-0 "
        "-o jsonpath='{.spec.nodeName}'"
    ).strip()
    worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
    cgroup_path = worker_vm.succeed(
        "find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
        "-print -quit 2>/dev/null"
    ).strip()
    assert cgroup_path, "cgroup not found — build not actually running?"
    procs = int(worker_vm.succeed(f"wc -l < {cgroup_path}/cgroup.procs").strip())
    assert procs > 0, f"cgroup empty ({cgroup_path}) — builder not in cgroup"

    # ── Assertion 1: terminal phase is Failed, NOT re-queued. ──
    # timeoutSeconds=5 + worker report latency + controller reconcile
    # ≈ 10-15s typical, 60s is TCG headroom.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} get build test-timeout "
        "-o jsonpath='{.status.phase}' | grep -qx Failed",
        timeout=60,
    )
    # Message should mention timeout (from stderr_loop.rs's error_msg).
    # Not load-bearing (controller may paraphrase) — informational print.
    msg = k3s_server.succeed(
        "k3s kubectl -n ${ns} get build test-timeout "
        "-o jsonpath='{.status.message}'"
    ).strip()
    print(f"build-timeout: status.message = {msg!r}")

    # ── Assertion 1b: it was NOT reassigned. ──
    # If the scheduler had treated this as InfrastructureFailure, the
    # drv would cycle Building → Pending → Building on a retry before
    # eventually poisoning. Scrape the scheduler's derivations_running
    # metric: it should be 0 for this drv_hash now (terminal), and
    # rio_scheduler_builds_reassigned_total should NOT have incremented
    # for a timeout. We use the simpler proxy: phase reached Failed
    # WITHOUT passing through Pending again. Check Build CR conditions
    # history — there should be exactly ONE Building→Failed transition.
    # (kubectl doesn't surface transition history easily; fall back to
    # asserting the worker metric has outcome=timed_out.)
    #
    # Scrape via apiserver proxy (worker pod is distroless, no curl).
    # Port 9091 is the metrics port (common.nix). NOTE: use the numeric
    # port, not "metrics" — named port lookup panics (see tooling-gotchas).
    metrics = k3s_server.succeed(
        "k3s kubectl -n ${ns} get --raw "
        "/api/v1/namespaces/${ns}/pods/default-workers-0:9091/proxy/metrics"
    )
    timed_out_line = [
        l for l in metrics.splitlines()
        if 'rio_worker_builds_total' in l and 'outcome="timed_out"' in l
    ]
    assert timed_out_line, (
        "rio_worker_builds_total{outcome=\"timed_out\"} missing — "
        "timeout went through Err→InfrastructureFailure path instead of "
        "Ok(TimedOut)→nix_failure_to_proto path"
    )
    # Belt+suspenders: outcome=infra_failure should NOT have incremented
    # (it didn't exist before this build, and this build was a timeout).
    infra_line = [
        l for l in metrics.splitlines()
        if 'rio_worker_builds_total' in l and 'outcome="infra_failure"' in l
        and not l.startswith('#')
    ]
    if infra_line:
        # Line exists (some prior build may have hit infra) — parse
        # value, should be 0 in a fresh lifecycle run.
        val = float(infra_line[0].rsplit(' ', 1)[1])
        assert val == 0, f"infra_failure count = {val} — timeout misclassified"

    # ── Assertion 2: cgroup is GONE. ──
    # Kernel rejects rmdir on non-empty cgroup (EBUSY), so
    # not-exists ⇒ procs emptied ⇒ cgroup.kill fired. Without fix 1
    # this would EBUSY-leak — the sleep 30 builder process is still
    # alive (only ~5-15s elapsed, daemon.kill() doesn't reach it).
    worker_vm.wait_until_succeeds(
        f"! test -e {cgroup_path}",
        timeout=30,
    )
    print(f"build-timeout: cgroup {cgroup_path} removed (builder killed, rmdir succeeded)")

    # ── Assertion 3: second build of SAME drv succeeds. ──
    # Without fix 1, BuildCgroup::create → mkdir → EEXIST (leaked
    # cgroup from attempt 1). With fix 1, clean slate.
    # Delete + reapply WITHOUT timeoutSeconds so it actually finishes.
    kubectl("delete build test-timeout --wait=true")
    k3s_server.succeed(
        "k3s kubectl apply -f - <<'EOF'\n"
        "apiVersion: rio.build/v1alpha1\n"
        "kind: Build\n"
        "metadata:\n"
        "  name: test-timeout-retry\n"
        "  namespace: ${ns}\n"
        "spec:\n"
        f"  derivation: {drv_path}\n"
        "EOF"
    )
    # sleepSecs=30 + dispatch lag → 120s is generous.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} get build test-timeout-retry "
        "-o jsonpath='{.status.phase}' | grep -qx Succeeded",
        timeout=120,
    )
    kubectl("delete build test-timeout-retry --wait=false")
```

**Derivation** (add to lifecycle.nix's let block, near `cancelDrv` at line 164):

```nix
  # build-timeout victim. sleepSecs=30 vs spec.timeoutSeconds=5 — wide
  # gap so TCG dispatch lag (timeout may fire at 8-12s wall) never lets
  # the sleep finish first. Same marker-in-drvname pattern so the
  # cgroup dir is findable from the VM host.
  timeoutDrv = drvs.mkTrivial {
    marker = "lifecycle-timeout";
    sleepSecs = 30;
  };
```

**Composition** (`nix/tests/default.nix`): either append `"build-timeout"` to `vm-lifecycle-core-k3s`'s fragment list (pushes core from ~10.7min to ~13min — the second build's sleep 30 dominates), or create `vm-lifecycle-timeout-k3s` as a standalone. Given project-state.md says 6 more splits are planned, standalone is cleaner:

```nix
  vm-lifecycle-timeout-k3s = lifecycleMod.mkTest {
    name = "lifecycle-timeout";
    fragments = [ "build-timeout" ];
  };
```

---

## 6. Spec + tracey

Tracey markers: `r[worker.cgroup.kill-on-teardown]`, `r[worker.timeout.no-reassign]`, `r[worker.status.nix-to-proto]`, `r[worker.cancel.flag-clear-enoent]` — all in [`worker.md`](../../components/worker.md).

- `r[worker.cgroup.kill-on-teardown]`: after `daemon.wait()` returns, executor writes `cgroup.kill` and drains `cgroup.procs` before dropping `BuildCgroup` (`daemon.kill()` only reaches direct child; builder is grandchild).
- `r[worker.timeout.no-reassign]`: build timeout produces `Ok(BuildResult { status: TimedOut })`, not `Err(ExecutorError)` — timeout is a build outcome, not infrastructure failure (would otherwise reassign forever).
- `r[worker.status.nix-to-proto]`: Nix→proto `BuildStatus` mapping is exhaustive (no `_` arm); only `MiscFailure`/`NoSubstituters` collapse to `PermanentFailure`.
- `r[worker.cancel.flag-clear-enoent]`: if `try_cancel_build`'s `cgroup.kill` write returns `ENOENT` (raced ahead of `BuildCgroup::create`), the `cancelled` flag is cleared.

None overlap existing rules — `worker.cancel.cgroup-kill` is the cancel path specifically; the first above adds the teardown path.

---

## 7. Rollout order

The fixes have a dependency order:

1. **Proto variants first** (`types.proto` +3 variants) — everything downstream references them.
2. **Scheduler match arm** (`completion.rs`) — must land same commit as proto or skew risk (new variants → `Unspecified` → reassign).
3. **`nix_failure_to_proto` + call site** — can't reference `TimedOut` until proto has it.
4. **`stderr_loop.rs` timeout → `Ok`** — independent; can land anytime after step 3 (needs the mapping to route `TimedOut` correctly).
5. **`cgroup.kill()` + drain in `mod.rs`** — fully independent of 1-4. Could land first.
6. **`runtime.rs` flag-clear-on-ENOENT** — fully independent.
7. **Metric outcome label** — depends on proto variants existing.
8. **Unit tests** — same commit as their respective impl.
9. **VM fragment** — last; validates the whole stack.

Suggested commit sequence:

| # | Commit | Scope |
|---|---|---|
| 1 | `fix(worker): kill cgroup tree on teardown (not just daemon)` | mod.rs cgroup.kill + drain + scopeguard hardening |
| 2 | `fix(worker): clear cancel flag on ENOENT kill` | runtime.rs + unit test |
| 3 | `feat(proto): add TimedOut/NotDeterministic/InputRejected BuildResultStatus` | types.proto + scheduler completion.rs arms |
| 4 | `fix(worker): exhaustive Nix→proto status mapping` | nix_failure_to_proto + call site + unit tests + metric labels |
| 5 | `fix(worker): build timeout is Ok(TimedOut) not Err` | stderr_loop.rs |
| 6 | `docs(worker): spec rules for timeout/cgroup/status` | worker.md + tracey annotations |
| 7 | `test(vm): lifecycle build-timeout fragment` | lifecycle.nix + default.nix |

Commits 1+2 are shippable standalone (close the cgroup leak + flag staleness without touching proto). Commits 3-5 must ship together (proto skew). Commit 7 validates the lot.

---

## 8. Risk notes

- **`cgroup.kill` on success path is a no-op** — verified: writing `1` to an empty cgroup's `cgroup.kill` is harmless (kernel walks an empty tree). No behavior change for successful builds beyond one extra syscall + 10ms poll loop that exits immediately.
- **Drain loop on blocking pool** — 200 × 10ms = 2s worst case on a blocking thread. Concurrent builds (up to semaphore limit) each take a blocking thread during teardown. Tokio's blocking pool defaults to 512 threads; with `max_concurrent_builds` typically ≤ 32 this is fine.
- **Proto wire compat** — adding enum variants is additive. Old scheduler receiving new-variant status from new worker decodes as 0 (`Unspecified`) → treats as transient → reassigns. Exactly the storm we're fixing. **Deploy scheduler and worker together** (they already are, same Helm chart).
- **VM fragment runtime** — ~2.5min (Building wait 120s budget, but typical 10-20s + timeout fire at 5s + Failed wait + cgroup-gone + second build 30s sleep). Standalone fragment avoids inflating `vm-lifecycle-core-k3s`.
