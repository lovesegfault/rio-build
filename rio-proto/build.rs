fn main() -> Result<(), Box<dyn std::error::Error>> {
    // prost-build 0.14 / tonic-prost-build 0.14 do NOT emit
    // `cargo:rerun-if-changed` for the compiled protos (the
    // build_with_config path drops it). Without an explicit directive
    // here, cargo's watch-everything default applies — which works, but
    // is silently disabled the moment ANY `rerun-if-changed` is emitted.
    // Make the dependency explicit so the rerun chain is robust to
    // future additions. Emitted first so a `?` early-return below can't
    // skip it.
    println!("cargo:rerun-if-changed=proto/");

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);

    // Emit the binary FileDescriptorSet alongside the generated Rust
    // modules. Re-exported from lib.rs as `FILE_DESCRIPTOR_SET` so
    // downstream build scripts (rio-test-support's MockAdmin codegen)
    // can decode it via a [build-dependencies] on rio-proto instead of
    // running their own protoc on `../rio-proto/proto/`. Single protoc
    // invocation point for the whole workspace.
    let mut b = tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(out_dir.join("file_descriptor_set.bin"));

    // ChunkData.data carries up to CHUNK_MAX (256 KiB) per message on
    // the GetChunks hot path. The moka chunk cache stores `Bytes`;
    // mapping the proto `bytes` field to `bytes::Bytes` instead of
    // `Vec<u8>` lets the handler hand the cached buffer to the encoder
    // without a copy. The 32-byte digest fields stay `Vec<u8>` — a
    // copy there is noise.
    b = b.bytes(".rio.types.ChunkData.data");

    // CompletionReport (~312B) dwarfs the other ExecutorMessage oneof
    // arms (~80B). Generated code; boxing would ripple through every
    // construction/match site for a stack-slot win we don't need on
    // this stream's hot path.
    b = b.type_attribute(
        "rio.types.ExecutorMessage.msg",
        "#[allow(clippy::large_enum_variant)]",
    );
    // Same shape for the pull outcome: WorkAssignment (~232B; ~256B
    // after P0588's input_roots/input_closure) dwarfs Gone (0B) /
    // NotYetReady (4B). One response per pod lifetime — boxing the
    // assignment arm would buy nothing.
    b = b.type_attribute(
        "rio.types.PullAssignmentResponse.outcome",
        "#[allow(clippy::large_enum_variant)]",
    );
    // SchedulerMessage retained for transitional clients; no-op once
    // the message is removed.
    b = b.type_attribute(
        "rio.types.SchedulerMessage.msg",
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
        "NodeSelectorTerm",
        "NodeSelectorRequirement",
        "GetSpawnIntentsResponse",
        "UpstreamInfo",
        "ListUpstreamsResponse",
        "DebugExecutorState",
        "DebugListExecutorsResponse",
        "ListSlaOverridesResponse",
        "SlaCandidateRow",
        "SlaExplainResponse",
        "SlaDefaultsResponse",
        "SlaTier",
        "SlaProbeShape",
        "GetSlaMispredictorsResponse",
        "SlaMispredictorEntry",
    ] {
        b = b.type_attribute(format!("rio.types.{ty}"), "#[derive(serde::Serialize)]");
    }
    // SeedCorpus/SeedEntry: Serialize + Deserialize so rio-cli can write
    // a typed corpus to disk and read it back for `import-corpus`
    // without depending on rio-scheduler's `prior::SeedCorpus`.
    // SlaStatusResponse (+ nested SlaOverride): Deserialize so xtask
    // gate_b can parse `rio-cli --json sla status` output back into the
    // typed struct and reach `DurationFit::t_at` via
    // `duration_fit_from_status` instead of re-deriving the curve
    // (bug_032).
    for ty in [
        "SeedCorpus",
        "SeedEntry",
        "SlaStatusResponse",
        "SlaOverride",
    ] {
        b = b.type_attribute(
            format!("rio.types.{ty}"),
            "#[derive(serde::Serialize, serde::Deserialize)]",
        );
    }
    for field in [
        "ClusterStatusResponse.uptime_since",
        "ExecutorInfo.resources",
        "ExecutorInfo.attempt_opened",
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
            // Castore Directory DAG (own package, no service).
            "proto/castore.proto",
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
