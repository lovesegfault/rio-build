//! Generate the default-stub arms of `MockAdmin`'s `AdminService` impl.
//!
//! PROBLEM (Bundle E / InspectBuildDag): adding an RPC to `admin.proto`
//! breaks `MockAdmin`'s trait impl — it's missing the new method. The
//! fix is always the same rote stub: `Ok(Response::new(Default::default()))`.
//! Before this build.rs, that stub was hand-written per-RPC and the
//! grep-hunt ("which mock broke?") happened at compile time.
//!
//! APPROACH: run protoc with `--descriptor_set_out` on `admin.proto`,
//! decode the binary `FileDescriptorSet` with `prost-types`, iterate
//! `service[].method[]` for name/input_type/output_type/server_streaming
//! as struct fields (same parser rio-proto uses — single source of truth).
//! Emit a `macro_rules!` definition wrapping one fn body per unary RPC.
//! `src/grpc.rs` `include!`s the macro def at module level, then calls it
//! inside the `impl AdminService for MockAdmin` block. (`include!` itself
//! can't sit in impl-item position — "non-impl item macro in impl item
//! position" — but a macro_rules expansion can.)
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
use std::process::Command;

use heck::{ToSnakeCase, ToUpperCamelCase};
use prost::Message;
use prost_types::FileDescriptorSet;

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
    let proto_dir = format!("{manifest}/../rio-proto/proto");
    let proto_file = format!("{proto_dir}/admin.proto");
    println!("cargo:rerun-if-changed={proto_file}");
    // admin.proto imports these — rerun if they change too (the
    // descriptor set includes them via --include_imports).
    for dep in ["types.proto", "dag.proto", "admin_types.proto"] {
        println!("cargo:rerun-if-changed={proto_dir}/{dep}");
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let descriptor_path = format!("{out_dir}/admin_descriptor.bin");

    // protoc via the PROTOC env var (set by the dev shell / nix build).
    // Same binary rio-proto's tonic-build uses — identical parse.
    // --include_imports: the descriptor set must contain the transitive
    // deps (types.proto etc.) or the service's input/output types won't
    // resolve. We only read admin.proto's FileDescriptorProto from the
    // set, but protoc needs the full graph to emit it.
    let protoc = std::env::var("PROTOC").expect("PROTOC env var (set by dev shell)");
    let status = Command::new(&protoc)
        .arg("--descriptor_set_out")
        .arg(&descriptor_path)
        .arg("--include_imports")
        .arg("-I")
        .arg(&proto_dir)
        .arg(&proto_file)
        .status()
        .unwrap_or_else(|e| panic!("spawn {protoc}: {e}"));
    assert!(status.success(), "protoc failed: {status}");

    let bytes = std::fs::read(&descriptor_path).expect("read descriptor set");
    let fds = FileDescriptorSet::decode(&*bytes).expect("decode FileDescriptorSet");

    // The set has one FileDescriptorProto per .proto file (admin.proto +
    // its imports). We want AdminService, which lives in admin.proto.
    let service = fds
        .file
        .iter()
        .flat_map(|f| &f.service)
        .find(|s| s.name() == "AdminService")
        .expect("AdminService in descriptor set");

    let mut generated_body = String::new();

    for m in &service.method {
        let proto_name = m.name();

        if MANUAL_METHODS.contains(&proto_name) {
            // Hand-written in grpc.rs — skip codegen.
            continue;
        }

        // This build.rs intentionally only generates unary stubs.
        // Server-streaming RPCs need an associated `type FooStream`
        // AND a stream body — both go in grpc.rs's MANUAL_METHODS
        // section. If a new streaming RPC isn't in MANUAL_METHODS,
        // fail loud here instead of generating a broken unary stub.
        assert!(
            !m.server_streaming(),
            "streaming RPC {proto_name} must be in MANUAL_METHODS \
             (add it there + write the associated type + stream body in grpc.rs)"
        );

        let snake = proto_name.to_snake_case();
        let rust_req = proto_type_to_rust(m.input_type());
        let rust_resp = proto_type_to_rust(m.output_type());

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
        !generated_body.is_empty(),
        "descriptor set has zero generated methods for AdminService — protoc/proto_dir wrong?"
    );

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
             }}\n"
        ),
    )
    .unwrap();
}

/// Map a descriptor type reference to the Rust path seen from `src/grpc.rs`.
///
/// Descriptor type names are fully-qualified with leading dot:
/// `.google.protobuf.Empty` → `()` (tonic special-case — the generated
/// trait signature uses `Request<()>`).
///
/// `.rio.types.Foo` → `types::Foo` with prost's ident normalization
/// applied. prost runs every message name through heck's
/// UpperCamelCase, which lowercases consecutive caps: `GCRequest` →
/// `GcRequest`, `GCProgress` → `GcProgress`. Already-normalized names
/// (`ClusterStatusResponse`) are idempotent under the transform.
fn proto_type_to_rust(proto_type: &str) -> String {
    // Descriptor form has leading dot: ".rio.types.Foo".
    let proto_type = proto_type.strip_prefix('.').unwrap_or(proto_type);
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
