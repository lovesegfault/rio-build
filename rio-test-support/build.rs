//! Generate the default-stub arms of `MockAdmin`'s `AdminService` impl.
//!
//! PROBLEM (Bundle E / InspectBuildDag): adding an RPC to `admin.proto`
//! breaks `MockAdmin`'s trait impl — it's missing the new method. The
//! fix is always the same rote stub: `Ok(Response::new(Default::default()))`.
//! Before this build.rs, that stub was hand-written per-RPC and the
//! grep-hunt ("which mock broke?") happened at compile time.
//!
//! APPROACH: regex-parse `admin.proto` (proto3 `rpc` syntax is stable),
//! emit a `macro_rules!` definition wrapping one `async fn` body per
//! unary RPC. `src/grpc.rs` `include!`s the macro def at module level,
//! then calls it inside the `impl AdminService for MockAdmin` block.
//! (`include!` itself can't sit in impl-item position — "non-impl
//! item macro in impl item position" — but a macro_rules expansion can.)
//!
//! The macro emits the PRE-DESUGARED `fn -> Pin<Box<Future>>` form,
//! not `async fn`: `#[tonic::async_trait]` on the impl block runs at
//! the AST level BEFORE macro expansion, so it would never see the
//! macro's `async fn`s. A plain `fn` with the exact lifetime-bounded
//! shape async-trait produces is passed through untouched — async-trait
//! only rewrites on the `async` keyword.
//!
//! The four methods with non-default behavior (streaming RPCs + the two
//! call-recording unaries) are skipped via `MANUAL_METHODS` and stay
//! hand-written in grpc.rs. New default-stub RPCs need zero Rust
//! changes; new streaming/recording RPCs need MANUAL_METHODS growth +
//! a hand-written body (still less work than before).

use std::fmt::Write as _;

use heck::{ToSnakeCase, ToUpperCamelCase};
use regex::Regex;

