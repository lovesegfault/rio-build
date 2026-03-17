# Remediation 15: Shutdown-signal cluster

**Parent:** [`phase4a.md` §2.9](../phase4a.md#29-worker-sigint-missing)
**Findings:** `wkr-sigint-not-handled`, `wkr-hb-exit-bypasses-raii`
**Blast radius:** P1 — local-dev profraw loss (Ctrl+C), FUSE mount leak on heartbeat death (next worker start `EBUSY`)
**Effort:** ~1 h implementation + one VM cycle

---

## Scope

One finding, one crate — but the audit of "who rolls their own signal
handling" turned up two more loops that never return. This remediation
clusters all three into a single PR because they share root cause
(infinite loops with no cancellation token) and because the test asset
that verifies the worker fix (`sigint-graceful` fragment) gives the
controller fix a free home.

| Binary | Site | Problem | Consequence |
|---|---|---|---|
| `rio-worker` | `main.rs:386` | SIGTERM-only, hand-rolled | Ctrl+C → default handler → no profraw, mount leak |
| `rio-worker` | `main.rs:433` | `std::process::exit(1)` | heartbeat death → FUSE mount leak → next start `EBUSY` |
| `rio-controller` | `main.rs:313` | `Autoscaler::run()` infinite loop | runtime-abort on exit, not graceful return |
| `rio-controller` | `main.rs:382` | health accept loop infinite | same |

The controller entries are **not** correctness bugs — the tokio runtime
aborts spawned tasks on drop, and `main()` still returns normally (so
`atexit` → profraw flush works). They are consistency fixes: after this,
`grep -r 'loop {' rio-*/src/main.rs` shows zero loops without a
cancellation arm.

---

## 1. `rio-worker/src/main.rs:386` — adopt `shutdown_signal()`

Every other binary (`rio-scheduler/src/main.rs:171`,
`rio-gateway/src/main.rs:196`, `rio-store/src/main.rs:164`) calls
`rio_common::signal::shutdown_signal()` once near the top of `main()`
and threads the `CancellationToken` through every loop. The worker rolled
its own and only watches SIGTERM.

```diff
--- a/rio-worker/src/main.rs
+++ b/rio-worker/src/main.rs
@@ -33,6 +33,10 @@ async fn main() -> anyhow::Result<()> {
     let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

     let cli = CliArgs::parse();
     let cfg: Config = rio_common::config::load("worker", cli)?;
     let _otel_guard = rio_common::observability::init_tracing("worker")?;
+
+    // One token, cancelled on SIGTERM OR SIGINT. Cloned into every
+    // loop that must break for main() to return (profraw flush,
+    // FUSE Drop).
+    let shutdown = rio_common::signal::shutdown_signal();
```

Then replace the hand-rolled `sigterm` binding and select-arm. The
`StreamEnd::Sigterm` variant is renamed — it now covers both signals,
and "Sigterm" in the name would mislead the next reader.

```diff
@@ -378,15 +382,13 @@
     // scheduler will eventually time us out via heartbeat (we're still
     // heartbeating until exit) and WorkerDisconnected will reassign.
     //
-    // select! is biased toward sigterm: poll it FIRST each iteration.
+    // select! is biased toward shutdown: poll it FIRST each iteration.
     // Without `biased;`, select! picks a ready branch pseudorandomly —
-    // under heavy assignment traffic, SIGTERM could starve behind
+    // under heavy assignment traffic, the token could starve behind
     // stream messages. K8s sends SIGTERM then starts the grace period
     // clock; we want to react immediately, not after the next gap in
     // assignments.
-    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
-
     'reconnect: loop {
```

```diff
@@ -436,11 +438,11 @@
             tokio::select! {
                 biased;

-                _ = sigterm.recv() => {
-                    break StreamEnd::Sigterm;
+                _ = shutdown.cancelled() => {
+                    break StreamEnd::Shutdown;
                 }
```

```diff
@@ -505,9 +507,9 @@
         };

         match stream_end {
-            StreamEnd::Sigterm => break 'reconnect,
+            StreamEnd::Shutdown => break 'reconnect,
             StreamEnd::Closed | StreamEnd::Error => {
```

```diff
@@ -641,11 +643,11 @@
 }

-/// Why the inner select loop exited. Sigterm breaks the outer
+/// Why the inner select loop exited. Shutdown breaks the outer
 /// reconnect loop; Closed/Error trigger a reconnect.
 enum StreamEnd {
-    Sigterm,
+    Shutdown,
     Closed,
     Error,
 }
```

### 1.1 Bonus: close the reconnect-loop blind spot

`nix/tests/common.nix:472-485` documents a workaround: `collectCoverage`
stops workers FIRST because if the scheduler dies first, the worker
enters the reconnect retry body (`main.rs:413-425`, `:521-523`) which
**does not poll the signal** — it sleeps, then `continue 'reconnect`s
back to `build_execution().await`. systemd's 90s `TimeoutStopSec` then
SIGKILLs. The Pass-1/Pass-2 ordering in `collectCoverage` papers over
this.

With a token, the fix is a one-armed `select!` around each sleep:

```diff
@@ -419,12 +421,18 @@
             Err(e) => {
                 // Leader still settling, or balance channel hasn't
                 // caught up. Back off and retry. The balanced
                 // channel's probe loop rediscovers within ~3s.
                 tracing::warn!(error = %e, "BuildExecution open failed; retrying in 1s");
                 relay_target_tx.send_replace(None);
-                tokio::time::sleep(Duration::from_secs(1)).await;
-                continue 'reconnect;
+                tokio::select! {
+                    biased;
+                    _ = shutdown.cancelled() => break 'reconnect,
+                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
+                }
             }
         };
```

```diff
@@ -517,10 +525,15 @@
                     "BuildExecution stream ended; reconnecting (running builds continue)"
                 );
                 relay_target_tx.send_replace(None);
-                tokio::time::sleep(Duration::from_secs(1)).await;
-                continue 'reconnect;
+                tokio::select! {
+                    biased;
+                    _ = shutdown.cancelled() => break 'reconnect,
+                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
+                }
             }
         }
     }
```

`break 'reconnect` here is the same exit the inner select takes — falls
through to `run_drain()` then `drop(fuse_session)`. The
`collectCoverage` Pass-1/Pass-2 comment can be updated to note the
workaround is no longer load-bearing (keep the ordering — it's still
correct, just not required for profraw).

---

## 2. `rio-worker/src/main.rs:431-434` — return `Err`, don't `exit(1)`

When the heartbeat task finishes (panic caught by `spawn_monitored`,
or the infinite loop somehow returned), the current code bails out
hard:

```rust
// main.rs:431-434 (current)
if heartbeat_handle.is_finished() {
    tracing::error!("heartbeat loop terminated unexpectedly; exiting");
    std::process::exit(1);
}
```

`std::process::exit()` is `-> !` via `libc::exit()`. No unwinding. The
`fuse_session` binding at `main.rs:207` holds a `fuser::BackgroundSession`
whose inner `Mount` runs `fusermount -u` on `Drop` — that never fires.
Next worker start on the same node: `mount_fuse_background` → `EBUSY` →
CrashLoopBackOff until a human runs `fusermount -u /var/rio/fuse-store`.

The fix is to let the stack unwind. `main()` is already
`-> anyhow::Result<()>` and every other error path uses `?`. `bail!`
expands to `return Err(anyhow!(...))`, which unwinds through
`drop(fuse_session)` at `:559`.

```diff
@@ -429,10 +431,14 @@
         let stream_end = loop {
             if heartbeat_handle.is_finished() {
-                tracing::error!("heartbeat loop terminated unexpectedly; exiting");
-                std::process::exit(1);
+                // bail! not exit(1): unwind the stack so fuse_session
+                // (main.rs:207) drops → Mount::drop → fusermount -u.
+                // exit(1) would leak the mount → next start EBUSY.
+                // Skip run_drain: heartbeat is the scheduler probe;
+                // if it's dead, DrainWorker won't land anyway.
+                anyhow::bail!("heartbeat loop terminated unexpectedly");
             }
```

**Why `bail!` and not a new `StreamEnd` variant:** heartbeat death is
not a "stream ended, reconnect" event — it's a "scheduler is
permanently unreachable from this worker" event. Flowing through
`break 'reconnect` → `run_drain()` would waste the grace-period budget
on a `connect_admin()` that can't succeed (same scheduler). Straight
`Err` out of `main()` → non-zero exit → systemd `Restart=on-failure`
restarts the pod, which is the right recovery.

---

## 3. `rio-controller/src/main.rs` — autoscaler + health get tokens

The two kube-rs `Controller` loops use `.shutdown_on_signal()`, which
installs kube-runtime's own SIGTERM/SIGINT watcher. When those fire,
`join(wp_controller, build_controller).await` at `:334` returns,
`main()` falls through, `Ok(())` returns, tokio runtime drops, spawned
tasks are aborted. `atexit` fires → profraw flushes. **Coverage is not
broken here.**

But `Autoscaler::run()` (`scaling.rs:163`) and the health accept loop
(`main.rs:391`) never observe the shutdown. They're force-aborted
mid-`interval.tick().await` or mid-`listener.accept().await`. Any
future state we might add (metrics flush on final tick, accept-queue
drain) would be silently skipped. And it's the only `main.rs` in the
workspace where `grep 'loop {'` finds a loop without a cancellation
arm.

`shutdown_signal()` coexists fine with kube-rs's `shutdown_on_signal()`:
tokio's `signal(SignalKind)` installs a shared process-wide disposition;
each `Signal` instance receives every signal of that kind. Two watchers
→ both fire.

```diff
--- a/rio-controller/src/main.rs
+++ b/rio-controller/src/main.rs
@@ -186,6 +186,13 @@ async fn main() -> anyhow::Result<()> {
     let cfg: Config = rio_common::config::load("controller", cli)?;
     let _otel_guard = rio_common::observability::init_tracing("controller")?;
+
+    // Cancellation for the non-kube-runtime loops (autoscaler,
+    // health). kube-rs's .shutdown_on_signal() installs its OWN
+    // SIGTERM/SIGINT watcher for the Controller loops — tokio's
+    // signal() is broadcast, both watchers fire. This token is
+    // for the spawn_monitored tasks that kube-rs doesn't manage.
+    let shutdown = rio_common::signal::shutdown_signal();
```

### 3.1 Autoscaler

`Autoscaler::run()` signature changes to take the token; the loop body
`select!`s on it.

```diff
--- a/rio-controller/src/scaling.rs
+++ b/rio-controller/src/scaling.rs
@@ -160,20 +160,26 @@ impl Autoscaler {
     /// Main loop. Never returns (barring panic). main.rs spawns
-    /// this via `spawn_monitored` — if it dies, logged, controller
-    /// keeps reconciling (just without autoscale).
-    pub async fn run(mut self) {
+    /// this via `spawn_monitored`. Returns on cancellation
+    /// (SIGTERM/SIGINT) or panic (logged by spawn_monitored;
+    /// controller keeps reconciling without autoscale).
+    pub async fn run(mut self, shutdown: rio_common::signal::Token) {
         let mut interval = tokio::time::interval(self.timing.poll_interval);
         // MissedTickBehavior::Skip: if one iteration takes >30s
         // (slow apiserver), don't fire twice immediately after.
         // Catch up on the NEXT normal tick.
         interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

         loop {
-            interval.tick().await;
+            tokio::select! {
+                biased;
+                _ = shutdown.cancelled() => return,
+                _ = interval.tick() => {}
+            }
             if let Err(e) = self.tick().await {
```

```diff
--- a/rio-controller/src/main.rs
+++ b/rio-controller/src/main.rs
@@ -310,7 +316,7 @@
     info!(?timing, "autoscaler timing");
     let autoscaler = Autoscaler::new(client.clone(), admin, timing, recorder);
-    rio_common::task::spawn_monitored("autoscaler", autoscaler.run());
+    rio_common::task::spawn_monitored("autoscaler", autoscaler.run(shutdown.clone()));
```

### 3.2 Health server

`spawn_health_server` takes the token and selects on it in the accept
loop. Cancellation → `return` (the spawned per-connection tasks are
fire-and-forget; they'll be aborted by runtime drop, which is fine —
a K8s probe mid-flight during shutdown doesn't matter).

```diff
@@ -378,10 +384,10 @@
 /// `200` — that's all we need to match.
-fn spawn_health_server(addr: std::net::SocketAddr) {
+fn spawn_health_server(addr: std::net::SocketAddr, shutdown: rio_common::signal::Token) {
     rio_common::task::spawn_monitored("health-server", async move {
         info!(addr = %addr, "starting HTTP health server");
         let listener = match tokio::net::TcpListener::bind(addr).await {
             Ok(l) => l,
             Err(e) => {
                 warn!(error = %e, addr = %addr, "health bind failed");
                 return;
             }
         };
         loop {
-            let Ok((mut stream, _)) = listener.accept().await else {
-                continue; // accept fail is transient; retry
-            };
+            let (mut stream, _) = tokio::select! {
+                biased;
+                _ = shutdown.cancelled() => return,
+                r = listener.accept() => match r {
+                    Ok(pair) => pair,
+                    Err(_) => continue, // accept fail is transient; retry
+                },
+            };
```

Call-site update (find the `spawn_health_server(cfg.health_addr)` call
near the top of `main()` — it's before the connect retry loop):

```diff
-    spawn_health_server(cfg.health_addr);
+    spawn_health_server(cfg.health_addr, shutdown.clone());
```

---

## 4. `CLAUDE.md` coverage section — drop stale parentheticals

Line `CLAUDE.md:81` claims the worker and controller "already had"
graceful shutdown when the coverage architecture was built. §2.9 shows
the worker's version was SIGTERM-only (no SIGINT, and the reconnect
body didn't poll it). After this remediation the claim is true — for
both signals, from every loop body — so the parenthetical becomes
noise. Drop it.

```diff
--- a/CLAUDE.md
+++ b/CLAUDE.md
@@ -79,7 +79,7 @@
 1. **Instrumented build** (`rio-workspace-cov`): `RUSTFLAGS="-C instrument-coverage"` + distinct pname → separate deps cache, builds in parallel with normal workspace.
 2. **Coverage-mode VM tests** (`vmTestsCov`): same tests with `coverage=true` → `LLVM_PROFILE_FILE=/var/lib/rio/cov/rio-%p-%m.profraw` set in all rio-* service envs (`%p`=PID for restarts, `%m`=binary signature for safe merging).
-3. **Graceful shutdown** (`rio-common::signal::shutdown_signal`): SIGTERM → CancellationToken → main() returns normally → atexit handlers fire → LLVM profraw flush. All binaries: store, scheduler, gateway, worker (already had it), controller (already had it).
+3. **Graceful shutdown** (`rio-common::signal::shutdown_signal`): SIGTERM or SIGINT → CancellationToken → main() returns normally → atexit handlers fire → LLVM profraw flush. All binaries: store, scheduler, gateway, worker, controller.
 4. **Collection** (`common.nix collectCoverage`): at end of each testScript, `systemctl stop rio-*` → graceful drain → tar `/var/lib/rio/cov` → `copy_from_vm` to `$out/coverage/<node>/profraw.tar.gz`. phase3a also deletes the k3s STS so the worker pod's hostPath-mounted profraws flush.
```

---

## 5. VM test — `sigint-graceful` fragment in `scheduling.nix`

The standalone fixture (`scheduling.nix`) runs `rio-worker` as a
systemd unit on real worker VMs — the only place we can deliver SIGINT
to a worker PID and inspect the aftermath from the host. The k3s
fixture's worker is distroless-in-a-pod (no shell, probed via
host-side cgroup walks per `tooling-gotchas.md`).

New fragment, independent (no FUSE-cache coupling), slots into
`vm-scheduling-disrupt-standalone` alongside `reassign` (both disturb
a worker and wait for systemd to bring it back).

### 5.1 Fragment body

```python
sigint-graceful = ''
  # ══════════════════════════════════════════════════════════════════
  # sigint-graceful — Ctrl+C path: FUSE unmounts, profraw flushes
  # ══════════════════════════════════════════════════════════════════
  # r[verify worker.shutdown.sigint]
  #
  # Before remediation 15: worker main.rs:386 watched SIGTERM only.
  # SIGINT hit the default handler → immediate termination → no
  # Drop, no atexit. FUSE mount leaked (next start EBUSY),
  # profraw never flushed (local dev Ctrl+C = zero coverage).
  #
  # The assertion is two-layered:
  #   1. mountpoint -q FAILS after unit stops → Mount::drop ran
  #      → fusermount -u ran → main() returned (stack unwound).
  #      This is the PRIMARY assertion — it proves SIGINT was
  #      caught, the token cancelled, the select! arm broke, and
  #      the drop at main.rs:559 executed.
  #   2. [coverage-mode only] a fresh profraw appeared with mtime
  #      after the kill. LLVM flushes via __llvm_profile_write_file
  #      registered in atexit — if main() returned, this fired.
  #      Without the fix, the process dies in the signal handler
  #      before atexit runs.
  #
  # Uses wsmall2: wsmall1 holds FUSE cache state for fuse-direct /
  # fuse-slowpath (core-test coupling). wsmall2 is disposable here.
  with subtest("sigint-graceful: SIGINT → FUSE unmounted → profraw flushed"):
      # Baseline: mount IS present (worker running, FUSE alive).
      # If this fails, the fixture is broken — not our bug.
      wsmall2.succeed("mountpoint -q /var/rio/fuse-store")

      # Coverage-mode baseline. ls may exit 2 (no match) in
      # non-coverage builds — `|| echo 0` swallows that.
      # We compare COUNT before/after, not existence: the worker
      # may have restarted during an earlier fragment (systemd
      # Restart=on-failure), leaving stale profraws. A strict
      # "file exists" check would pass for the wrong reason.
      profraw_before = int(wsmall2.succeed(
          "ls /var/lib/rio/cov/*.profraw 2>/dev/null | wc -l || echo 0"
      ).strip())

      # SIGINT, not SIGTERM. systemctl kill delivers to MainPID.
      # `systemctl stop` would send SIGTERM (KillSignal default) —
      # that path already works. This tests the NEW code.
      wsmall2.succeed("systemctl kill -s INT rio-worker.service")

      # Unit reaches inactive when main() returns. NOT wait_for_unit
      # (that waits for active). 30s: drain is near-instant with no
      # in-flight builds, but run_drain's connect_admin to a
      # still-live scheduler can take a few seconds under TCG.
      wsmall2.wait_until_succeeds(
          "systemctl is-active rio-worker.service | grep -qx inactive",
          timeout=30,
      )

      # PRIMARY: mount gone. `! mountpoint -q` inverts the exit.
      # If SIGINT hit the default handler, the process died without
      # running Mount::drop — fusermount never ran — mount is still
      # there (kernel keeps it until the fd closes, but systemd
      # already reaped the MainPID so the fd IS closed... actually
      # no: AutoUnmount means the kernel unmounts when the fuse fd
      # closes regardless. So this alone doesn't distinguish
      # graceful from crash).
      #
      # REVISED primary: check the service's exit code. main()
      # returning Ok(()) → exit 0. SIGINT default handler → exit
      # 130 (128+SIGINT). `systemctl show -p ExecMainStatus` gives
      # the raw status; ExecMainCode=1 (CLD_EXITED) + Status=0
      # is graceful; Code=2 (CLD_KILLED) + Status=2 is death-by-
      # signal.
      exit_info = wsmall2.succeed(
          "systemctl show rio-worker.service "
          "-p ExecMainCode -p ExecMainStatus"
      )
      assert "ExecMainCode=1" in exit_info, (
          f"worker should exit via return-from-main (CLD_EXITED=1), "
          f"not death-by-signal. Got: {exit_info!r}. "
          f"SIGINT handler not installed?"
      )
      assert "ExecMainStatus=0" in exit_info, (
          f"worker main() should return Ok(()) on SIGINT drain. "
          f"Got: {exit_info!r}"
      )

      # SECONDARY: mount gone. With ExecMainCode=1 proven above,
      # this is belt-and-suspenders — but it's cheap and it's what
      # a human debugging `EBUSY` would check first.
      wsmall2.succeed("! mountpoint -q /var/rio/fuse-store")

      # TERTIARY [coverage only]: profraw count went up.
      ${if coverage then ''
      profraw_after = int(wsmall2.succeed(
          "ls /var/lib/rio/cov/*.profraw 2>/dev/null | wc -l"
      ).strip())
      assert profraw_after > profraw_before, (
          f"graceful SIGINT should flush a fresh profraw via atexit; "
          f"before={profraw_before} after={profraw_after}. "
          f"main() returned (ExecMainCode=1 above) but atexit "
          f"didn't fire? LLVM_PROFILE_FILE unset in unit env?"
      )
      '' else ''
      pass  # profraw assertion is coverage-mode only
      ''}

      # Restart for later fragments + collectCoverage. systemd
      # Restart=on-failure does NOT fire for exit 0 (it's
      # on-FAILURE). Must start manually.
      wsmall2.succeed("systemctl start rio-worker.service")
      wsmall2.wait_for_unit("rio-worker.service")
'';
```

**Implementation note on the `coverage` interpolation:** `scheduling.nix`
receives `common` as an argument; `common.coverage` (or however
`common.nix` exposes the flag — check `common.nix:36` where `coverage`
is a function parameter) needs to be in scope at the fragment
definition site. If it's not already threaded through, the
`${if ... then ... else ...}` becomes a `common.covSnippet "..."`
helper instead. Don't block on this — the FUSE-unmount + ExecMainCode
assertions are the load-bearing ones and need no conditional.

### 5.2 Wire into `default.nix`

```diff
--- a/nix/tests/default.nix
+++ b/nix/tests/default.nix
@@ -192,8 +192,9 @@
       {
         name = "disrupt";
         subtests = [
           "sizeclass"
           "reassign"
+          "sigint-graceful"
         ];
       };
```

`assertChains` in `scheduling.nix:656` needs no update —
`sigint-graceful` touches wsmall2 only, no FUSE-cache dependency on
`fanout`.

### 5.3 Spec marker

The `r[verify worker.shutdown.sigint]` annotation in the fragment
banner needs a matching spec rule. Add to
`docs/src/components/worker.md` (find the existing shutdown section,
or create one next to the drain-sequence text):

```markdown
r[worker.shutdown.sigint]

The worker handles both SIGTERM and SIGINT by breaking the
BuildExecution select loop, running `run_drain()`, and returning from
`main()`. Local development (`cargo run` → Ctrl+C) and Kubernetes pod
deletion (kubelet → SIGTERM) share the same exit path.
```

And on the select arm:

```rust
// r[impl worker.shutdown.sigint]
_ = shutdown.cancelled() => {
    break StreamEnd::Shutdown;
}
```

---

## Validation checklist

- [ ] `nix develop -c cargo clippy --all-targets -- --deny warnings`
- [ ] `nix develop -c cargo nextest run` (no new unit tests — the
      signal path is VM-only; `signal.rs` already has a SIGTERM
      self-raise test and SIGINT would be symmetric)
- [ ] `nix-build-remote --no-nom --dev -- -L .#checks.x86_64-linux.vm-scheduling-disrupt-standalone`
      — green with the new fragment
- [ ] `nix-build-remote --no-nom --dev -- -L .#cov-vm-scheduling-disrupt-standalone`
      — the `profraw_after > profraw_before` assertion fires in
      coverage mode
- [ ] `tracey query rule worker.shutdown.sigint` — spec + impl + verify
      all present
- [ ] `grep -rn 'loop {' rio-*/src/main.rs | grep -v select` — should
      be empty (every main-loop has a cancellation arm)
- [ ] `grep 'process::exit' rio-*/src/main.rs` — should be empty
