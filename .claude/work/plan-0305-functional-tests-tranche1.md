# Plan 0305: Functional test tier — port Lix functionaltests2 scenarios (tranche 1)

User-requested high-priority feature. Rio's test pyramid has a hole in the middle:

| Tier | What | Backend | Speed | Finds |
|---|---|---|---|---|
| Unit ([`wire_opcodes/`](../../rio-gateway/tests/wire_opcodes/main.rs)) | Per-opcode wire format | `MockStore` in-memory | ~ms | Encoding bugs |
| Golden ([`golden_conformance.rs`](../../rio-gateway/tests/golden_conformance.rs)) | Byte-compare vs nix-daemon | `MockStore` | ~1s | Protocol drift |
| **—** | **—** | **—** | **—** | **—** |
| VM ([`nix/tests/scenarios/`](../../nix/tests/scenarios/)) | Full k3s+FUSE+cgroup | Real everything | 2-5min, KVM | System bugs |

The gap has cost us before. [P0054](plan-0054-wire-format-conformance-wopaddmultiple-narfrompath.md)'s note: **"CRITICAL: VM test caught, byte-tests were wrong."** The `wire_opcodes` tests for `wopAddMultipleToStore`/`wopNarFromPath` passed against `MockStore`; the real stack rejected the bytes. `MockStore` at [`rio-test-support/src/grpc.rs:39`](../../rio-test-support/src/grpc.rs) stores NARs in a `HashMap<String, (PathInfo, Vec<u8>)>` — it doesn't parse NARs, doesn't verify hashes, doesn't enforce reference integrity. It accepts anything.