/// Proto method names (PascalCase) whose MockAdmin impls are NOT
/// generated — they stay hand-written in `src/grpc.rs`. Streaming
/// RPCs need associated types + a non-empty stream body (CLI drain
/// loop expects an `is_complete=true` frame); the two custom unaries
/// record calls / echo request fields so smoke tests can assert on
/// pass-through.
const MANUAL_METHODS: &[&str] = &[
    "GetBuildLogs", // streaming — sends one is_complete=true chunk
    "TriggerGC",    // streaming — sends one is_complete=true frame
    "ClearPoison",  // records drv_hash to clear_poison_calls
    "CreateTenant", // echoes tenant_name back in TenantInfo
];

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let proto = format!("{manifest}/../rio-proto/proto/admin.proto");
    println!("cargo:rerun-if-changed={proto}");

    let content = std::fs::read_to_string(&proto).unwrap_or_else(|e| panic!("read {proto}: {e}"));

    // proto3 rpc: `rpc Name(ReqType) returns ([stream] RespType);`
    // Comments (`//`) are fine — they never contain a line starting
    // with `rpc ` followed by the full signature shape.
    let re =
        Regex::new(r"rpc\s+(\w+)\s*\(\s*([\w.]+)\s*\)\s*returns\s*\(\s*(stream\s+)?([\w.]+)\s*\)")
            .unwrap();

    let mut all_methods = Vec::new();
    let mut generated_body = String::new();

    for cap in re.captures_iter(&content) {
        let proto_name = &cap[1];
        let req_type = &cap[2];
        let is_stream = cap.get(3).is_some();
        let resp_type = &cap[4];

        all_methods.push(proto_name.to_string());

        if MANUAL_METHODS.contains(&proto_name) {
            // Hand-written in grpc.rs — skip codegen but keep in METHODS.
            continue;
        }

        // This build.rs intentionally only generates unary stubs.
        // Server-streaming RPCs need an associated `type FooStream`
        // AND a stream body — both go in grpc.rs's MANUAL_METHODS
        // section. If a new streaming RPC isn't in MANUAL_METHODS,
        // fail loud here instead of generating a broken unary stub.
        assert!(
            !is_stream,
            "streaming RPC {proto_name} must be in MANUAL_METHODS \
             (add it there + write the associated type + stream body in grpc.rs)"
        );

        let snake = proto_name.to_snake_case();
        let rust_req = proto_type_to_rust(req_type);
        let rust_resp = proto_type_to_rust(resp_type);

        // Emit the PRE-DESUGARED form — what `#[async_trait]`
        // produces. `#[tonic::async_trait]` on the impl block runs
        // BEFORE macro expansion, so `async fn` inside this macro would
        // never get the rewrite and fail with "lifetime parameters do
        // not match". A plain `fn` returning Pin<Box<Future>> is passed
        // through untouched by the attribute, which only looks for the
        // `async` keyword.
        //
        // Shape mirrors async-trait 0.1's desugaring exactly (lifetime
        // names `'life0`/`'async_trait`, bounds, Box::pin wrapper).
        writeln!(
            generated_body,
            "        fn {snake}<'life0, 'async_trait>(\n\
             \x20           &'life0 self,\n\
             \x20           _: tonic::Request<{rust_req}>,\n\
             \x20       ) -> ::core::pin::Pin<\n\
             \x20           ::std::boxed::Box<\n\
             \x20               dyn ::core::future::Future<\n\
             \x20                       Output = ::std::result::Result<\n\
             \x20                           tonic::Response<{rust_resp}>,\n\
             \x20                           tonic::Status,\n\
             \x20                       >,\n\
             \x20                   > + ::core::marker::Send\n\
             \x20                   + 'async_trait,\n\
             \x20           >,\n\
             \x20       >\n\
             \x20       where\n\
             \x20           'life0: 'async_trait,\n\
             \x20           Self: 'async_trait,\n\
             \x20       {{\n\
             \x20           ::std::boxed::Box::pin(async move {{\n\
             \x20               Ok(tonic::Response::new(<{rust_resp}>::default()))\n\
             \x20           }})\n\
             \x20       }}\n"
        )
        .unwrap();
    }

    assert!(
        !all_methods.is_empty(),
        "parsed zero RPCs from {proto} — regex or file path broken"
    );

    // METHODS: proto-canonical PascalCase names. Covers ALL RPCs (manual
    // + generated) so xtask regen mocks sees the full list. Consumers
    // convert per-target: heck snake_case for Rust trait-method names,
    // lowercase-first-char for connect-es localName (NOT heck
    // lowerCamelCase — TriggerGC → triggerGC, not triggerGc).
    let methods_lit = all_methods
        .iter()
        .map(|m| format!("{m:?}"))
        .collect::<Vec<_>>()
        .join(", ");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(
        format!("{out_dir}/mock_admin_generated.rs"),
        format!(
            "// @generated by rio-test-support/build.rs from admin.proto\n\
             // include!()-ed at module level in src/grpc.rs (before the\n\
             // impl AdminService block — macro_rules is textually scoped).\n\
             \n\
             /// Expands to the default-stub unary method bodies inside\n\
             /// `impl AdminService for MockAdmin`. `include!` can't sit in\n\
             /// impl-item position; a macro_rules expansion can.\n\
             macro_rules! mock_admin_default_methods {{\n\
             \x20   () => {{\n\
             {generated_body}\
             \x20   }};\n\
             }}\n\
             \n\
             impl MockAdmin {{\n\
             \x20   /// Proto-canonical PascalCase RPC names from `admin.proto`.\n\
             \x20   /// Covers ALL AdminService methods (manual + generated).\n\
             \x20   ///\n\
             \x20   /// `cargo xtask regen mocks` reads this and writes\n\
             \x20   /// `rio-dashboard/src/test-support/admin-methods.json`\n\
             \x20   /// with connect-es lowerCamelCase names.\n\
             \x20   pub const METHODS: &[&str] = &[{methods_lit}];\n\
             }}\n"
        ),
    )
    .unwrap();
}

/// Map a proto type reference to the Rust path seen from `src/grpc.rs`.
///
/// `google.protobuf.Empty` → `()` (tonic special-case — the generated
/// trait signature uses `Request<()>`).
///
/// `rio.types.Foo` → `types::Foo` with prost's ident normalization
/// applied. prost runs every message name through heck's
/// UpperCamelCase, which lowercases consecutive caps: `GCRequest` →
/// `GcRequest`, `GCProgress` → `GcProgress`. Already-normalized names
/// (`ClusterStatusResponse`) are idempotent under the transform.
fn proto_type_to_rust(proto_type: &str) -> String {
    if proto_type == "google.protobuf.Empty" {
        return "()".to_string();
    }
    // All admin.proto RPC types live in package rio.types (types.proto +
    // dag.proto + build_types.proto + admin_types.proto all share that
    // package; prost merges into one module — see rio-proto/src/lib.rs).
    let name = proto_type
        .strip_prefix("rio.types.")
        .unwrap_or_else(|| panic!("unhandled proto type namespace: {proto_type}"));
    format!("types::{}", name.to_upper_camel_case())
}
