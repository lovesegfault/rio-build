fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &["proto/rio/v1/agent.proto", "proto/rio/v1/raft.proto"],
            &["proto"],
        )?;
    Ok(())
}