**Lix [`functionaltests2`](https://git.lix.systems/lix-project/lix/src/branch/main/tests/functional2)** is a curated catalog of scenarios real nix clients depend on. It's Python/pytest (newer, cleaner than CppNix's [`tests/functional/`](https://github.com/NixOS/nix/tree/master/tests/functional) shell scripts). We port the **scenarios**, not the harness. The harness is a new `RioStack` fixture: `GatewaySession`'s DuplexStream pattern, but backed by real `StoreServiceImpl` + ephemeral PG instead of `MockStore`.

**Scope: tranche 1.** Survey + harness + 3 scenario ports. Follow-on tranches port remaining categories (CA/cutoff, structured-attrs, build-semantics) — noted at bottom of doc.

## Tasks

### T1 — `docs:` survey Lix functionaltests2, categorize by rio-relevance

Clone or browse [`git.lix.systems/lix-project/lix`](https://git.lix.systems/lix-project/lix) → `tests/functional2/`. Produce a table (append to this plan doc under `## Survey output` — yes, the implementer modifies the plan doc for this task; it's a research artifact):

| Lix test | Subsystem | Rio crate | Tranche | Why |
|---|---|---|---|---|
| `test_add_to_store_*` | store/put | rio-gateway+rio-store | **1** | Real hash verify — MockStore skips |
| `test_references_*` | store/closure | rio-gateway+rio-store | **1** | `references` always `NO_STRINGS` in current tests |
| `test_nar_*` | store/nar | rio-gateway+rio-store | **1** | P0054 bug class |
| `test_ca_*` | ca/cutoff | rio-scheduler | 2 | Needs real scheduler — out of scope tranche 1 |
| `test_flake_*` / `test_eval_*` | eval | — | never | Rio doesn't eval |
| `test_gc_*` | gc | rio-store | 3 | Different GC model — `r[store.gc.two-phase]` not nix's roots |
| … | | | | |

Categorize **every** test in functionaltests2. Tranche-1 = store-interacting opcode sequences. Tranche-2 = CA+scheduler (needs real scheduler fixture, which needs leader election stubbed — heavier). Tranche-3+ = store internals. Tranche-never = eval/flakes/lang.

Secondary: skim CppNix `tests/functional/` for anything functionaltests2 hasn't ported yet but rio cares about (the `ssh-ng` remote-store tests in particular — that's rio's exact client surface).

### T2 — `test(rio-test-support):` `RioStack` fixture — gateway + real rio-store + ephemeral PG

NEW `rio-gateway/tests/functional/mod.rs`. Cannot go in `rio-test-support` — `rio-store` dev-depends on `rio-test-support` ([`rio-store/Cargo.toml:103`](../../rio-store/Cargo.toml)), so `rio-test-support → rio-store` would cycle. Lives as a test-local module mirroring [`rio-gateway/tests/common/mod.rs`](../../rio-gateway/tests/common/mod.rs).

MODIFY [`rio-gateway/Cargo.toml:46`](../../rio-gateway/Cargo.toml) — add `rio-store = { workspace = true }` and `sqlx = { workspace = true }` to `[dev-dependencies]`. No cycle: both depend on `rio-proto`, neither depends on the other.

The fixture lifts [`StoreSession`](../../rio-store/tests/grpc/main.rs) (real `StoreServiceImpl` + `TestDb`) and [`GatewaySession`](../../rio-gateway/tests/common/mod.rs) (DuplexStream + `run_protocol`) into one struct:

```rust
// rio-gateway/tests/functional/mod.rs
use rio_store::grpc::StoreServiceImpl;
use rio_test_support::{TestDb, grpc::{MockScheduler, spawn_mock_scheduler}};

// rio-store's MIGRATOR is cfg(test) — integration tests compile the lib
// without cfg(test). sqlx::migrate! embeds at compile time from a path
// relative to CARGO_MANIFEST_DIR, so we keep a local copy. Same SQL.
// Mirrors rio-store/tests/grpc/main.rs:31.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Gateway wire protocol backed by REAL rio-store (ephemeral PG,
/// StoreServiceImpl) + MockScheduler. Functional-tier: catches bugs
/// MockStore hides (hash verify, NAR parse, reference integrity) without
/// the 2-5min VM cost.
///
/// Scheduler stays mocked: real scheduler needs leader election + worker
/// registration — too heavy for this tier. Tranche-2 scope.
pub struct RioStack {
    /// Client-side wire stream. Write opcodes, read responses.
    pub stream: tokio::io::DuplexStream,
    /// Direct DB access for white-box assertions (check manifests table,
    /// verify refcounts — things the wire protocol doesn't expose).
    pub db: TestDb,
    /// Scheduler mock — set build outcomes, inspect SubmitBuild calls.
    pub scheduler: MockScheduler,
    /// Direct store gRPC client — bypass the wire protocol for setup
    /// (seed paths) or for assertions gateway doesn't expose.
    pub store_client: rio_proto::StoreServiceClient<tonic::transport::Channel>,
    _store_handle: tokio::task::JoinHandle<()>,
    _sched_handle: tokio::task::JoinHandle<()>,
    _server_task: tokio::task::JoinHandle<()>,
}

impl RioStack {
    pub async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone());
        let router = tonic::transport::Server::builder()
            .add_service(rio_proto::StoreServiceServer::new(service));
        let (store_addr, store_handle) =
            rio_test_support::grpc::spawn_grpc_server(router).await;
        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;

        let store_client = rio_proto::client::connect_store(&store_addr.to_string()).await?;
        let sched_client = rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

        let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);
        let mut sc = store_client.clone();
        let mut scc = sched_client.clone();
        let server_task = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(server_stream);
            // Same EOF-is-clean / error-is-logged discipline as GatewaySession.
            let _ = rio_gateway::session::run_protocol(
                &mut r, &mut w, &mut sc, &mut scc, String::new()
            ).await;
        });

        Ok(Self {
            stream: client_stream, db, scheduler, store_client,
            _store_handle: store_handle,
            _sched_handle: sched_handle,
            _server_task: server_task,
        })
    }

    /// Handshake + setOptions done. Ready for opcodes.
    pub async fn new_ready() -> anyhow::Result<Self> {
        let mut s = Self::new().await?;
        rio_test_support::wire::do_handshake(&mut s.stream).await?;
        rio_test_support::wire::send_set_options(&mut s.stream).await?;
        Ok(s)
    }
}
```

`Drop` aborts handles. Follow `GatewaySession`'s pattern exactly — EOF-is-clean, error-is-logged-not-panicked (error-path tests deliberately trigger `STDERR_ERROR`).

### T3 — `test:` port add→query roundtrip — real hash verification

NEW `rio-gateway/tests/functional/store_roundtrip.rs`. The `MockStore` version at [`wire_opcodes/opcodes_write.rs:26`](../../rio-gateway/tests/wire_opcodes/opcodes_write.rs) sends `references: NO_STRINGS` and the mock accepts any hash. Real `StoreServiceImpl` computes the NAR hash on receipt ([`r[store.integrity.verify-on-put]`](../../docs/src/components/store.md)) and rejects mismatches.

Port Lix `test_add_to_store_flat` / `test_add_to_store_recursive` semantics:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn add_then_query_path_info_real_hash() -> TestResult {
    let mut stack = RioStack::new_ready().await?;
    let (nar, nar_hash) = make_nar(b"hello functional tier");

    // wopAddToStoreNar with the CORRECT hash. Real store verifies it.
    wire_send!(&mut stack.stream;
        u64: 39,                               // wopAddToStoreNar
        string: &test_store_path("func-a"),
        string: "",                            // deriver
        string: &hex::encode(nar_hash),        // narHash — store REJECTS if wrong
        strings: wire::NO_STRINGS,             // references (T5 covers non-empty)
        u64: 0, u64: nar.len() as u64,         // registrationTime, narSize
        u64: 0, strings: wire::NO_STRINGS,     // ultimate, sigs
        string: "",                            // ca
        u64: 0, u64: 0,                        // repair, dontCheckSigs
        framed: &nar,
    );
    drain_stderr_until_last(&mut stack.stream).await?;

    // wopQueryPathInfo — round-trip through real PG, real manifest table.
    wire_send!(&mut stack.stream;
        u64: 26, string: &test_store_path("func-a"),
    );
    drain_stderr_until_last(&mut stack.stream).await?;
    let valid = wire::read_u64(&mut stack.stream).await?;
    assert_eq!(valid, 1, "path should be valid after add");
    // ... read full PathInfo, assert nar_hash matches what we sent

    // White-box: manifest row actually exists in PG.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM manifests")
        .fetch_one(&stack.db.pool).await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn add_with_wrong_hash_rejected() -> TestResult {
    // Same as above but send nar_hash = zeros. MockStore accepts this.
    // Real store: STDERR_ERROR. This is the MockStore gap made concrete.
    let mut stack = RioStack::new_ready().await?;
    // ... send wopAddToStoreNar with hash = [0u8; 32]
    let err = drain_stderr_expecting_error(&mut stack.stream).await?;
    assert!(err.message.contains("hash mismatch") || err.message.contains("integrity"));
    Ok(())
}
```

### T4 — `test:` port add-multiple → nar-from-path — NAR framing through real reassembly

NEW `rio-gateway/tests/functional/nar_roundtrip.rs`. This is the P0054 bug class. `wopAddMultipleToStore` frames are unaligned ([`r[gw.opcode.add-multiple.unaligned-frames]`](../../docs/src/components/gateway.md)); `wopNarFromPath` streams raw bytes ([`r[gw.opcode.nar-from-path.raw-bytes]`](../../docs/src/components/gateway.md)). Real `StoreServiceImpl` parses the NAR into chunks ([`r[store.cas.fastcdc]`](../../docs/src/components/store.md)), stores them, and reassembles on read ([`r[store.nar.reassembly]`](../../docs/src/components/store.md)). MockStore does `HashMap::insert(bytes)` → `HashMap::get(bytes)` — byte-identical by construction, proves nothing.

Port Lix `test_nar_roundtrip` semantics. Use `RioStack::new()` variant with `StoreServiceImpl::new_chunked()` (MemoryChunkBackend) so NARs actually get chunked + reassembled, not inline-blob shortcut:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn add_multiple_then_nar_from_path_byte_identical() -> TestResult {
    let mut stack = RioStack::new_chunked_ready().await?;

    // 3 paths via wopAddMultipleToStore. Different sizes to hit chunk
    // boundaries. ~100KB each → crosses FastCDC normal-size (8KB default).
    let paths = (0..3).map(|i| {
        let (nar, hash) = make_nar(&vec![i as u8; 100_000]);
        (test_store_path(&format!("func-nar-{i}")), nar, hash)
    }).collect::<Vec<_>>();

    wire_send!(&mut stack.stream; u64: 44, u64: 0, u64: 0, u64: 3); // wopAddMultipleToStore, repair=0, dontCheck=0, num=3
    for (path, nar, hash) in &paths {
        // Per-path: PathInfo fields + raw NAR bytes (NOT framed — r[gw.opcode.add-multiple.unaligned-frames])
        // ... full PathInfo wire encoding
        wire_send!(&mut stack.stream; raw: nar);
    }
    drain_stderr_until_last(&mut stack.stream).await?;

    // Read each back. Bytes MUST be identical — round-tripped through
    // FastCDC chunk → PG manifest → MemoryChunkBackend → reassembly.
    for (path, original_nar, _) in &paths {
        wire_send!(&mut stack.stream; u64: 38, string: path); // wopNarFromPath
        drain_stderr_until_last(&mut stack.stream).await?;
        let mut received = vec![0u8; original_nar.len()];
        tokio::io::AsyncReadExt::read_exact(&mut stack.stream, &mut received).await?;
        assert_eq!(&received, original_nar, "NAR bytes must survive chunk+reassemble");
    }
    Ok(())
}
```

### T5 — `test:` port references chain — closure integrity through real store

NEW `rio-gateway/tests/functional/references.rs`. Every `wire_opcodes` test sends `references: NO_STRINGS`. Real nix workflows are reference chains: `hello.drv` → `glibc`, `bash`. `wopQueryValidPaths` with `substitute=true` walks references; `wopQueryMissing` computes closure. None of that touches a real reference graph today outside VM tests.

Port Lix `test_references` / `test_closure` semantics:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn references_chain_query_missing() -> TestResult {
    let mut stack = RioStack::new_ready().await?;

    // Chain: C -> B -> A. Add A first (no refs), then B (refs=[A]), then C (refs=[B]).
    // Store enforces: can't add B if A isn't valid. Mock doesn't check.
    let path_a = test_store_path("chain-a");
    let path_b = test_store_path("chain-b");
    let path_c = test_store_path("chain-c");

    // A: leaf
    let (nar_a, hash_a) = make_nar(b"leaf");
    add_to_store_nar(&mut stack, &path_a, &nar_a, hash_a, &[]).await?;

    // B: references A
    let (nar_b, hash_b) = make_nar_with_ref(&path_a);  // NAR body mentions path_a
    add_to_store_nar(&mut stack, &path_b, &nar_b, hash_b, &[&path_a]).await?;

    // C: references B (transitively A)
    let (nar_c, hash_c) = make_nar_with_ref(&path_b);
    add_to_store_nar(&mut stack, &path_c, &nar_c, hash_c, &[&path_b]).await?;

    // wopQueryMissing for C alone → willSubstitute should be empty (all present),
    // but the closure walk must find B and A in the store's reference graph.
    wire_send!(&mut stack.stream;
        u64: 40,                          // wopQueryMissing
        strings: &[path_c.as_str()],
    );
    drain_stderr_until_last(&mut stack.stream).await?;
    // ... read willBuild/willSubstitute/unknown — all should be empty
    //     (everything is present). This proves closure walk reached PG.

    // White-box: references table has the edges.
    let edges: Vec<(String, String)> = sqlx::query_as(
        "SELECT referrer, referenced FROM path_references ORDER BY referrer"
    ).fetch_all(&stack.db.pool).await?;
    assert_eq!(edges.len(), 2);  // B→A, C→B
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn add_with_dangling_reference_rejected() -> TestResult {
    // Add B with references=[A] but A doesn't exist. MockStore: fine.
    // Real store: must reject (or accept-but-warn? check r[store.integrity.*]).
    // Lix test_add_missing_reference establishes the expected behavior.
    // ...
}
```

`add_to_store_nar` / `make_nar_with_ref` are local helpers — not `rio-test-support` exports yet (tranche 2 promotes if stable).

### T6 — `docs:` verification.md functional tier row

MODIFY [`docs/src/verification.md`](../../docs/src/verification.md). New section between `## Unit Tests` (`:29`) and `## Integration Tests` (`:42`). The existing `## Integration Tests` section lists scenarios — not what's actually tested (most of that list is VM-only aspirational). This new section is the concrete middle tier:

```markdown
## Functional Tests

Gateway wire protocol against **real `rio-store`** (ephemeral PG, `StoreServiceImpl`) — the `RioStack` fixture. No k8s, no VM, no KVM. Catches bugs `MockStore` hides:

- `add` → `query` with hash verification (MockStore accepts any hash)
- `add-multiple` → `nar-from-path` through FastCDC chunk+reassembly (MockStore: `HashMap` round-trip)
- Reference chains (`wire_opcodes` tests always send `references: NO_STRINGS`)

Scenarios ported from [Lix `functionaltests2`](https://git.lix.systems/lix-project/lix/src/branch/main/tests/functional2). Run: `cargo nextest run -p rio-gateway --test functional`.
```

Also add a row to the CI-tier table at `:87-90`: functional tests are in `.#ci`'s nextest run already (they're just `#[tokio::test]` in `rio-gateway/tests/`), so no table change — but add a parenthetical to the existing `| CI |` row: "Unit tests, **functional tests (real rio-store)**, clippy, …".

## Exit criteria

- T1 survey table appended to this plan doc, every Lix functionaltests2 test categorized by tranche (1/2/3/never)
- `RioStack::new()` + `RioStack::new_ready()` work; `cargo nextest run -p rio-gateway --test functional` green
- `add_with_wrong_hash_rejected` FAILS when `StoreServiceImpl::put_path` is patched to skip verification — proves the test sees what MockStore can't
- `add_multiple_then_nar_from_path_byte_identical` round-trips ≥100KB NARs through real FastCDC chunking (not inline-blob)
- `references_chain_query_missing` asserts rows in `path_references` table — white-box proof the graph is real
- `docs/src/verification.md` has the Functional Tests section; `## Integration Tests` list unchanged (it's VM-aspirational, not this tier)

## Tracey

References existing markers — these already have `r[verify]` annotations, but only from mock-backed tests. This plan adds second `r[verify]` lines from real-stack tests (tracey allows multiple verify sites per rule):

- [`r[store.integrity.verify-on-put]`](../../docs/src/components/store.md) — T3 `add_with_wrong_hash_rejected` verifies from the gateway side
- [`r[store.nar.reassembly]`](../../docs/src/components/store.md) — T4 verifies through the full wire path, not store-in-isolation
- [`r[store.cas.fastcdc]`](../../docs/src/components/store.md) — T4 chunked variant
- [`r[gw.opcode.add-multiple.unaligned-frames]`](../../docs/src/components/gateway.md) — T4 against real store (currently only golden-tested)
- [`r[gw.opcode.nar-from-path.raw-bytes]`](../../docs/src/components/gateway.md) — T4 reassembly path
- [`r[gw.opcode.query-missing]`](../../docs/src/components/gateway.md) — T5 with non-empty closure

No new markers. This is a test-infra plan — it deepens coverage of existing spec, doesn't add spec.

## Files

```json files
[
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "T2: add rio-store + sqlx to [dev-dependencies]"},
  {"path": "rio-gateway/tests/functional/mod.rs", "action": "NEW", "note": "T2: RioStack fixture (real StoreServiceImpl + TestDb + MockScheduler + DuplexStream)"},
  {"path": "rio-gateway/tests/functional/main.rs", "action": "NEW", "note": "T2: test binary entrypoint — mod declarations, shared helpers"},
  {"path": "rio-gateway/tests/functional/store_roundtrip.rs", "action": "NEW", "note": "T3: add->query with real hash verify + wrong-hash-rejected"},
  {"path": "rio-gateway/tests/functional/nar_roundtrip.rs", "action": "NEW", "note": "T4: add-multiple->nar-from-path through real FastCDC"},
  {"path": "rio-gateway/tests/functional/references.rs", "action": "NEW", "note": "T5: reference chain + dangling-ref rejection"},
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "T6: Functional Tests section between Unit (:29) and Integration (:42)"},
  {"path": ".claude/work/plan-0305-functional-tests-tranche1.md", "action": "MODIFY", "note": "T1: implementer appends survey table under ## Survey output"}
]
```

```
rio-gateway/
├── Cargo.toml                    # T2: +rio-store, +sqlx dev-deps
└── tests/functional/             # NEW directory
    ├── mod.rs                    # T2: RioStack fixture
    ├── main.rs                   # T2: test binary entrypoint
    ├── store_roundtrip.rs        # T3: add→query, hash verify
    ├── nar_roundtrip.rs          # T4: add-multiple→nar-from-path
    └── references.rs             # T5: reference chains
docs/src/
└── verification.md               # T6: Functional Tests section
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [300], "note": "User-requested HIGH priority — fills the MockStore gap that historically cost us (P0054). Test-infra parallel to feature work. soft_dep P0300: it adds Lix as a flake input — if it lands first, T1 can survey from the flake-input source tree instead of external clone. Non-blocking: external browse works fine."}
```

**Depends on:** none — test-infra is parallel to feature work. `RioStack` uses only already-public types: [`StoreServiceImpl`](../../rio-store/src/grpc/) (pub), [`TestDb`](../../rio-test-support/src/pg.rs) (pub), [`MockScheduler`](../../rio-test-support/src/grpc.rs) (pub).

**Soft dep:** [P0300](plan-0300-multi-nix-compat-matrix.md) adds Lix as a flake input (`inputs.lix`). If it lands first, T1's survey can read `${inputs.lix}/tests/functional2/` from the nix store instead of an external clone. Not blocking — the survey is a one-time browse of public source.

**Conflicts with:**
- [`rio-gateway/Cargo.toml`](../../rio-gateway/Cargo.toml) — not in collision top-30. `[dev-dependencies]` at `:46` is append-only.
- [`docs/src/verification.md`](../../docs/src/verification.md) — P0300 touches `:10` and `:92`, P0268 touches `:83`, P0221/P0301 touch `:92`. This plan inserts a new section between `:29` and `:42` — non-overlapping insert point. Low contention.
- `rio-gateway/tests/functional/` — NEW directory, zero collision.

## Follow-on tranches (scoped out)

| Tranche | Scope | Blocker |
|---|---|---|
| 2 | CA/cutoff scenarios, `wopBuildDerivation` with real scheduler | `RioStack` with real `SchedulerActor` — needs leader election stub, worker registration mock (builder seam landed [P0318](plan-0318-riostackbuilder-tranche2-axis.md) — tranche-2 plan uses `.with_real_scheduler()` and fills the stub) |
| 3 | Store-internal: GC two-phase, signature verification chains, narinfo HTTP | Real S3 backend (MinIO ephemeral, like PG?) |
| 4 | `ssh-ng` end-to-end: real SSH transport instead of DuplexStream | `russh` test fixture — [`ssh_hardening.rs`](../../rio-gateway/tests/ssh_hardening.rs) has the start |

Each becomes its own P-doc after T1's survey table exists — the table IS the backlog.

## Survey output

Surveyed at Lix `774f95759908891d9f9bcd2dd3271f5c148359d7` (2026-03-18). P0300 not yet landed; cloned externally. 90 test files across 8 directories; ~45 distinct test scenarios after filtering out testlib self-tests.

### `tests/functional2/store/` — primary tranche-1 material

| Lix test | Subsystem | Rio crate | Tranche | Why |
|---|---|---|---|---|
| `test_add.py::test_add_fixed` | store/put flat CA | rio-gateway+rio-store | **1** | `wopAddToStore` with `fixed:sha256` — T3 covers the wire shape; this validates store-side hash compute |
| `test_add.py::test_add_fixed_rec` | store/put recursive CA | rio-gateway+rio-store | **1** | `wopAddToStore` with `fixed:r:sha256` — same path, recursive NAR ingestion |
| `test_add.py::test_hash` | store/query hash roundtrip | rio-gateway+rio-store | **1** | `wopQueryPathInfo` nar\_hash field matches sha256 of uploaded bytes — exactly the T3 scenario |
| `test_add.py::test_invalid_flat_add` | store/put validation | rio-gateway | 2 | Rejection of symlink/dir for flat ingestion — needs NAR tree fixtures beyond single-file `make_nar` |
| `test_add.py::TestNix3AddPath::test_nix3_rec_some_references` | store/put with refs | rio-gateway+rio-store | **1** | Non-empty references on add — the T5 scenario; Lix's only direct references-list test |
| `test_add.py::TestNix3AddPath::test_nix3_rec_bad_json*` | cli input validation | — | never | nix3 `--references-list-json` CLI parsing; rio has no JSON CLI surface |
| `test_evil_nars.py::test_evil_nar` (13 parametrized cases) | store/nar parse | rio-store+rio-nix | 2 | Malformed NAR rejection: slashes in names, `.`/`..` entries, NUL bytes, misorder, duplicates, case-hack collisions. **HIGH value** — `nar_parsing` fuzz target covers random bytes but not these semantic attacks. Needs `DirectoryUnordered`-style NAR tree builder. |
| `test_evil_nars.py::test_unicode_evil_nar` | store/nar parse | rio-nix | 2 | NFC vs NFD unicode normalization in NAR entry names |
| `test_ssh_relay.py::test_ssh_relay` | ssh-ng transport | rio-gateway | 4 | Daemon-through-daemon proxy over ssh-ng. Rio's exact client surface but needs real SSH fixture (russh). |
| `test_http.py::test_http_simple` | binary cache HTTP | rio-store cache\_server | 3 | `/nix-cache-info` + narinfo fetch. rio-store has this route; no rio test exercises the full client flow. |
| `test_gc.py::test_selfref_gc` | gc | rio-store gc | 3 | Self-referential path GC safety. Different GC model (`r[store.gc.two-phase]` vs nix roots) but the self-ref edge case is universal. |
| `test_local_store.py::TestLocalStore::test_path_info_*` | store/query | rio-gateway | 2 | `wopQueryPathInfo` wire format with relative/absolute/URI path normalization — rio only accepts absolute |
| `test_local_store.py::test_local_is_trusted` / `test_doctor_shows_trust` | trust flag | rio-gateway | 2 | Handshake trusted-flag semantics. Existing golden tests cover the byte but not the semantics. |
| `test_dump_db.py::test_dump_db` | store internals | — | never | `nix-store --dump-db` SQLite schema export; rio uses PG, no equivalent |
| `test_optimise_store.py::test_optimise_store*` | store dedup | — | never | nix's hardlink-based optimization; rio dedups via FastCDC chunks, different mechanism |
| `test_case_hack.py::test_case_hack` | nar case-insensitive FS | rio-nix | 3 | `~nix~case~hack~N` suffix handling for macOS/Windows. Low priority (rio workers are Linux). |
| `test_placeholders.py::test_placeholders` | build placeholder substitution | rio-scheduler | 2 | Output path placeholders in drv env — needs real scheduler |
| `test_xattrs.py::test_add_xattrs_*` (6 cases) | store/put xattr strip | rio-nix nar | 3 | NAR serialization must strip xattrs. Rio's `NarNode` doesn't model xattrs so this is implicitly correct — but no test proves it. |
| `test_simple.py::test_store_system` / `test_out_path` | eval + build | — | never | `nix-instantiate` + `nix-build` — eval-dependent |
| `test_build.py::test_build_dir_permissions` | sandbox | rio-worker | 3 | Build dir permission bits — worker-side, VM territory |

### `tests/functional2/store/cache/` — binary cache

| Lix test | Subsystem | Rio crate | Tranche | Why |
|---|---|---|---|---|
| `test_cache_compressions.py::test_cache_compressions` | binary cache | rio-store cache\_server | 3 | zstd/xz/bzip2/gzip NAR compression. rio only does zstd; test would need parametrization. |
| `test_compression_levels.py::test_compression_levels` | binary cache | rio-store cache\_server | 3 | zstd level tuning — operational, not protocol |
| `test_substitute_truncated_nar.py::test_substitute_truncated_nar` | binary cache integrity | rio-store | **1** (adapt) | Client receives truncated NAR from cache → must reject. Adapt as: `wopNarFromPath` after store-side manifest corruption. The T4 integrity guarantee. |

### `tests/functional2/build/` — needs real scheduler (tranche 2+)

| Lix test | Subsystem | Rio crate | Tranche | Why |
|---|---|---|---|---|
| `test_ca.py::test_*_cert_in_*_builds` (3 cases) | FOD SSL cert env | rio-worker | 3 | `NIX_SSL_CERT_FILE` in FOD sandbox. Rio does this via `impureEnvVars` (P0243 territory). |
| `test_fixed.py::test_good` / `test_bad` / `test_check` | FOD hash verify | rio-scheduler+rio-worker | 2 | FOD output hash mismatch handling. Core CA behavior — needs real scheduler + worker. |
| `test_fixed.py::test_illegal_references` | FOD reference scan | rio-worker | 2 | FOD outputs can't have references. `r[worker.refscan.*]` territory. |
| `test_fixed.py::test_same_as_add` | FOD ↔ add equivalence | rio-store | 2 | Building an FOD and adding its output directly → same store path. Good invariant test. |
| `test_fixed.py::test_parallel_same` | scheduler dedup | rio-scheduler | 2 | Two builds of same drv → one execution. `r[sched.dedup.*]`. |
| `test_remote.py::test_remote_trustless_*` (3: unsigned/ia/ca) | remote build trust | rio-gateway+rio-scheduler | 2 | **High value for rio** — trustless remote builds over ssh-ng. Rio's actual deployment model. Input-addressed and CA variants. |
| `test_substitution.py::test_substitution_fallback_*` (4 cases) | substituter chain | — | never | Multi-substituter fallback; rio has one store, no chain |
| `test_structured_attrs.py::test_improper_structured_attrs_drv` | drv parsing | rio-nix | 2 | `__structuredAttrs` with bad `__json`. `derivation_parsing` fuzz target relevant. |
| `test_pass_as_file.py::test_pass_as_file` | build env | rio-worker | 3 | `passAsFile` attr → file in build dir. Worker-side. |
| `test_output_normalization.py::test_output_normalization` | build output | rio-worker | 2 | Mtimes zeroed, mode normalized. `r[worker.normalize.*]`. |
| `test_delete.py::test_regression_6572_*` | gc + build race | rio-store gc | 3 | GC-during-build deletion race. Rio's `scheduler_live_pins` mechanism should prevent; worth testing. |
| `test_build_jobless.py::test_j0_*` (4 cases) | scheduler policy | — | never | `-j0` local-job restriction; rio always dispatches to workers |
| `test_xattrs.py::test_xattrs_*` (2 cases) | build sandbox | rio-worker | 3 | xattr handling in sandbox — worker-side |

### `tests/functional2/commands/` — mostly CLI-facing

| Lix test | Subsystem | Rio crate | Tranche | Why |
|---|---|---|---|---|
| `test_build/test_reference_checks.py::test_references_detected` | refscan | rio-worker | 2 | Reference scanner finds store paths in output. `r[worker.refscan.scan]`. |
| `test_build/test_reference_checks.py::test_allowed_references_*` (3) | refscan policy | rio-worker | 2 | `allowedReferences` drv attr enforcement |
| `test_build/test_reference_checks.py::test_disallowed_references` | refscan policy | rio-worker | 2 | `disallowedReferences` drv attr enforcement |
| `test_build/test_build_fod.py::test_url_mismatch` + keep-going variants | FOD failure | rio-scheduler | 2 | FOD hash mismatch error propagation. Known rio gap (P0243 "Failed-FOD BuildResult doesn't propagate over ssh-ng"). |
| `test_build/test_build_fod.py::test_missing_dependency*` | scheduler DAG | rio-scheduler | 2 | Missing-input dep error. |
| `test_build/test_build_multiple_outputs.py::test_build_outputs*` (7) | multi-output drv | rio-scheduler | 2 | `^out`, `^*` output selectors. `DerivedPath` parsing + scheduler handling. |
| `test_build/test_build_output_cycles.py::test_cycle*` (3) | build validation | rio-scheduler | 2 | Output reference cycle detection |
| `test_build/test_tarball.py::test_fetch_*` (8 cases) | eval fetchers | — | never | `fetchTarball`/`fetchTree` — eval-side |
| `test_build/test_timeout.py::test_timeout_*` (5 cases) | build timeout | rio-worker | 3 | `max-build-log-size`, `max-silent-time`. Rio has these knobs but tested only in VM. |
| `test_hash.py` | cli hash | — | never | `nix hash` CLI subcommand |
| `test_search.py` / `test_why_depends.py` / `test_env_query_xml.py` | cli queries | — | never | `nix search`, `nix why-depends`, `nix-env -q --xml` |
| `test_custom_sub_commands.py` | cli extensibility | — | never | `nix-foo` on PATH dispatch |
| `test_store_delete/test_unlink.py` | gc | rio-store gc | 3 | `nix store delete` — rio equivalent is admin GC RPC |

### `tests/functional2/daemon/`

| Lix test | Subsystem | Rio crate | Tranche | Why |
|---|---|---|---|---|
| `test_trust.py::test_trust` (parametrized) | handshake trust | rio-gateway | 2 | `trusted-users`/`allowed-users` matching. Rio uses JWT claims not unix users, but the trusted-flag wire semantics are the same. |

### `tests/functional2/cli/`, `eval/`, `flakes/`, `lang/` — all tranche-never

| Directory | Files | Why never |
|---|---|---|
| `cli/` | 7 tests (`test_fmt`, `test_profile`, `test_completions`, `test_compute_levels`, `test_suggestions`, `test_daemon`, `test_store`) | CLI-layer: `nix fmt`, `nix profile`, shell completion, system-features detection. Rio has no CLI. `test_daemon.py` is daemon-pid-file management, not protocol. |
| `eval/` | 12 tests | `nix eval`, NIX\_PATH, pure-eval, fetchMercurial, function tracing. Rio doesn't evaluate. |
| `flakes/` | 23 tests | Flake lockfiles, inputs, follows, registry. Rio doesn't evaluate. |
| `lang/` | 9 tests | `builtins.*`, parser whitespace, search paths. Rio doesn't evaluate. |

### Secondary: `tests/functional/` (CppNix-style shell scripts, 89 files)

Most scenarios duplicate `functional2/`. Notable rio-relevant tests NOT yet in `functional2/`:

| Shell test | Subsystem | Tranche | Why |
|---|---|---|---|
| `remote-store.sh` | ssh-ng daemon-through-daemon | 4 | Proxies one daemon through another over ssh-ng. Also tests `nix store ping --json` trusted flag. |
| `nix-copy-ssh-ng.sh` | ssh-ng path transfer | 4 | `nix copy --to ssh-ng://` — the `wopAddMultipleToStore` real-client exercise. **This is the P0054 reproduction.** |
| `nix-copy-ssh-common.sh` | ssh transfer shared | 4 | Shared helpers for the ssh copy tests |
| `nar-access.sh` | `nix store cat`/`ls` over NAR | 2 | NAR traversal via store — `wopNarFromPath` + client-side NAR parse |
| `build-remote-input-addressed.sh` | remote build IA | 2 | Remote build with IA derivations. Core rio flow. |
| `build-remote-content-addressed-fixed.sh` | remote build CA | 2 | Remote build with CA fixed derivations. CA cutoff path. |
| `binary-cache.sh` | binary cache full flow | 3 | End-to-end binary cache populate + substitute |
| `legacy-ssh-store.sh` | ssh (non-ng) | never | Legacy `ssh://` protocol — rio only speaks `ssh-ng://` |

### Tranche summary

| Tranche | Count | Theme | Fixture needs |
|---|---|---|---|
| **1** (this plan) | 5 scenario groups | Store put/get roundtrip, hash verify, references, NAR integrity | `RioStack` (real `StoreServiceImpl` + PG + MockScheduler). T3-T5 implement. |
| 2 | ~22 | CA/FOD builds, refscan, trustless remote, multi-output, structured-attrs | `RioStack` + real `SchedulerActor` (leader election stub, worker registration mock). ~1 sprint. (builder seam landed [P0318](plan-0318-riostackbuilder-tranche2-axis.md) — tranche-2 plan uses `.with_real_scheduler()` and fills the stub) |
| 3 | ~14 | Binary cache HTTP, GC, xattrs, timeouts, build-dir permissions | MinIO ephemeral (S3 backend), some VM overlap |
| 4 | ~5 | ssh-ng transport end-to-end | russh test fixture — `nix copy --to ssh-ng://` with real SSH |
| never | ~50 | eval, flakes, lang, CLI, substituter-chains | Out of rio's domain |

**Tranche-1 picks for T3–T5:** The plan's three scenarios (add→query hash verify, add-multiple→nar-from-path chunked roundtrip, reference chain) map directly to `test_add.py::test_hash`, `test_substitute_truncated_nar.py` (adapted), and `test_add.py::TestNix3AddPath::test_nix3_rec_some_references`. No Lix test is a 1:1 port — Lix's harness is `nix`-CLI-shaped; rio's is wire-protocol-shaped. We port the **scenario** (what's being proved) not the **invocation shape**.
