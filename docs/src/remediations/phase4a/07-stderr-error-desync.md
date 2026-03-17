# Remediation 07: STDERR_ERROR → STDERR_LAST desync

**Parent:** [phase4a.md §2.1](../phase4a.md#21-gateway-stderr_error-desync-verified-high-currently-latent)
**Severity:** HIGH (P1) — latent protocol corruption, currently masked by CppNix's discard-on-error pooling behavior
**Findings:** gw-stderr-error-then-last-desync, gw-stderr-build-derivation, gw-stderr-build-paths-with-results
**Spec rule touched:** `r[gw.stderr.error-before-return]` (made bidirectional)

---

## Bug summary

Two DAG-validation failure paths in `rio-gateway/src/handler/build.rs` send `STDERR_ERROR` and then **also** send `STDERR_LAST` + a `BuildResult` payload. `STDERR_ERROR` is a terminal frame — confirmed against `libstore/worker-protocol-connection.cc:72`, real `nix-daemon`'s `stopWork()` sends `STDERR_ERROR` **XOR** `STDERR_LAST`, never both. The client stops reading after `STDERR_ERROR` and throws, leaving `STDERR_LAST` + payload unconsumed in the TCP buffer. On the next opcode over a pooled connection, those stale bytes are read as the new opcode's response.

**Self-incriminating:** the comment at `build.rs:160-164` already documents that `STDERR_ERROR` → `STDERR_LAST` is invalid. The code 350 lines down does the inverse, which is equally invalid.

**Why latent:** CppNix today discards the connection on any daemon error. That's a one-line client change away from being false; connection pooling after recoverable errors is a reasonable optimization that any future Nix release might ship.

---

## Layer 1: Handler fix — drop the `stderr.error()` calls

The two sites both already construct a `BuildResult::failure` that carries the rejection reason in `errorMsg`. The `stderr.error()` call is redundant **and** wrong. Delete it; let the client see the failure via the `BuildResult` payload delivered through the normal `STDERR_LAST` + result sequence.

### Site 1 — `handle_build_derivation` (wopBuildDerivation, opcode 36)

**File:** `rio-gateway/src/handler/build.rs:508-523`

```diff
     // Validate BEFORE inlining (no point doing FindMissingPaths +
     // inline for a DAG we're about to reject). __noChroot check +
     // early MAX_DAG_NODES.
     if let Err(reason) = translate::validate_dag(&nodes, drv_cache) {
         warn!(reason = %reason, "rejecting build: DAG validation failed");
-        stderr
-            .error(&rio_nix::protocol::stderr::StderrError::simple(
-                "DAGValidationFailed",
-                format!("build rejected: {reason}"),
-            ))
-            .await?;
-        // BuildResult::failure so the wire protocol gets a clean
-        // STDERR_LAST + result sequence (caller expects one even
-        // on rejection).
+        // Do NOT send STDERR_ERROR here — it is a terminal frame.
+        // The client receives the rejection via BuildResult.errorMsg
+        // after STDERR_LAST. See build.rs:160-164 for the inverse
+        // invariant (STDERR_ERROR → STDERR_LAST is equally invalid).
         let failure = BuildResult::failure(BuildStatus::MiscFailure, reason);
         stderr.finish().await?;
         write_build_result(stderr.inner_mut(), &failure).await?;
         return Ok(());
     }
```

The `warn!` stays — server-side observability of rejected builds is still wanted. The `reason` string flows unchanged into `BuildResult.errorMsg`, so the client sees identical text to what `STDERR_ERROR` would have shown.

### Site 2 — `handle_build_paths_with_results` (wopBuildPathsWithResults, opcode 46)

**File:** `rio-gateway/src/handler/build.rs:789-800`

```diff
-        // Validate BEFORE inlining: __noChroot check + early
-        // MAX_DAG_NODES. On reject: STDERR_ERROR + per-path failure,
-        // no SubmitBuild.
+        // Validate BEFORE inlining: __noChroot check + early
+        // MAX_DAG_NODES. On reject: per-path BuildResult::failure,
+        // no SubmitBuild. No STDERR_ERROR — the failure BuildResult
+        // is delivered via STDERR_LAST + result at :872 below.
         let build_result = if let Err(reason) = translate::validate_dag(&all_nodes, drv_cache) {
             warn!(reason = %reason, "rejecting build: DAG validation failed");
-            stderr
-                .error(&rio_nix::protocol::stderr::StderrError::simple(
-                    "DAGValidationFailed",
-                    format!("build rejected: {reason}"),
-                ))
-                .await?;
             BuildResult::failure(BuildStatus::MiscFailure, reason)
         } else {
```

This site already falls through to the shared `stderr.finish()` + per-path `write_build_result` loop at `:872-879`, so no early-return plumbing changes needed — just delete the six-line `stderr.error()` block.

### Audit: no other violating sites

`grep -n 'stderr\.error' rio-gateway/src/handler/build.rs` returns only these two sites. All other error paths in `build.rs` use the `stderr_err!` macro (`handler/mod.rs:42-53`), which sends `STDERR_ERROR` and then `return Err(anyhow!(...))` — correct, because the session loop sees `Err` and does **not** follow up with `STDERR_LAST`. Confirmed at:

- `:466-470` — inline-BasicDerivation `__noChroot` check uses `stderr_err!` → returns `Err`, no follow-up write. Correct.
- `:660`, `:664` — `handle_build_paths` (opcode 9) failure paths use `stderr_err!`. Correct.

The `// r[impl gw.stderr.error-before-return]` annotation at `:673` stays — site 2 still implements the rule; it just stops violating the new bidirectional half.

---

## Layer 2: StderrWriter API hardening — make the bug unrepresentable

**File:** `rio-nix/src/protocol/stderr.rs`

The root design flaw: `error()` does not mark the writer as terminal, and `finish()` does not check whether the writer is already terminal. This makes `error()` → `finish()` a silent wire-corrupting sequence rather than an immediate failure.

### Design: two-flag state, not one

The existing `finished: bool` tracks "STDERR_LAST was sent; `inner_mut()` is now safe to call." Overloading it to also mean "STDERR_ERROR was sent" conflates two distinct states:

| State | `finished` | `errored` | `log()`/`start_activity()`/… | `finish()` | `inner_mut()` |
|---|---|---|---|---|---|
| Fresh | false | false | ok | ok | **panic** |
| After `finish()` | true | false | Err | Err (double-finish) | ok |
| After `error()` | false | **true** | Err | Err | **panic** |

The caveat in the task description is the key constraint: if `error()` simply set `finished = true`, then `error()` → `inner_mut()` would succeed and let a handler write raw garbage bytes after `STDERR_ERROR`. A separate `errored` flag lets `inner_mut()` continue to assert `finished && !errored` — i.e., you can only get the raw writer after a clean `STDERR_LAST`.

### Diff

```diff
 pub struct StderrWriter<W> {
     writer: W,
     next_activity_id: u64,
     finished: bool,
+    errored: bool,
 }

 impl<W: AsyncWrite + Unpin> StderrWriter<W> {
     pub fn new(writer: W) -> Self {
         StderrWriter {
             writer,
             next_activity_id: 1,
             finished: false,
+            errored: false,
         }
     }

-    /// Check that the writer has not been finished; return an error if it has.
+    /// Check that the writer is still live (neither finished nor errored).
     fn check_not_finished(&self) -> Result<(), WireError> {
-        if self.finished {
+        if self.finished || self.errored {
             return Err(WireError::Io(std::io::Error::other(
-                "StderrWriter already finished",
+                "StderrWriter already terminated (finish() or error() was called)",
             )));
         }
         Ok(())
     }
```

`inner_mut()` at `:151-158` — tighten the assertion to forbid the errored case:

```diff
     /// Get a mutable reference to the inner writer (for writing result data after STDERR_LAST).
     ///
     /// # Panics
     ///
-    /// Panics if called before `finish()`. The STDERR streaming loop must be
-    /// terminated before writing result data to the underlying stream.
+    /// Panics if called before `finish()`, or if `error()` was called.
+    /// `STDERR_ERROR` is terminal — no result payload follows it on the wire,
+    /// so handing out the raw writer after an error would let the handler
+    /// emit bytes that a real Nix client will never consume.
     pub fn inner_mut(&mut self) -> &mut W {
         assert!(
-            self.finished,
+            self.finished && !self.errored,
             "StderrWriter::inner_mut() called before finish() — \
-             this would corrupt the STDERR stream"
+             or after error() — this would corrupt the STDERR stream"
         );
         &mut self.writer
     }
```

`finish()` at `:171-176` — add the guard at the top:

```diff
     /// Send STDERR_LAST to end the streaming loop.
     /// After this, the caller should write the operation result directly.
+    /// Returns `Err` if `error()` or `finish()` was already called —
+    /// `STDERR_ERROR` and `STDERR_LAST` are mutually exclusive terminal frames.
     pub async fn finish(&mut self) -> Result<(), WireError> {
+        self.check_not_finished()?;
         wire::write_u64(&mut self.writer, STDERR_LAST).await?;
         self.writer.flush().await?;
         self.finished = true;
         Ok(())
     }
```

`error()` at `:194-212` — set `errored` at the end:

```diff
     // r[impl gw.stderr.error-format]
     // r[impl gw.stderr.error-before-return]
-    /// Send STDERR_ERROR with the full structured error format.
+    /// Send STDERR_ERROR with the full structured error format.
+    ///
+    /// This is a **terminal** operation. After calling `error()`, the writer
+    /// is poisoned: `finish()`, `log()`, `start_activity()`, etc. all return
+    /// `Err`, and `inner_mut()` panics. The handler MUST `return Err(...)`
+    /// immediately after this call — see `r[gw.stderr.error-before-return]`.
     pub async fn error(&mut self, err: &StderrError) -> Result<(), WireError> {
         self.check_not_finished()?;
         wire::write_u64(&mut self.writer, STDERR_ERROR).await?;
         // ... unchanged body ...
         self.writer.flush().await?;
+        self.errored = true;
         Ok(())
     }
```

**Flag-set-after-flush, not before:** if the write itself fails (broken pipe), the flag is not set. This is intentional — a handler that catches a broken-pipe `WireError` and retries the whole operation on a fresh writer should not be blocked. In practice no handler retries wire writes, but the semantics are cleaner.

### Why `Err` not `panic!`

`check_not_finished()` already returns `WireError::Io` rather than panicking, and every caller in `build.rs` propagates with `?`. Keeping that contract means the Layer-1 bug would have surfaced as a clean handler error (logged, connection closed, metrics incremented) rather than a process abort. Downgrading to a panic now would be a regression in failure mode.

The one exception is `inner_mut()`, which already panics (it returns `&mut W`, not `Result`) — that stays a panic.

---

## Layer 3: Spec fix — make `r[gw.stderr.error-before-return]` bidirectional

**File:** `docs/src/components/gateway.md:522-523`

The current rule is one-directional: it mandates "if returning `Err`, send `STDERR_ERROR` first" but does not forbid "send `STDERR_ERROR`, then return `Ok`." The two buggy sites obey the letter of the current rule (they return `Ok(())`), which is exactly why static review missed them.

```diff
 r[gw.stderr.error-before-return]
-**Every handler error path that returns `Err(...)` MUST send `STDERR_ERROR` to the client first.** Never use bare `?` to propagate errors from store operations, NAR extraction, or ATerm parsing --- always wrap in a match that sends `STDERR_ERROR` before returning. For batch opcodes like `wopBuildPathsWithResults`, per-entry errors should push `BuildResult::failure` and `continue`, not abort the entire batch.
+**`STDERR_ERROR` and `STDERR_LAST` are mutually exclusive terminal frames. A handler sends exactly one of them, exactly once.**
+
+- If a handler returns `Err(...)`, it MUST send `STDERR_ERROR` first, and the session loop MUST NOT follow up with `STDERR_LAST`. Never use bare `?` to propagate errors from store operations, NAR extraction, or ATerm parsing --- always wrap in a match that sends `STDERR_ERROR` before returning.
+- If a handler sends `STDERR_ERROR`, it MUST `return Err(...)` immediately after. It MUST NOT call `stderr.finish()`, and it MUST NOT write a result payload. `STDERR_ERROR` is terminal for the operation --- the client stops reading STDERR frames and throws, so any bytes that follow are stranded in the TCP buffer and corrupt the next opcode on a pooled connection.
+- To report a **recoverable** per-operation failure while keeping the session open for subsequent opcodes, use `BuildResult::failure` (or the opcode's equivalent failure-carrying result type) delivered via `STDERR_LAST` + result. For batch opcodes like `wopBuildPathsWithResults`, per-entry errors push `BuildResult::failure` and `continue` --- they do not abort the batch.
+
+The `StderrWriter` API enforces this: `error()` poisons the writer so that subsequent `finish()` returns `Err` and `inner_mut()` panics.
```

**`tracey bump` required:** this is a meaningful semantic change (the rule gained a new MUST-NOT clause). Run `tracey bump` before committing so existing `// r[impl gw.stderr.error-before-return]` annotations at `handler/mod.rs:42` (stderr_err macro), `build.rs:673`, and `stderr.rs:192` are flagged stale until someone re-reviews them against the bidirectional rule. All three sites are in fact compliant after Layer 1+2, so the re-review is a rubber-stamp — but the tracey workflow requires it.

---

## Layer 4: Unit + wire-level tests

### 4a. StderrWriter state-machine tests

**File:** `rio-nix/src/protocol/stderr.rs` (add to the existing `#[cfg(test)] mod tests` block after `test_stderr_error_simple` at `:382`)

```rust
// r[verify gw.stderr.error-before-return]
/// error() is terminal: a subsequent finish() must be rejected
/// (STDERR_ERROR and STDERR_LAST are mutually exclusive).
#[tokio::test]
async fn test_error_then_finish_rejected() {
    let mut buf = Vec::new();
    let mut writer = StderrWriter::new(&mut buf);
    writer
        .error(&StderrError::simple("rio-build", "boom"))
        .await
        .unwrap();

    let result = writer.finish().await;
    assert!(result.is_err(), "finish() after error() must return Err");

    // Byte-level: STDERR_ERROR frame, NOT followed by STDERR_LAST.
    let mut reader = Cursor::new(&buf);
    assert_eq!(wire::read_u64(&mut reader).await.unwrap(), STDERR_ERROR);
    // Consume the error body (type, level, name, message, havePos, traceCount).
    let _ = wire::read_string(&mut reader).await.unwrap(); // type
    let _ = wire::read_u64(&mut reader).await.unwrap();    // level
    let _ = wire::read_string(&mut reader).await.unwrap(); // name
    let _ = wire::read_string(&mut reader).await.unwrap(); // message
    assert_eq!(wire::read_u64(&mut reader).await.unwrap(), 0); // havePos
    assert_eq!(wire::read_u64(&mut reader).await.unwrap(), 0); // traceCount
    // The cursor is now at EOF. No STDERR_LAST bytes follow.
    assert_eq!(
        reader.position() as usize,
        buf.len(),
        "no bytes after STDERR_ERROR frame"
    );
}

/// error() is terminal: subsequent log() is rejected.
#[tokio::test]
async fn test_error_then_log_rejected() {
    let mut buf = Vec::new();
    let mut writer = StderrWriter::new(&mut buf);
    writer
        .error(&StderrError::simple("rio-build", "boom"))
        .await
        .unwrap();
    assert!(writer.log("this should not be sent").await.is_err());
}

/// error() is terminal: inner_mut() panics (no result payload after STDERR_ERROR).
#[tokio::test]
#[should_panic(expected = "after error()")]
async fn test_error_then_inner_mut_panics() {
    let mut buf = Vec::new();
    let mut writer = StderrWriter::new(&mut buf);
    writer
        .error(&StderrError::simple("rio-build", "boom"))
        .await
        .unwrap();
    let _ = writer.inner_mut(); // ← panic here
}

/// finish() is idempotent-rejecting: double finish() is an error.
#[tokio::test]
async fn test_double_finish_rejected() {
    let mut buf = Vec::new();
    let mut writer = StderrWriter::new(&mut buf);
    writer.finish().await.unwrap();
    assert!(writer.finish().await.is_err());
    // Exactly one STDERR_LAST in the buffer (8 bytes).
    assert_eq!(buf.len(), 8);
}
```

### 4b. Wire-level regression test — DAG-validation failure produces clean STDERR_LAST

**File:** `rio-gateway/tests/wire_opcodes/build.rs` (add after `test_build_derivation_basic_format` at `:285`)

The trigger: `validate_dag` rejects on `__noChroot=1` in a cached derivation's env. The existing `TEST_DRV_ATERM` has no env `__noChroot`, so we need a poisoned variant. Crucially, site 1 at `build.rs:508` runs `validate_dag(&nodes, drv_cache)` — it checks the **cache entry**, not the wire `BasicDerivation` — so seed the store with a `__noChroot` ATerm and the wire-sent env can be clean.

```rust
/// ATerm derivation with __noChroot=1 in env. Triggers validate_dag rejection.
const NOCHROOT_DRV_ATERM: &str = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("__noChroot","1"),("out","/nix/store/zzz-output")])"#;

// r[verify gw.stderr.error-before-return]
/// wopBuildDerivation: DAG-validation failure (cached drv has __noChroot)
/// sends STDERR_LAST + failure BuildResult, NOT STDERR_ERROR.
/// Regression for phase4a §2.1: prior code sent STDERR_ERROR then STDERR_LAST,
/// leaving stale bytes in the buffer.
#[tokio::test]
async fn test_build_derivation_dag_reject_clean_stderr_last() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    let drv_path = "/nix/store/00000000000000000000000000000001-nochroot.drv";
    // Seed the POISONED aterm — validate_dag reads from drv_cache, which
    // is populated from the store. The inline BasicDerivation sent on the
    // wire below is clean (no __noChroot) so the :459 inline check passes
    // and we reach the :508 validate_dag path.
    h.store
        .seed_with_content(drv_path, NOCHROOT_DRV_ATERM.as_bytes());

    wire_send!(&mut h.stream;
        u64: 36,                                 // wopBuildDerivation
        string: drv_path,
        u64: 1,                                  // 1 output
        string: "out",
        string: "/nix/store/zzz-output",
        string: "",
        string: "",
        strings: wire::NO_STRINGS,               // input_srcs
        string: "x86_64-linux",
        string: "/bin/sh",
        strings: &["-c", "echo hi"],
        u64: 1,                                  // 1 env pair (NOT __noChroot)
        string: "out",
        string: "/nix/store/zzz-output",
        u64: 0,                                  // build_mode
    );

    // KEY ASSERTION: drain_stderr_until_last PANICS if it sees STDERR_ERROR.
    // Before this fix, the handler sent STDERR_ERROR first → this line would
    // panic with "unexpected STDERR_ERROR". After the fix, we reach STDERR_LAST
    // cleanly.
    drain_stderr_until_last(&mut h.stream).await?;

    // BuildResult follows. Status should be a failure code, errorMsg populated.
    let status = wire::read_u64(&mut h.stream).await?;
    assert_ne!(status, 0, "status must be a failure code (not Built=0)");
    let error_msg = wire::read_string(&mut h.stream).await?;
    assert!(
        error_msg.contains("noChroot") || error_msg.contains("sandbox"),
        "errorMsg should mention the rejection reason, got: {error_msg:?}"
    );
    // Consume the rest of BuildResult (timesBuilt..builtOutputs).
    let _ = wire::read_u64(&mut h.stream).await?; // timesBuilt
    let _ = wire::read_bool(&mut h.stream).await?; // isNonDet
    let _ = wire::read_u64(&mut h.stream).await?; // start
    let _ = wire::read_u64(&mut h.stream).await?; // stop
    let tag = wire::read_u64(&mut h.stream).await?; // cpuUser tag
    if tag == 1 { let _ = wire::read_u64(&mut h.stream).await?; }
    let tag = wire::read_u64(&mut h.stream).await?; // cpuSystem tag
    if tag == 1 { let _ = wire::read_u64(&mut h.stream).await?; }
    let outs = wire::read_u64(&mut h.stream).await?; // builtOutputs count
    assert_eq!(outs, 0, "failure BuildResult has no built outputs");

    // NO-STALE-BYTES ASSERTION: send a second opcode on the same stream.
    // If the handler had written extra bytes after STDERR_ERROR (the bug),
    // they would be sitting in the buffer right now, and this opcode's
    // response would be garbage. wopSetOptions is the simplest "ping":
    // it reads a fixed payload and responds with just STDERR_LAST.
    wire_send!(&mut h.stream;
        u64: 19,                                 // wopSetOptions
        u64: 0, u64: 0, u64: 0, u64: 0,          // keepFailed..maxBuildJobs
        u64: 0, u64: 0, u64: 0, u64: 0,          // maxSilentTime..buildCores
        u64: 0,                                  // useSubstitutes
        u64: 0,                                  // overrides count
    );
    // If stale bytes were present, read_u64 inside drain would not see
    // STDERR_LAST (0x616c7473) — it would see whatever garbage was left.
    drain_stderr_until_last(&mut h.stream).await?;

    // Scheduler must NOT have been called — rejection is pre-submit.
    assert_eq!(
        h.scheduler.submit_calls.read().unwrap().len(),
        0,
        "DAG rejection happens BEFORE SubmitBuild"
    );

    h.finish().await;
    Ok(())
}
```

**Why `drain_stderr_until_last` is the primary assertion:** `rio-test-support/src/wire.rs:56-67` — this helper already `panic!("unexpected STDERR_ERROR: ...")` when it encounters `STDERR_ERROR`. No new test-support helper needed; the existing one is exactly the right oracle. Before the fix this test panics at the drain; after the fix it passes.

**Why the second-opcode probe matters:** `drain_stderr_until_last` proves `STDERR_ERROR` wasn't sent, but it does not prove the stream is aligned — the handler could theoretically send `STDERR_LAST` twice, or write `STDERR_LAST` + BuildResult + extra garbage. The `wopSetOptions` follow-up reads the next 8 bytes expecting `STDERR_LAST` and fails loudly on any misalignment.

### 4c. Sibling test for wopBuildPathsWithResults (site 2)

Add an analogous test hitting `build.rs:792`. The structure is the same — seed a `__noChroot` drv, send opcode 46 with that drv's path, assert `drain_stderr_until_last` succeeds, read the per-path results block (`u64 count` + `count × (string path + BuildResult)`), assert failure status, follow up with `wopSetOptions`. Omitted here for brevity; the diff is mechanical given 4b.

---

## Layer 5: VM test — prove no desync on a real `nix` client

**File:** `nix/tests/scenarios/security.nix:380-395`

The existing subtest only asserts that `nix-build` **fails** with a `__noChroot`-related error string. It does not exercise the session after the failure, so it cannot detect the stale-bytes desync — this is the "passes blindly" observation from the audit.

### The complication: `nix-build` closes the connection

CppNix's `nix-build` (and `nix build`) opens an `ssh-ng://` connection, runs the build, and exits. It does not reuse the session for a second opcode after a failure. We cannot prove "no desync" using the stock CLI alone — the connection is discarded before any desync could manifest.

### Approach: low-level wire client in Python

The NixOS test driver already runs arbitrary Python. Add a helper that opens a raw SSH subsystem connection to the gateway, runs the Nix worker handshake, sends `wopBuildDerivation` with a `__noChroot` derivation, drains the STDERR loop, and then sends `wopIsValidPath` (a cheap read-only opcode) on the **same** connection. If the response to `wopIsValidPath` is well-formed, the session is aligned.

**Add to `security.nix` let-block** (after `noChrootDrv` at `:57`):

```nix
  # Raw wire client: proves no TCP-buffer desync after DAG rejection
  # by sending a second opcode on the SAME ssh-ng session. The stock
  # nix CLI closes the connection on error, so it can't detect this.
  wireProbe = pkgs.writeText "wire-probe.py" ''
    import struct, subprocess, sys

    def w_u64(f, n): f.write(struct.pack("<Q", n))
    def r_u64(f): return struct.unpack("<Q", f.read(8))[0]
    def w_str(f, s):
        b = s.encode()
        w_u64(f, len(b)); f.write(b); f.write(b"\x00" * ((8 - len(b) % 8) % 8))
    def r_str(f):
        n = r_u64(f); s = f.read(n); f.read((8 - n % 8) % 8); return s.decode()

    STDERR_LAST  = 0x616c7473
    STDERR_ERROR = 0x63787470
    STDERR_NEXT  = 0x6f6c6d67

    # ssh subsystem → rio-gateway stdio. -T: no pty. -o Batch: no prompts.
    p = subprocess.Popen(
        ["ssh", "-T", "-o", "BatchMode=yes", sys.argv[1], "nix-daemon", "--stdio"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0,
    )
    i, o = p.stdin, p.stdout

    # Handshake: client magic → server magic + proto version → client proto.
    w_u64(i, 0x6e697863); i.flush()               # WORKER_MAGIC_1
    assert r_u64(o) == 0x6478696f                 # WORKER_MAGIC_2
    server_ver = r_u64(o)
    w_u64(i, server_ver)                           # echo proto version
    # post-handshake: obsolete cpu affinity + reserve space (proto >= 1.33)
    w_u64(i, 0); w_u64(i, 0); i.flush()
    # server sends STDERR_LAST to end handshake stderr loop
    while r_u64(o) != STDERR_LAST: pass

    # --- OP 1: wopBuildDerivation with __noChroot=1 inline ---
    # This hits the inline check at build.rs:459 (not validate_dag), but
    # that path uses stderr_err! → STDERR_ERROR → return Err, which the
    # session loop turns into connection close. We need the OTHER path.
    #
    # To hit build.rs:508 (validate_dag on cached drv), the __noChroot
    # drv must be in the gateway's drv_cache — i.e. uploaded via
    # wopAddToStoreNar first. That's substantially more wire plumbing
    # (NAR serialization in Python). Pragmatic simplification:
    #
    # wopBuildPathsWithResults (opcode 46) with the noChrootDrv path.
    # The drv is already in the store (nix-instantiate below puts it
    # there), so drv_cache resolves it, validate_dag fires at :792.
    drv_path = sys.argv[2]
    w_u64(i, 46)                                   # wopBuildPathsWithResults
    w_u64(i, 1); w_str(i, f"{drv_path}!out")       # 1 derived path
    w_u64(i, 0); i.flush()                         # build_mode = Normal

    # Drain STDERR. Must reach STDERR_LAST — seeing STDERR_ERROR here
    # means the bug is unfixed.
    saw_error = False
    while True:
        t = r_u64(o)
        if t == STDERR_LAST: break
        if t == STDERR_ERROR:
            saw_error = True
            # consume error body: type, level, name, msg, havePos, traceCount
            r_str(o); r_u64(o); r_str(o); r_str(o)
            if r_u64(o): r_str(o); r_u64(o); r_u64(o)  # pos
            for _ in range(r_u64(o)):
                if r_u64(o): r_str(o); r_u64(o); r_u64(o)
                r_str(o)
            break
        if t == STDERR_NEXT: r_str(o)              # skip log lines
        # START_ACTIVITY/STOP_ACTIVITY/RESULT have payloads we'd need to
        # skip — but validate_dag rejection fires BEFORE any activity
        # starts, so we only expect NEXT or LAST here.

    assert not saw_error, "REGRESSION: STDERR_ERROR sent on DAG rejection"

    # Read per-path results: count + (path + BuildResult) per entry.
    count = r_u64(o)
    assert count == 1
    r_str(o)                                       # echo path
    status = r_u64(o)
    assert status != 0, f"expected failure status, got {status}"
    err_msg = r_str(o)
    assert "noChroot" in err_msg or "sandbox" in err_msg, err_msg
    # Consume rest of BuildResult: timesBuilt, isNonDet, start, stop,
    # cpuUser(tag[+val]), cpuSystem(tag[+val]), builtOutputs count+entries.
    r_u64(o); r_u64(o); r_u64(o); r_u64(o)
    if r_u64(o): r_u64(o)
    if r_u64(o): r_u64(o)
    for _ in range(r_u64(o)): r_str(o); r_str(o)

    # --- OP 2: wopIsValidPath on the SAME connection ---
    # If stale bytes from a STDERR_ERROR→STDERR_LAST sequence were in
    # the buffer, the first r_u64 below would NOT be STDERR_LAST.
    w_u64(i, 1)                                    # wopIsValidPath
    w_str(i, "/nix/store/00000000000000000000000000000000-nonexistent")
    i.flush()
    t = r_u64(o)
    assert t == STDERR_LAST, f"desync: expected STDERR_LAST, got {t:#x}"
    valid = r_u64(o)
    assert valid == 0, f"nonexistent path reported valid: {valid}"

    print("WIRE-PROBE OK: no desync, second opcode well-formed")
    p.stdin.close(); p.wait(timeout=5)
  '';
```

**Replace the existing subtest at `:380-395`** with:

```nix
    with subtest("gateway-validate: __noChroot rejected, no wire desync"):
        # First: confirm the stock CLI still sees a rejection (sanity).
        result = client.fail(
            "nix-build --no-out-link "
            f"--store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${noChrootDrv} 2>&1"
        )
        assert ("sandbox escape" in result or "noChroot" in result), (
            f"expected __noChroot rejection, got: {result[:500]}"
        )

        # Second: instantiate the drv so it's in the gateway's store
        # (wire probe needs validate_dag to find it in drv_cache).
        drv_path = client.succeed(
            "nix-instantiate "
            f"--store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${noChrootDrv}"
        ).strip()

        # Third: raw-wire probe — send the rejected opcode, then a
        # second opcode on the SAME ssh session. This is what the
        # stock CLI cannot test (it closes on error).
        client.succeed(
            f"python3 ${wireProbe} ${gatewayHost} '{drv_path}' 2>&1"
        )
        print("gateway-validate PASS: __noChroot rejected, session aligned")
```

### Caveats on the wire probe

- **Handshake details may drift.** The post-handshake `w_u64(i, 0); w_u64(i, 0)` sequence (obsolete cpu-affinity + reserve-space fields) is protocol-version-dependent. If rio-gateway bumps its advertised protocol version, this script needs adjustment. Verify against `rio-nix/src/protocol/handshake.rs` at implementation time.
- **`nix-instantiate --store ssh-ng://` may or may not populate `drv_cache`.** If it uploads via `wopAddToStoreNar`, the gateway's `drv_cache` side-effect populates and `validate_dag` fires. If it uses a different opcode path, `drv_cache` stays cold and the test probes the wrong code path. **Verify at implementation time** by adding a `debug!` in `validate_dag` and checking the VM journal. If cold, fall back to uploading the `.drv` via `wopAddToStoreNar` from within the Python script (NAR-serialize the ATerm — straightforward but ~30 more lines).
- **Fallback if the Python probe is too fragile:** extend `rio-test-support` with a Rust binary that does the same two-opcode sequence over a real TCP connection. Build it into the VM image via `fixture.extraPackages`. More work, but type-checked and reuses the existing `rio-nix::protocol` codecs.

---

## Sequencing

| Step | Change | Verifies previous step |
|---|---|---|
| 1 | Layer 2 (StderrWriter hardening) + Layer 4a tests | — |
| 2 | `cargo nextest run -p rio-nix` — new tests green | Layer 2 correct |
| 3 | `cargo nextest run -p rio-gateway` — **expect 2 test failures** at the two buggy sites (`finish()` now returns `Err` after `error()`) | Layer 2 catches the bug at the type level |
| 4 | Layer 1 (delete `stderr.error()` calls) | — |
| 5 | `cargo nextest run -p rio-gateway` — green again | Layer 1 correct |
| 6 | Layer 4b+4c wire tests | Layer 1 behavior correct at byte level |
| 7 | Layer 3 spec + `tracey bump` + `tracey query validate` | — |
| 8 | Layer 5 VM test + `nix-build-remote -- .#checks.x86_64-linux.vm-security` | End-to-end |

**Step 3 is the proof that Layer 2 works.** If step 3 does NOT fail, Layer 2's guard is not catching the existing bug — the two sites at `:510` and `:794` both call `stderr.finish()` after `stderr.error()`, so a correct `check_not_finished()` guard in `finish()` must reject them. Do not skip this step.

## Tracey annotations to add

- `// r[verify gw.stderr.error-before-return]` on `test_error_then_finish_rejected` (stderr.rs)
- `// r[verify gw.stderr.error-before-return]` on `test_build_derivation_dag_reject_clean_stderr_last` (wire_opcodes/build.rs)
- Move `# r[verify gw.stderr.error-before-return]` to col-0 comments before the `{` in `security.nix` (not inside the testScript string literal — tracey's `.nix` parser only sees col-0 comments before the attrset open)

After `tracey bump`, re-check `tracey query rule gw.stderr.error-before-return` shows the bumped spec + 3 `impl` sites + ≥2 `verify` sites.
