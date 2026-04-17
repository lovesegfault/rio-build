fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut b = tonic_prost_build::configure()
        .build_server(true)
        .build_client(true);

    // CompletionReport (~312B) dwarfs the other ExecutorMessage oneof
    // arms (~80B). Generated code; boxing would ripple through every
    // construction/match site for a stack-slot win we don't need on
    // this stream's hot path.
    b = b.type_attribute(
        "rio.types.ExecutorMessage.msg",
        "#[allow(clippy::large_enum_variant)]",
    );

    // Derive `serde::Serialize` on the admin-facing response types so
    // rio-cli can `serde_json::to_string_pretty(&resp)` directly instead
    // of hand-rolling per-subcommand `*Json` projection structs.
    // `prost_types::Timestamp` and the nested `ResourceUsage` don't impl
    // Serialize — `#[serde(skip)]` those fields rather than pulling in
    // prost-wkt-types workspace-wide.
    for ty in [
        "ClusterStatusResponse",
        "ExecutorInfo",
        "BuildInfo",
        "TenantInfo",
        "ListExecutorsResponse",
        "ListBuildsResponse",
        "SpawnIntent",
        "GetSpawnIntentsResponse",
        "UpstreamInfo",
        "ListUpstreamsResponse",
        "DebugExecutorState",
        "DebugListExecutorsResponse",
        "SlaOverride",
        "ListSlaOverridesResponse",
        "SlaStatusResponse",
        "SlaCandidateRow",
        "SlaExplainResponse",
    ] {
        b = b.type_attribute(format!("rio.types.{ty}"), "#[derive(serde::Serialize)]");
    }
    for field in [
        "ClusterStatusResponse.uptime_since",
        "ExecutorInfo.resources",
        "ExecutorInfo.last_heartbeat",
        "ExecutorInfo.connected_since",
        "BuildInfo.submitted_at",
        "BuildInfo.started_at",
        "BuildInfo.finished_at",
        "TenantInfo.created_at",
    ] {
        b = b.field_attribute(format!("rio.types.{field}"), "#[serde(skip)]");
    }

    b.compile_protos(
        &[
            // All four data-type files share `package rio.types;` → prost
            // merges into one `rio.types.rs`.
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
