fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/types.proto",
                "proto/scheduler.proto",
                "proto/worker.proto",
                "proto/store.proto",
                "proto/admin.proto",
            ],
            &["proto/"],
        )?;
    Ok(())
}
