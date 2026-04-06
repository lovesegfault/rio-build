fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                // All four data-type files share `package rio.types;` → prost
                // merges into one `rio.types.rs`. The file-level split (P0376)
                // is for plan-DAG collision tracking, not Rust-module
                // separation.
                "proto/types.proto",
                "proto/dag.proto",
                "proto/build_types.proto",
                "proto/admin_types.proto",
                // Service definition files (each a distinct package).
                "proto/scheduler.proto",
                "proto/builder.proto",
                "proto/store.proto",
                "proto/admin.proto",
            ],
            &["proto/"],
        )?;
    Ok(())
}
