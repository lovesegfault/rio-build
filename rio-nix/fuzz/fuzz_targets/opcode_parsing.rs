#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::wire;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    // Need at least 8 bytes for an opcode number
    if data.len() < 8 {
        return;
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        // Parse the first 8 bytes as the opcode number
        let opcode_bytes: [u8; 8] = data[..8].try_into().unwrap();
        let opcode_num = u64::from_le_bytes(opcode_bytes);

        // Remaining bytes serve as the opcode payload
        let payload = &data[8..];

        // Try to resolve the opcode; if unknown, that's fine — just skip
        let Some(op) = WorkerOp::from_u64(opcode_num) else {
            return;
        };

        // For each known opcode, attempt to parse the payload the way
        // a real handler would read its arguments from the wire.
        let mut cursor = Cursor::new(payload);

        match op {
            WorkerOp::IsValidPath => {
                // Reads a single store path string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::QueryPathInfo => {
                // Reads a single store path string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::QueryPathFromHashPart => {
                // Reads a single hash-part string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::AddToStore => {
                // Legacy CA import: name, camStr, refs (strings),
                // repair bool, then framed NAR source
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_strings(&mut cursor).await;
                let _ = wire::read_bool(&mut cursor).await;
                let _ = wire::read_framed_stream(&mut cursor).await;
            }
            WorkerOp::AddTextToStore => {
                // Legacy text import: name, text, refs
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_strings(&mut cursor).await;
            }
            WorkerOp::EnsurePath => {
                // Reads a single store path string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::AddTempRoot => {
                // Reads a single store path string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::QueryValidPaths => {
                // Reads a set of store path strings, then a bool
                let _ = wire::read_strings(&mut cursor).await;
                let _ = wire::read_bool(&mut cursor).await;
            }
            WorkerOp::SetOptions => {
                // SetOptions reads 12 u64 fields then string pairs (overrides)
                for _ in 0..12 {
                    let _ = wire::read_u64(&mut cursor).await;
                }
                let _ = wire::read_string_pairs(&mut cursor).await;
            }
            WorkerOp::BuildPaths => {
                // Reads a list of strings (derivation paths) and a build mode u64
                let _ = wire::read_strings(&mut cursor).await;
                let _ = wire::read_u64(&mut cursor).await;
            }
            WorkerOp::NarFromPath => {
                // Reads a single store path string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::AddSignatures => {
                // Reads a store path string and a list of signature strings
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_strings(&mut cursor).await;
            }
            WorkerOp::BuildDerivation => {
                // Reads a store path and then a complex derivation structure;
                // just try reading a string and some basic fields
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_strings(&mut cursor).await;
            }
            WorkerOp::AddToStoreNar => {
                // Reads a store path string and metadata
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_strings(&mut cursor).await;
            }
            WorkerOp::QueryMissing => {
                // Reads a list of store path strings
                let _ = wire::read_strings(&mut cursor).await;
            }
            WorkerOp::QueryDerivationOutputMap => {
                // Reads a single store path string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::RegisterDrvOutput => {
                // Reads output id + realisation info
                let _ = wire::read_string(&mut cursor).await;
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::QueryRealisation => {
                // Reads a single output id string
                let _ = wire::read_string(&mut cursor).await;
            }
            WorkerOp::AddMultipleToStore => {
                // Reads a bool (repair) and then NAR data
                let _ = wire::read_bool(&mut cursor).await;
                let _ = wire::read_u64(&mut cursor).await;
            }
            WorkerOp::BuildPathsWithResults => {
                // Reads a list of strings (derivation paths) and a build mode u64
                let _ = wire::read_strings(&mut cursor).await;
                let _ = wire::read_u64(&mut cursor).await;
            }
        }
    });
});
