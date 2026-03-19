# Plan 0351: spawn_grpc_server_layered — Router<L> generic gap

consolidator-mc90 flagged, P0338 added the second inline copy this window. [`rio-test-support/src/grpc.rs:973`](../../rio-test-support/src/grpc.rs) `spawn_grpc_server` takes `tonic::transport::server::Router` (which is `Router<Identity>` — the default no-layer type). Any test needing `.layer()` (e.g., `InterceptorLayer` for fake JWT `Claims` injection) cannot use it: `.layer()` changes the type to `Router<Stack<InterceptorLayer<_>, Identity>>`, type mismatch.

**Two inline copies today:**

- [`rio-store/tests/grpc/chunk_service.rs:147-173`](../../rio-store/tests/grpc/chunk_service.rs) (P0264, mc=87) — StoreService + ChunkService with `InterceptorLayer`, 27L bind/spawn/yield. Comment at `:147-152` explains the helper gap.
- [`rio-store/tests/grpc/signing.rs:195-229`](../../rio-store/tests/grpc/signing.rs) (P0338, mc=95) — StoreService with `fake_interceptor`, 35L bind/spawn/yield + connect. Comment at `:183-186` same explanation.

Both carry near-identical "can't use `spawn_grpc_server` because `.layer()` changes `Router<Identity>` → `Router<Stack<...>>`" prose. Third inline likely: ANY test verifying `jwt_interceptor` pass-through to a handler reading `Claims` from extensions needs `InterceptorLayer` — the only way to populate extensions in-process. [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) T3 tests SIGHUP swap without a server, but P0349 follow-on or any P034x testing tenant-scoped RPCs will need it. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T23 (`batch_outputs_signed_with_tenant_key`) + T26 ([P0350](plan-0350-chunk-tenants-junction-cleanup-on-gc.md)'s downstream) will hit the same wall.

Two fix options:
- **(a)** make `spawn_grpc_server` generic over `L` — hairy: tonic `Router<L>` bounds involve `L: Layer<Routes> + Clone + Send + 'static` plus `L::Service: Service<Request<Body>> + ...`. The ~20 existing callers that don't use `.layer()` would need turbofish or type inference to hold.
- **(b)** add `spawn_grpc_server_layered<L>(router: Router<L>)` — second fn, caller-transparent bounds (tonic constrains `Router<L>: Service` internally via `serve_with_incoming`). Existing callers unchanged. Chosen here.

(b) is the pragmatic path: `spawn_grpc_server_layered` mirrors `spawn_grpc_server`'s body exactly — the genericness comes from tonic's own `Router<L>::serve_with_incoming`, which is already generic over `L` with the right bounds. We just pass it through.

[P0340](plan-0340-extract-seed-tenant-test-helper.md) touches `signing.rs:265` for `seed_tenant` migration — same file as copy-2, doesn't overlap `:195-229` but same-file rebase if both in-flight. Sequence this AFTER P0340 if both dispatch near-simultaneously.

## Entry criteria

- [P0338](plan-0338-tenant-signer-wiring-putpath.md) merged (second inline copy exists at `signing.rs:195-229` — DONE)

## Tasks

### T1 — `feat(test-support):` spawn_grpc_server_layered generic helper

MODIFY [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) after `spawn_grpc_server` (`~:988`):

```rust
/// Generic variant of [`spawn_grpc_server`] for routers with layers.
///
/// `Server::builder().layer(...)` changes `Router<Identity>` →
/// `Router<Stack<L, Identity>>`, which the non-generic
/// [`spawn_grpc_server`] can't accept. This variant passes the layer
/// parameter through to tonic's own generic `serve_with_incoming`.
///
/// ```ignore
/// let router = Server::builder()
///     .layer(InterceptorLayer::new(fake_interceptor))
///     .add_service(FooServer::new(foo));
/// let (addr, handle) = spawn_grpc_server_layered(router).await;
/// ```
///
/// The `L` bound mirrors tonic's `Router<L>::serve_with_incoming` —
/// `L: Layer<Routes>` where `L::Service` is a tower Service. Callers
/// don't spell the bounds; inference carries them through from the
/// `.layer(...)` call site.
pub async fn spawn_grpc_server_layered<L>(
    router: tonic::transport::server::Router<L>,
) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    L: tower::Layer<tonic::service::Routes> + Clone + Send + 'static,
    L::Service: tower::Service<
            http::Request<tonic::body::BoxBody>,
            Response = http::Response<tonic::body::BoxBody>,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    <L::Service as tower::Service<http::Request<tonic::body::BoxBody>>>::Future: Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("in-process gRPC server");
    });
    tokio::task::yield_now().await;
    (addr, handle)
}
```

The `where` clause is lifted from tonic's own `Router::serve_with_incoming` bound (check at dispatch — tonic 0.12's `Router<L>` has the exact signature; copy-paste it). If tonic's bound changes in a minor bump, this compiles-fails loudly rather than silently.

**Alternative considered:** make `spawn_grpc_server` itself generic with `L = Identity` default. Rust's default type parameters on functions are unstable (`associated_type_defaults`); would need a blanket-impl trick or a trait. Two functions is simpler.

### T2 — `refactor(store):` migrate chunk_service.rs inline to helper

MODIFY [`rio-store/tests/grpc/chunk_service.rs`](../../rio-store/tests/grpc/chunk_service.rs) at `:147-173`. Replace the inline `listener = bind... spawn...yield_now` block with:

```rust
        // Per-request fake JWT Claims injection via InterceptorLayer.
        // Wraps ALL services — same pattern as production (main.rs:495).
        // StoreService ignores the header; ChunkService reads it.
        let router = Server::builder()
            .layer(tonic::service::InterceptorLayer::new(
                test_tenant_interceptor,
            ))
            .add_service(StoreServiceServer::new(store_service))
            .add_service(ChunkServiceServer::new(chunk_service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
```

Net: ~27L → ~10L. Delete the "can't use the shared helper because..." comment — it's now wrong.

### T3 — `refactor(store):` migrate signing.rs inline to helper

MODIFY [`rio-store/tests/grpc/signing.rs`](../../rio-store/tests/grpc/signing.rs) `spawn_store_with_fake_jwt` at `:195-229`. Replace the inline block with:

```rust
async fn spawn_store_with_fake_jwt(
    service: StoreServiceImpl,
    tenant_id: uuid::Uuid,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let fake_interceptor = move |mut req: tonic::Request<()>| {
        req.extensions_mut().insert(rio_common::jwt::Claims {
            sub: tenant_id,
            iat: 1_700_000_000,
            exp: 9_999_999_999,
            jti: "test-session-fake".into(),
        });
        Ok(req)
    };

    let router = Server::builder()
        .layer(tonic::service::InterceptorLayer::new(fake_interceptor))
        .add_service(StoreServiceServer::new(service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;

    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    Ok((StoreServiceClient::new(channel), server))
}
```

Keep the doc-comment at `:180-194` (explains WHY fake-interceptor vs real JWT verify); **delete** `:183-186` (the "can't reuse spawn_grpc_server" paragraph — now wrong). Net: ~35L → ~20L.

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'spawn_grpc_server_layered' rio-test-support/src/grpc.rs` → ≥1 hit (T1)
- `grep 'spawn_grpc_server_layered' rio-store/tests/grpc/chunk_service.rs rio-store/tests/grpc/signing.rs` → ≥2 hits (T2+T3: both call sites migrated)
- `grep -c "can't use.*spawn_grpc_server\|Inlining the.*bind.*avoids\|Inlined server spawn" rio-store/tests/grpc/chunk_service.rs rio-store/tests/grpc/signing.rs` → 0 (T2+T3: "can't use shared helper" comments deleted)
- `grep -c 'bind.*127.0.0.1:0\|serve_with_incoming' rio-store/tests/grpc/chunk_service.rs` → 0 (T2: inline gone — chunk_service.rs:400's `spawn_grpc_server` call is the non-layered variant, stays)
- Note: `:400` uses `spawn_grpc_server` (not layered, no interceptor). T2 only touches `:147-173`. Re-grep at dispatch: `grep -n 'TcpListener::bind\|serve_with_incoming' rio-store/tests/grpc/chunk_service.rs` → 0 hits (both inlines were at `:153`/`:165`; `:400` already uses the helper).
- `cargo nextest run -p rio-store grpc::chunk_service:: grpc::signing::` — all existing tests green (behavior preserved)

## Tracey

No spec markers. Test-support helper extraction — not a spec'd behavior.

## Files

```json files
[
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T1: +spawn_grpc_server_layered<L> after :988 (generic over layer)"},
  {"path": "rio-store/tests/grpc/chunk_service.rs", "action": "MODIFY", "note": "T2: :147-173 inline bind/spawn/yield → spawn_grpc_server_layered call"},
  {"path": "rio-store/tests/grpc/signing.rs", "action": "MODIFY", "note": "T3: :195-229 spawn_store_with_fake_jwt body → helper call; delete :183-186 wrong-why comment"}
]
```

```
rio-test-support/src/grpc.rs         # T1: +layered helper
rio-store/tests/grpc/
├── chunk_service.rs                 # T2: migrate inline
└── signing.rs                       # T3: migrate inline
```

## Dependencies

```json deps
{"deps": [338], "soft_deps": [340, 311], "note": "Consolidator-mc90 flagged, P0338 added 2nd copy (mc=95). P0338 DONE. rio-test-support/src/grpc.rs count=19 (hot-ish, but additive after :988). P0340 (seed_tenant extraction, deps=[338]) touches signing.rs:265 — same file as T3, :195-229 vs :265 non-overlapping. Soft-sequence AFTER P0340 to avoid rebase-pain. P0311-T23 (batch_outputs_signed_with_tenant_key) + T26 (maybe_sign fallback test) both touch signing.rs — additive test fns, non-overlapping with T3's helper. discovered_from=consolidator."}
```

**Depends on:** [P0338](plan-0338-tenant-signer-wiring-putpath.md) — second inline copy at `signing.rs:195-229` arrived with it (DONE).

**Conflicts with:** [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) count=19 — T1 appends after `:988`; [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T19 adds `fail_batch_precondition` field to `MockStore` at `:48` — different section, non-overlapping. [P0340](plan-0340-extract-seed-tenant-test-helper.md) touches `signing.rs:265` — same file as T3, `:195-229` vs `:265`, non-overlapping but same-file rebase; sequence AFTER P0340 if both in-flight.
