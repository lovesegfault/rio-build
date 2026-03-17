# Remediation 06: Realisation outPath — BOTH directions

**Parent:** [§1.6 of phase4a.md](../phase4a.md#16-high-was-critical-realisation-outpath-both-directions-broken)
**Severity:** HIGH (downgraded from CRITICAL — `realisations` table empty in prod, CA gated behind `--experimental-features`)
**Findings:** `wire-realisation-outpath-full-path`, `wire-realisation-register-basename-rejected`, `test-mock-store-no-validate-store-path`, `test-vm-protocol-wrong-opcode`
**Commit shape:** ONE atomic commit. Both directions + mock hardening + test updates land together, or the intermediate tree is unshippable (register fix alone makes `ca_roundtrip.rs` fail at the query assertion; query fix alone makes it fail at the register assertion once the mock validates).

---

## Bug recap

Nix's `Realisation` JSON serializes `outPath` as a **basename** (`"abc...-foo"`), not a full store path — `StorePath::to_string()` in CppNix omits the prefix, and `Realisation::fromJSON` parses via `StorePath::parse` which rejects `/` with `"illegal base-32 char '/'"`.

rio's internal representation (gRPC `types::Realisation.output_path`, PG `realisations.output_path`) uses **full paths** (`"/nix/store/abc...-foo"`). The gateway is the translation boundary and currently translates in **neither** direction:

| Direction | Opcode | File:line | Current | Correct |
|---|---|---|---|---|
| client → store | 42 `wopRegisterDrvOutput` | `opcodes_read.rs:486` | passes basename verbatim → store's `validate_store_path` rejects → `STDERR_ERROR` | prepend `/nix/store/` before gRPC call |
| store → client | 43 `wopQueryRealisation` | `opcodes_read.rs:558` | passes full path verbatim → client's `StorePath::parse` rejects | strip `/nix/store/` before `json!` |

The **correct** pattern already exists at `rio-nix/src/protocol/build.rs:350-351` (read) and `:415-418` (write), with comments explaining exactly this. Commit `5786f82` fixed `write_build_result` but never touched `opcodes_read.rs` — same field name, different struct.

**Why tests didn't catch it:** three compounding fidelity failures. (1) `ca_roundtrip.rs` and `wire_opcodes/` fixtures send full paths and assert full paths — the two bugs cancel. (2) `MockStore::register_realisation` has no `validate_store_path` — the real store's `mod.rs:464` check is never exercised. (3) `nix/tests/scenarios/protocol.nix` subtest labeled `"Realisation outPath basename"` exercises opcode **47** (`BuildResult.builtOutputs`), not opcode 43.

---

## Step 1 — Register fix (client → store, prepend prefix)

**File:** `rio-gateway/src/handler/opcodes_read.rs`
**Lines:** 447-486
**Reference pattern:** `rio-nix/src/protocol/build.rs:350-351`

### Current code

```rust
// opcodes_read.rs:447-450
let out_path = parsed
    .get("outPath")
    .and_then(|v| v.as_str())
    .unwrap_or_default();
// ...
// opcodes_read.rs:482-490
let req = types::RegisterRealisationRequest {
    realisation: Some(types::Realisation {
        drv_hash: drv_hash.to_vec(),
        output_name,
        output_path: out_path.to_string(),  // ← basename passed through
        output_hash: output_hash.to_vec(),
        signatures,
    }),
};
```

### Diff

```diff
--- a/rio-gateway/src/handler/opcodes_read.rs
+++ b/rio-gateway/src/handler/opcodes_read.rs
@@ -447,10 +447,22 @@
-    let out_path = parsed
+    // Wire outPath is a BASENAME ("<hashpart>-<name>") per CppNix
+    // StorePath::to_string(). Our gRPC/PG repr uses full paths. Prepend
+    // the prefix here — the gateway is the translation boundary. Idempotent
+    // guard: if a (buggy or future) client sends the full path, don't
+    // double-prepend. Reference: rio-nix/src/protocol/build.rs:350-351
+    // (same transform for BuildResult.builtOutputs read path).
+    let out_path_raw = parsed
         .get("outPath")
         .and_then(|v| v.as_str())
         .unwrap_or_default();
+    let out_path = if out_path_raw.starts_with(rio_nix::store_path::STORE_PREFIX) {
+        out_path_raw.to_string()
+    } else {
+        format!("{}{out_path_raw}", rio_nix::store_path::STORE_PREFIX)
+    };

     let Some((drv_hash, output_name)) = parse_drv_output_id(id) else {
@@ -483,7 +495,7 @@
     let req = types::RegisterRealisationRequest {
         realisation: Some(types::Realisation {
             drv_hash: drv_hash.to_vec(),
             output_name,
-            output_path: out_path.to_string(),
+            output_path: out_path,
             output_hash: output_hash.to_vec(),
             signatures,
         }),
     };
```

**Notes:**
- No new import needed — `rio_nix` is already in scope (see `StorePath::parse` usage at line 16). Use the fully-qualified `rio_nix::store_path::STORE_PREFIX` to avoid touching the `use` block; or add `use rio_nix::store_path::STORE_PREFIX;` next to existing rio_nix imports — implementer's choice.
- `STORE_PREFIX` is `"/nix/store/"` (trailing slash included) — `format!("{}{out_path_raw}", STORE_PREFIX)` yields `/nix/store/abc...-foo`.
- `.unwrap_or_default()` on a missing `outPath` now produces `/nix/store/` (11 chars, no hash part) instead of `""`. Both fail `validate_store_path` at the real store; with Step 3 applied, both fail at `MockStore` too. This is fine — missing `outPath` is malformed input.

---

## Step 2 — Query fix (store → client, strip prefix)

**File:** `rio-gateway/src/handler/opcodes_read.rs`
**Line:** 558
**Reference pattern:** `rio-nix/src/protocol/build.rs:415-418`

### Current code

```rust
// opcodes_read.rs:556-561
let json = serde_json::json!({
    "id": id,
    "outPath": r.output_path,  // ← full path passed through
    "signatures": r.signatures,
    "dependentRealisations": {}
});
```

### Diff

```diff
--- a/rio-gateway/src/handler/opcodes_read.rs
+++ b/rio-gateway/src/handler/opcodes_read.rs
@@ -548,6 +548,15 @@
         Ok(resp) => {
             let r = resp.into_inner();
+            // Wire outPath is a BASENAME — CppNix Realisation::fromJSON
+            // feeds it to StorePath::parse, which rejects '/' with
+            // "illegal base-32 char '/'". Strip the prefix. Reference:
+            // rio-nix/src/protocol/build.rs:415-418 (same transform for
+            // BuildResult.builtOutputs write path). unwrap_or defensive:
+            // if the store somehow returns a basename, pass it through.
+            let out_path_basename = r
+                .output_path
+                .strip_prefix(rio_nix::store_path::STORE_PREFIX)
+                .unwrap_or(&r.output_path);
             // Reconstruct the Nix Realisation JSON. id is the same string
             // we got; outPath from the store; signatures from the store;
             // dependentRealisations is always empty (phase 5 populates it).
@@ -556,7 +565,7 @@
             let json = serde_json::json!({
                 "id": id,
-                "outPath": r.output_path,
+                "outPath": out_path_basename,
                 "signatures": r.signatures,
                 "dependentRealisations": {}
             });
```

---

## Step 3 — MockStore hardening (validate_store_path)

**File:** `rio-test-support/src/grpc.rs`
**Lines:** 304-316
**Rationale:** Mocks must validate inputs the same way the real service does, or they mask input-shape bugs. The real store at `rio-store/src/grpc/mod.rs:464` calls `validate_store_path(&proto.output_path)?` → `StorePath::parse`. With this check added, the pre-fix gateway (passing basename through) fails here — the test becomes load-bearing.

### Current code

```rust
// rio-test-support/src/grpc.rs:304-316
async fn register_realisation(
    &self,
    request: Request<types::RegisterRealisationRequest>,
) -> Result<Response<types::RegisterRealisationResponse>, Status> {
    let r = request
        .into_inner()
        .realisation
        .ok_or_else(|| Status::invalid_argument("realisation required"))?;
    // Key by (drv_hash, output_name) — mirrors the real store's PK.
    let key = (r.drv_hash.clone(), r.output_name.clone());
    self.realisations.write().unwrap().insert(key, r);
    Ok(Response::new(types::RegisterRealisationResponse {}))
}
```

### Diff

```diff
--- a/rio-test-support/src/grpc.rs
+++ b/rio-test-support/src/grpc.rs
@@ -308,6 +308,16 @@
         let r = request
             .into_inner()
             .realisation
             .ok_or_else(|| Status::invalid_argument("realisation required"))?;
+        // Mirror the real store's validation at rio-store/src/grpc/mod.rs:464.
+        // Without this, the mock accepts basenames that the real store rejects,
+        // masking wire-format bugs (see phase4a §1.6: gateway passed basename
+        // verbatim, real store returned invalid_argument, mock swallowed it).
+        // Inline check rather than dep on rio-nix: rio-test-support is a
+        // leaf test crate; adding a rio-nix dep for one `StorePath::parse`
+        // call is not worth the build-graph coupling. The prefix+length check
+        // is sufficient to catch basename-vs-full-path (/nix/store/ + 32-char
+        // hash + '-' = 44 chars minimum).
+        if !r.output_path.starts_with("/nix/store/") || r.output_path.len() < 44 {
+            return Err(Status::invalid_argument(format!(
+                "invalid store path {:?}: must start with /nix/store/ and have a 32-char hash part",
+                r.output_path
+            )));
+        }
         // Key by (drv_hash, output_name) — mirrors the real store's PK.
         let key = (r.drv_hash.clone(), r.output_name.clone());
```

**Alternative (stronger, if the dep is acceptable):** add `rio-nix = { path = "../rio-nix" }` to `rio-test-support/Cargo.toml` `[dependencies]` and use the real parser:

```rust
rio_nix::store_path::StorePath::parse(&r.output_path)
    .map_err(|e| Status::invalid_argument(format!("invalid store path {:?}: {e}", r.output_path)))?;
```

Implementer's choice. The inline prefix check is sufficient for the bug class at hand (basename vs full path); the real parser also catches bad nixbase32, oversized names, etc. Recommend the inline check — keep `rio-test-support` dep-free.

---

## Step 4 — Test updates (wire-level assertions)

All three test files currently use full paths on the wire side. Flip each to the real-client shape: **basename on the wire, full path in `MockStore`**.

### 4a. `rio-gateway/tests/ca_roundtrip.rs`

| Line | Current | New | Why |
|---|---|---|---|
| 43 | `let output_path = "/nix/store/caaa...-ca-output";` | unchanged — this is the **store-side** repr, used for `sess.store.seed()` | canonical internal form |
| new | — | `let output_basename = "caaa...-ca-output";` | wire-side repr |
| 63 | `"outPath": output_path,` | `"outPath": output_basename,` | real clients send basename |
| 84 | `assert_eq!(stored.output_path, output_path);` | unchanged | **this is the assertion that proves Step 1 works** — sent basename, MockStore got full path |
| 108 | `parsed["outPath"], output_path,` | `parsed["outPath"], output_basename,` | **this is the assertion that proves Step 2 works** — store has full path, wire got basename |

### Diff

```diff
--- a/rio-gateway/tests/ca_roundtrip.rs
+++ b/rio-gateway/tests/ca_roundtrip.rs
@@ -40,7 +40,12 @@
     // This is what a worker does after a successful CA build. The
     // nar_hash is the content identity — same bytes always give the
     // same hash, regardless of the input-addressed store path.
+    // output_path: internal (gRPC/PG) repr — full /nix/store/ path.
+    // output_basename: wire repr — what real nix clients send in
+    //   Realisation JSON (CppNix StorePath::to_string() omits prefix).
+    // The gateway translates between them; these assertions prove it.
     let output_path = "/nix/store/caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca-output";
+    let output_basename = "caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca-output";
     let (nar, nar_hash) = make_nar(b"deterministic CA output bytes");
     sess.store
         .seed(make_path_info(output_path, &nar, nar_hash), nar);
@@ -60,7 +65,7 @@
     let drv_output_id = format!("sha256:{drv_hash_hex}!out");
     let realisation_json = serde_json::json!({
         "id": drv_output_id,
-        "outPath": output_path,
+        "outPath": output_basename,  // wire format: basename, NOT /nix/store/...
         "signatures": ["test-key:fake-sig-base64"],
         "dependentRealisations": {}
     })
@@ -81,7 +86,9 @@
         let stored = realisations
             .get(&(drv_hash_bytes.clone(), "out".into()))
             .expect("realisation should be stored via gRPC");
-        assert_eq!(stored.output_path, output_path);
+        // Sent basename on the wire; gateway prepended /nix/store/ before
+        // the gRPC call. This is the Step-1 (register-direction) assertion.
+        assert_eq!(stored.output_path, output_path, "gateway should prepend STORE_PREFIX");
         assert_eq!(stored.signatures, vec!["test-key:fake-sig-base64"]);
     }
@@ -105,8 +112,10 @@
     // Roundtrip: what we registered is what we get back.
     assert_eq!(parsed["id"], drv_output_id, "id echoes what we sent");
+    // Store has the full path; gateway stripped /nix/store/ before the
+    // json! serialize. This is the Step-2 (query-direction) assertion.
     assert_eq!(
-        parsed["outPath"], output_path,
-        "outPath roundtrips (THIS is the cache-hit payload)"
+        parsed["outPath"], output_basename,
+        "gateway should strip STORE_PREFIX for wire (THIS is the cache-hit payload)"
     );
```

### 4b. `rio-gateway/tests/wire_opcodes/opcodes_write.rs` — `test_register_drv_output_stores_realisation`

| Line | Current | New |
|---|---|---|
| 379 | `let out_path = "/nix/store/000...-ca-test-out";` | `let out_path_full = "/nix/store/000...-ca-test-out";`<br>`let out_path_wire = "000...-ca-test-out";` |
| 381 | `..."outPath":"{out_path}"...` | `..."outPath":"{out_path_wire}"...` |
| 397 | `assert_eq!(stored.output_path, out_path);` | `assert_eq!(stored.output_path, out_path_full, "gateway prepended prefix");` |

### Diff

```diff
--- a/rio-gateway/tests/wire_opcodes/opcodes_write.rs
+++ b/rio-gateway/tests/wire_opcodes/opcodes_write.rs
@@ -376,9 +376,13 @@
     let mut h = GatewaySession::new_with_handshake().await?;

     // 64-char hex = 32-byte SHA-256. All-AA for test determinism.
     let drv_hash_hex = "aa".repeat(32);
-    let out_path = "/nix/store/00000000000000000000000000000000-ca-test-out";
+    // Wire repr (basename) vs internal repr (full path). Real nix clients
+    // send the basename — CppNix StorePath::to_string() omits /nix/store/.
+    // Gateway prepends it before the gRPC call.
+    let out_path_wire = "00000000000000000000000000000000-ca-test-out";
+    let out_path_full = "/nix/store/00000000000000000000000000000000-ca-test-out";
     let realisation_json = format!(
-        r#"{{"id":"sha256:{drv_hash_hex}!out","outPath":"{out_path}","signatures":["sig:test"],"dependentRealisations":{{}}}}"#
+        r#"{{"id":"sha256:{drv_hash_hex}!out","outPath":"{out_path_wire}","signatures":["sig:test"],"dependentRealisations":{{}}}}"#
     );
@@ -394,7 +398,8 @@
     let key = (drv_hash, "out".to_string());
     let stored = h.store.realisations.read().unwrap().get(&key).cloned();
     let stored = stored.expect("MockStore should have the realisation");
-    assert_eq!(stored.output_path, out_path);
+    // Sent basename, MockStore got full path — gateway's prepend is working.
+    assert_eq!(stored.output_path, out_path_full, "gateway should prepend STORE_PREFIX");
     assert_eq!(stored.signatures, vec!["sig:test"]);
```

### 4c. `rio-gateway/tests/wire_opcodes/opcodes_read.rs` — `test_query_realisation_hit_returns_json`

| Line | Current | New |
|---|---|---|
| 326 | `let out_path = "/nix/store/111...-ca-hit";` | `let out_path_store = "/nix/store/111...-ca-hit";`<br>`let out_path_wire = "111...-ca-hit";` |
| 334 | `output_path: out_path.into(),` | `output_path: out_path_store.into(),` (unchanged semantically — seeding MockStore with full path) |
| 357 | `assert_eq!(parsed["outPath"], out_path);` | `assert_eq!(parsed["outPath"], out_path_wire, "gateway stripped prefix");` |

### Diff

```diff
--- a/rio-gateway/tests/wire_opcodes/opcodes_read.rs
+++ b/rio-gateway/tests/wire_opcodes/opcodes_read.rs
@@ -323,7 +323,11 @@
     let mut h = GatewaySession::new_with_handshake().await?;

     let drv_hash_hex = "cc".repeat(32);
     let drv_hash = hex::decode(&drv_hash_hex)?;
-    let out_path = "/nix/store/11111111111111111111111111111111-ca-hit";
+    // Store-side repr (full path, how we seed MockStore) vs wire repr
+    // (basename, what the gateway serializes). Real nix clients parse
+    // outPath via StorePath::parse — full path fails with "illegal
+    // base-32 char '/'".
+    let out_path_store = "/nix/store/11111111111111111111111111111111-ca-hit";
+    let out_path_wire = "11111111111111111111111111111111-ca-hit";

     // Seed MockStore's realisations map directly.
     h.store.realisations.write().unwrap().insert(
         (drv_hash.clone(), "out".into()),
         rio_proto::types::Realisation {
             drv_hash,
             output_name: "out".into(),
-            output_path: out_path.into(),
+            output_path: out_path_store.into(),
             output_hash: vec![0xDDu8; 32],
             signatures: vec!["sig:seeded".into()],
         },
     );
@@ -354,7 +358,8 @@
     assert_eq!(parsed["id"], id);
     // outPath comes from MockStore — this is the payload that matters for
-    // the cache hit.
-    assert_eq!(parsed["outPath"], out_path);
+    // the cache hit. MockStore has the full path; gateway stripped the
+    // prefix for the wire.
+    assert_eq!(parsed["outPath"], out_path_wire, "gateway should strip STORE_PREFIX");
```

---

## Step 5 — Golden conformance test (opcodes 42 + 43 against real nix-daemon)

**File:** new test functions in `rio-gateway/tests/golden_conformance.rs` + new helpers in `rio-gateway/tests/golden/daemon.rs`

This is the only test class that catches "our tests agree with each other but not with Nix." Every other test in Steps 4a-4c would still pass if we implemented the transform backwards in BOTH directions. The live daemon is the ground truth.

### The hard part: getting a realisation INTO the live daemon

Unlike `wopQueryPathInfo` (just needs a valid path in the db), `wopQueryRealisation` needs a **realisation** to exist — which only happens after the daemon itself built a CA derivation. `start_local_daemon()` already sets `NIX_CONFIG = "experimental-features = nix-command flakes ca-derivations"` (daemon.rs:461, :755), so the feature is enabled. But the daemon's db is either (a) a symlink to the host's `/nix/var/nix/db` (which may or may not have CA realisations) or (b) fresh/hermetic (empty realisations table).

Two viable approaches:

#### Approach A (recommended): build a CA drv inside the test, then query its realisation

```rust
// In rio-gateway/tests/golden/daemon.rs — new helper

/// Build a CA derivation against the live daemon and return
/// `(drv_output_id, out_path_basename)` so the test can query it back.
///
/// Uses `nix build --store unix://<socket>` so the realisation lands in
/// THIS daemon's db, not the host's. Returns the DrvOutput id string
/// (`"sha256:<modular-hash-hex>!out"`) discovered via
/// `nix derivation show --recursive`.
pub fn build_ca_realisation_in_daemon(socket: &str, state_dir: &Path) -> (String, String) {
    // 1. nix-instantiate a CA derivation (same expr as build_ca_test_path
    //    but with a unique name so it doesn't cache-hit the host store).
    //    Use --store unix://{socket} so the .drv lands in THIS daemon.
    let drv_path = /* nix-instantiate --store unix://{socket} --expr '
        derivation {
            name = "rio-golden-realisation";
            builder = "/bin/sh";
            args = ["-c" "echo -n golden-realisation > $out"];
            system = builtins.currentSystem;
            __contentAddressed = true;
            outputHashMode = "flat";
            outputHashAlgo = "sha256";
        }' */;

    // 2. nix build --store unix://{socket} {drv_path}^out
    //    This triggers wopBuildDerivation + wopRegisterDrvOutput INSIDE
    //    the daemon. The realisation now exists.
    let out_path = /* nix build --store unix://{socket} --no-link
                      --print-out-paths {drv_path}^out */;

    // 3. Discover the modular drv hash. `nix derivation show` emits the
    //    inputDrvs-normalized hash; for a leaf CA drv with no inputDrvs,
    //    `nix-store -q --hash {drv_path}` works but returns the store-path
    //    hash, NOT the modular hash. Correct source: the daemon's own
    //    realisations table, queried via:
    //      nix realisation info --store unix://{socket} {drv_path}^out --json
    //    → JSON with `id` field = "sha256:<modular-hex>!out".
    let realisation_json = /* nix realisation info --store unix://{socket}
                              {drv_path}^out --json */;
    let id: String = serde_json::from_str::<serde_json::Value>(&realisation_json)
        .unwrap()["id"].as_str().unwrap().into();

    let basename = out_path.strip_prefix("/nix/store/").unwrap().to_string();
    (id, basename)
}
```

**Hermetic-sandbox caveat:** `/bin/sh` doesn't exist in the nixbuild.net sandbox. Guard this test with the same hermetic-skip pattern as `build_ca_test_path()` — check for `RIO_GOLDEN_CA_PATH` or `linked_db` and `#[ignore]`/skip if hermetic. The VM test (Step 6, option B) is the hermetic backstop.

#### Approach B: register-then-query (simpler, daemon-side write + read roundtrip)

Skip building entirely — send `wopRegisterDrvOutput` to the live daemon with a hand-crafted Realisation JSON, then `wopQueryRealisation` for it. This tests that **our wire bytes** are parseable by the daemon (register direction) AND that we can parse **the daemon's wire bytes** (query direction).

```rust
// In rio-gateway/tests/golden_conformance.rs

/// Golden: wopRegisterDrvOutput then wopQueryRealisation against live daemon.
///
/// This is a write-then-read: send opcode 42 with a Realisation JSON (basename
/// outPath), then opcode 43 for the same id. Assert the daemon echoes the
/// basename back. Validates BOTH that the daemon accepts our Register wire
/// bytes AND that its Query response matches our expectations.
///
/// NOT a byte-for-byte comparison with rio-gateway (like the other goldens) —
/// this is a daemon-is-ground-truth test. rio-gateway isn't in the loop at all.
/// Its role: if THIS test asserts "basename" and the wire_opcodes tests assert
/// "basename", they agree. If someone breaks one direction, either this test
/// or the wire_opcodes test will diverge.
// r[verify gw.opcode.mandatory-set]
#[tokio::test]
async fn test_golden_live_realisation_register_then_query() -> anyhow::Result<()> {
    let (socket, _guard) = golden::daemon::fresh_daemon_socket();

    // out_path must be a path the daemon knows about (it validates existence
    // on RegisterDrvOutput). Use the standard golden fixture path, which
    // start_local_daemon() registers via --register-validity in hermetic mode
    // or sees via the symlinked db otherwise.
    let test_path = golden::daemon::build_test_path();
    let basename = test_path.strip_prefix("/nix/store/").unwrap();

    let drv_hash_hex = "ab".repeat(32);
    let id = format!("sha256:{drv_hash_hex}!out");
    let register_json = serde_json::json!({
        "id": id,
        "outPath": basename,  // ← the assertion under test
        "signatures": [],
        "dependentRealisations": {}
    }).to_string();

    // Two-opcode session: Register (42) then Query (43) on the same connection.
    let op_bytes = rio_test_support::wire_bytes! {
        u64: 42, string: &register_json,   // RegisterDrvOutput
        u64: 43, string: &id,              // QueryRealisation
    };

    let (_client, server) = golden::daemon::exchange_with_daemon(&socket, Some(&op_bytes)).await?;

    // Parse the server stream: handshake bytes + STDERR_LAST (register) +
    // STDERR_LAST (query) + u64(1) + string(json).
    // Skip handshake (N bytes, use existing helper), skip first STDERR_LAST,
    // skip second STDERR_LAST, read count + json.
    let mut cursor = std::io::Cursor::new(&server[/* post-handshake offset */..]);
    // drain first opcode's STDERR frames...
    // drain second opcode's STDERR frames...
    let count = rio_nix::protocol::wire::read_u64(&mut cursor).await?;
    assert_eq!(count, 1, "daemon should have the realisation we just registered");
    let json_str = rio_nix::protocol::wire::read_string(&mut cursor).await?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)?;

    // THE ASSERTION: real daemon returns basename, not full path.
    assert_eq!(
        parsed["outPath"].as_str().unwrap(),
        basename,
        "real nix-daemon serializes outPath as basename (no /nix/store/ prefix)"
    );
    assert!(!parsed["outPath"].as_str().unwrap().starts_with("/nix/store/"));

    Ok(())
}
```

**Recommend Approach B.** It's simpler (no build, no hermetic-skip), faster (no derivation build in the test), and tests exactly the wire-format question. The only prerequisite is that the daemon accepts `RegisterDrvOutput` for a path it knows about but didn't build itself — CppNix does (it's a cache-push operation, the binary cache populates realisations this way).

**What to assert:**
1. Register with basename → daemon returns `STDERR_LAST` (success), not `STDERR_ERROR`. This alone proves the register-direction wire format is correct.
2. Query returns `count = 1`.
3. `parsed["outPath"]` does NOT start with `/nix/store/`.
4. `parsed["outPath"]` equals the basename we sent.

**Negative test (optional but cheap):** send Register with `"outPath": test_path` (FULL path) and assert `STDERR_ERROR`. This proves the daemon actually cares — if it accepted both, our basename-on-wire invariant would be softer than we think.

---

## Step 6 — VM test correction

**File:** `nix/tests/scenarios/protocol.nix`
**Lines:** 13-15, 81-82, 106-119

### The problem

Three comments claim to cover `Realisation outPath basename`, but all three describe the `BuildResult.builtOutputs[].outPath` field inside opcode **47** (`wopBuildPathsWithResults`), not the `Realisation` JSON from opcode **43** (`wopQueryRealisation`). Commit `5786f82` fixed `write_build_result` in `build.rs:415-418` — the subtest at line 106 legitimately regression-tests THAT fix. But it has nothing to do with opcodes 42/43.

### Option A: fix the comments only (minimal)

Rename the coverage claim from "Realisation" to "BuildResult builtOutputs" so `grep`/readers don't think opcode 43 is covered.

```diff
--- a/nix/tests/scenarios/protocol.nix
+++ b/nix/tests/scenarios/protocol.nix
@@ -13,3 +13,5 @@
-#   Realisation outPath basename — build succeeding at all requires the
-#     client to parse BuildResult (which contains the Realisation JSON).
-#     Full-path outPath → "illegal base-32 char '/'" → build fails.
+#   BuildResult builtOutputs outPath basename — opcode 47
+#     (wopBuildPathsWithResults). Build succeeding requires the client to
+#     parse BuildResult, whose builtOutputs[].outPath is a basename.
+#     Full-path → "illegal base-32 char '/'" → build fails.
+#     NOT opcode 43 (wopQueryRealisation) — see phase4a §1.6 golden test.
@@ -81,2 +83,2 @@
-        #   Realisation outPath full-path → client's BuildResult parser
-        #     rejects with "illegal base-32 character '/'" after build.
+        #   BuildResult builtOutputs outPath full-path (opcode 47) →
+        #     client's BuildResult parser rejects with "illegal base-32
+        #     character '/'" after build.
@@ -106,4 +108,5 @@
-    with subtest("output round-trips (Realisation outPath basename parsed)"):
-        # The build succeeding at all requires the client's BuildResult
-        # parser to accept the Realisation JSON. But prove the output is
-        # also queryable — a separate opcode path (wopQueryPathInfo) that
+    with subtest("output round-trips (BuildResult builtOutputs parsed)"):
+        # Opcode 47 coverage, NOT opcode 43. The build succeeding requires
+        # the client's BuildResult parser to accept builtOutputs[].outPath
+        # as a basename. Opcode 43 (wopQueryRealisation) is covered by the
+        # golden_conformance.rs live-daemon test. Prove the output is also
+        # queryable — a separate opcode path (wopQueryPathInfo) that
```

### Option B: add actual opcode 42/43 coverage (comprehensive)

Add a new subtest in the cold-path scenario that builds a `__contentAddressed` derivation through rio. A successful CA build through rio's gateway exercises:
- Worker → scheduler → gateway → client: `BuildResult.builtOutputs` (opcode 47, already covered)
- Client → gateway → store: `wopRegisterDrvOutput` (opcode 42) — the nix client fires this after every CA build
- A followup `nix realisation info --store {store_url} {drv}^out` exercises `wopQueryRealisation` (opcode 43)

```nix
# New subtest in nix/tests/scenarios/protocol.nix coldScript, after
# "output round-trips":

with subtest("CA realisation roundtrip (opcodes 42+43, phase4a §1.6)"):
    # Build a content-addressed derivation through rio. Post-build, the
    # nix client sends wopRegisterDrvOutput (42) with a Realisation JSON
    # whose outPath is a BASENAME. Gateway must prepend /nix/store/ before
    # the gRPC call, or rio-store's validate_store_path rejects it.
    #
    # drvs.mkCaTrivial is a new helper in lib/derivations.nix:
    #   __contentAddressed=true, outputHashMode="flat", outputHashAlgo="sha256".
    ca_drv = client.succeed(
        "nix-instantiate --impure --argstr marker vm-ca-roundtrip "
        "${drvs.mkCaTrivial} 2>&1 | tail -1"
    ).strip()
    client.succeed(
        f"nix copy --no-check-sigs --derivation --to '{store_url}' {ca_drv}"
    )
    ca_out = client.succeed(
        f"nix build --no-link --print-out-paths --store '{store_url}' "
        f"--extra-experimental-features ca-derivations '{ca_drv}^out' 2>&1 | tail -1"
    ).strip()
    assert ca_out.startswith("/nix/store/"), f"CA build failed: {ca_out!r}"

    # Build succeeding ⇒ opcode 42 succeeded (gateway prepended prefix,
    # store accepted). Now opcode 43: query the realisation back. If the
    # gateway returns a full path in the JSON, `nix realisation info`
    # fails client-side with "illegal base-32 char '/'".
    realisation = client.succeed(
        f"nix realisation info --json --store '{store_url}' "
        f"--extra-experimental-features ca-derivations '{ca_drv}^out'"
    )
    parsed = _json.loads(realisation)
    # `nix realisation info` output shape: {"<drvhash>!out": {"outPath": "..."}}
    # — grab the single value. outPath here is post-client-parse, so it
    # will be a full path (client re-adds prefix). The test passing AT ALL
    # proves the wire was well-formed; assert the value matches our build.
    rinfo = next(iter(parsed.values()))
    assert rinfo["outPath"] == ca_out, (
        f"realisation outPath mismatch: {rinfo['outPath']!r} != {ca_out!r}"
    )
```

**Recommend Option A for THIS commit** (comment fix only), with Option B as a follow-up commit. Rationale: Option B needs a new `drvs.mkCaTrivial` helper in `nix/tests/lib/derivations.nix`, adds ~30s to the cold-path VM test, and the golden conformance test (Step 5) already provides the ground-truth coverage at unit-test speed. The VM subtest is nice-to-have (end-to-end through the real scheduler+store, not MockStore) but not load-bearing for this remediation.

If the reviewer wants Option B in the same commit, the `mkCaTrivial` helper is straightforward — same shape as `mkTrivial` but with `__contentAddressed = true; outputHashMode = "flat"; outputHashAlgo = "sha256";`. The client's `--extra-experimental-features ca-derivations` flag is required (the flag is per-invocation, the VM's `nix.conf` doesn't set it globally).

---

## Step 7 — Migration (prod data cleanup)

### Confirm the table is empty

§1.6 states the prod `realisations` table was verified empty. Confirm before landing:

```sql
-- Run against prod PG (read-only):
SELECT COUNT(*) FROM realisations;
-- Expected: 0
```

If `COUNT(*) = 0`: **no migration needed.** The gateway fix means all future rows get full paths. Done.

### If nonzero (contingency)

If somehow nonzero (e.g., a test-cluster row leaked, or someone manually inserted), existing rows have one of two corrupt shapes:

1. **Full path** (`/nix/store/...`) — came from our buggy test fixtures hitting a test cluster. These are **correct** post-fix (internal repr is full path). No change needed.
2. **Basename** (`abc...-foo`) — would only exist if a real nix client somehow got past the `validate_store_path` rejection (it can't — §1.6 verified the real store rejects). Vanishingly unlikely, but the fix is:

```sql
-- Contingency: normalize any basename rows to full paths.
-- Expected: 0 rows affected (real store's validate_store_path blocked these).
UPDATE realisations
   SET output_path = '/nix/store/' || output_path
 WHERE output_path NOT LIKE '/nix/store/%';
```

Given both directions are broken and CA is gated behind an experimental feature, the expected `COUNT(*)` is 0 and this step is a no-op confirmation.

---

## Atomicity check

Before committing, verify the intermediate states are NOT shippable:

| Step landed alone | `cargo nextest run` result | Why |
|---|---|---|
| Step 1 only | `ca_roundtrip.rs:108` FAILS | register sends basename, store has full path, query returns full path, test still asserts full path → passes by coincidence. Actually GREEN. But wrong. |
| Step 2 only | `ca_roundtrip.rs:84` FAILS | register still sends full path (test fixture), store has full path, query strips → returns basename, test asserts full path at :108 → FAILS |
| Step 3 only | `ca_roundtrip.rs:73` FAILS | test sends full path JSON → gateway passes full path through → MockStore now validates → passes (full path is valid). Wait no — test at :63 sends `output_path` (full). `MockStore` validates `/nix/store/` prefix → OK. GREEN. |
| Step 1+3 only | `ca_roundtrip.rs:84` FAILS | test sends full path at :63, gateway's idempotent guard doesn't double-prepend, MockStore gets full path, :84 asserts full path → GREEN. :108 asserts full path, query returns full path → GREEN. **Spuriously green — the test isn't load-bearing.** |
| Step 1+2+3 only (no Step 4) | `ca_roundtrip.rs:108` FAILS | test sends full path at :63 (idempotent guard → full path in store), query strips → basename at :108, test asserts full path → FAIL. **This is the correct failure that forces Step 4.** |
| All steps | GREEN | ✓ |

The key insight: **Step 3 (MockStore hardening) combined with Step 4 (test fixture flip to basename) is what makes the test load-bearing.** Landing them together is non-negotiable.

---

## Validation sequence

```bash
# 1. Unit tests (catches Steps 1-4 interaction)
nix develop -c cargo nextest run -p rio-gateway

# 2. Clippy (catches any borrowck/lifetime issues from the new let-binding)
nix develop -c cargo clippy --all-targets -- --deny warnings

# 3. Golden conformance (Step 5 — only works outside hermetic sandbox)
nix develop -c cargo nextest run -p rio-gateway golden_live_realisation

# 4. tracey (both impl sites are under r[impl gw.opcode.mandatory-set] — no new markers)
nix develop -c tracey query rule gw.opcode.mandatory-set

# 5. Full CI
nix-build-remote --no-nom --dev -- -L .#ci
```

---

## Spec/doc sync

No spec changes needed — `r[gw.opcode.mandatory-set]` already covers opcodes 42+43 (see `opcodes_read.rs:514` comment `// r[impl gw.opcode.mandatory-set]`). The golden test adds a `// r[verify gw.opcode.mandatory-set]` annotation.

Consider adding a narrower spec rule `r[gw.wire.realisation-json-basename]` in `docs/src/components/gateway.md` documenting the basename convention, with `r[impl]` at both `build.rs:415` and the new `opcodes_read.rs` sites. Follow-up, not blocking.
