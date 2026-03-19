//! `wopAddMultipleToStore` → `wopNarFromPath` through REAL FastCDC.
//!
//! The P0054 bug class. `wire_opcodes` tests for these opcodes passed
//! against `MockStore`; the real stack rejected the bytes.
//!
//! `MockStore` roundtrip: `HashMap::insert(bytes)` → `HashMap::get(bytes)`.
//! Byte-identical by construction — proves nothing.
//!
//! Real stack roundtrip: `wopAddMultipleToStore` unaligned frames,
//! gateway streams to `PutPath` gRPC, `StoreServiceImpl` runs FastCDC
//! (~64 KiB chunks), BLAKE3 each chunk, write to `MemoryChunkBackend`
//! plus manifest to PG; then `wopNarFromPath`, `GetPath` gRPC, read
//! manifest, fetch chunks in order, concatenate, sha256 verify whole
//! NAR, gateway writes raw bytes after `STDERR_LAST`. Byte-identical
//! by **correctness**.
//!
//! Adapts Lix `store/cache/test_substitute_truncated_nar.py` semantics
//! (integrity through a storage round-trip) to rio's wire surface.

use super::*;

// r[verify store.nar.reassembly]
// r[verify store.cas.fastcdc]
// r[verify gw.opcode.add-multiple.unaligned-frames]
// r[verify gw.opcode.nar-from-path.raw-bytes]
/// 3 paths via `wopAddMultipleToStore`, each >256 KiB (over
/// `INLINE_THRESHOLD`), read each back via `wopNarFromPath`. Bytes must
/// survive chunk → manifest → backend → reassembly byte-for-byte.
#[tokio::test(flavor = "multi_thread")]
async fn add_multiple_then_nar_from_path_byte_identical() -> TestResult {
    let mut stack = RioStack::new_chunked_ready().await?;

    // 3 paths at 512 KiB each — well over INLINE_THRESHOLD (256 KiB),
    // chunks into ~8 pieces at FastCDC's 64 KiB normal-size. Different
    // seeds so each path is distinct content (but the 7919-prime
    // generator means they share SOME chunks — incidental dedup).
    let paths: Vec<(String, Vec<u8>, [u8; 32])> = (0..3)
        .map(|i| {
            let (nar, hash) = make_large_nar(i, 512 * 1024);
            (test_store_path(&format!("func-nar-{i}")), nar, hash)
        })
        .collect();

    // Build the inner framed payload per the REAL Nix protocol
    // (Store::addMultipleToStore(Source &) in store-api.cc):
    //   [num_paths: u64]
    //   for each: ValidPathInfo (9 fields) + NAR as narSize PLAIN bytes
    // NAR is NOT nested-framed — addToStore(info, source) reads narSize
    // bytes from the already-framed outer stream.
    // (r[gw.opcode.add-multiple.unaligned-frames])
    let mut inner = wire_bytes![u64: paths.len() as u64];
    for (path, nar, hash) in &paths {
        let entry = wire_bytes![
            string: path.as_str(),
            string: "",                    // deriver
            string: &hex::encode(hash),
            strings: wire::NO_STRINGS,     // refs
            u64: 0,                        // regtime
            u64: nar.len() as u64,         // nar_size
            bool: false,                   // ultimate
            strings: wire::NO_STRINGS,     // sigs
            string: "",                    // ca
            raw: nar,                      // PLAIN bytes (not framed)
        ];
        inner.extend_from_slice(&entry);
    }

    wire_send!(&mut stack.stream;
        u64: 44,                           // wopAddMultipleToStore
        bool: false,                       // repair
        bool: true,                        // dontCheckSigs
        framed: &inner,
    );
    drain_stderr_until_last(&mut stack.stream).await?;

    // White-box: chunks actually landed in the backend. If this is zero,
    // the store took the inline-blob shortcut (narinfo.inline_blob NOT
    // NULL) and this test is NOT exercising reassembly. The exit
    // criterion demands real chunking.
    let chunk_backend = stack
        .chunk_backend
        .as_ref()
        .expect("new_chunked provides it");
    let chunk_count = chunk_backend.len();
    assert!(
        chunk_count > 0,
        "NARs >INLINE_THRESHOLD must be chunked; backend is empty — \
         store took inline shortcut and this test proves nothing"
    );
    // 3 paths × ~8 chunks each, minus dedup. Loose lower bound.
    assert!(
        chunk_count >= 3,
        "suspiciously few chunks for 1.5 MiB of NARs: {chunk_count}"
    );

    // White-box: manifests are chunked (inline_blob IS NULL).
    let chunked_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM manifests WHERE inline_blob IS NULL")
            .fetch_one(&stack.db.pool)
            .await?;
    assert_eq!(
        chunked_count, 3,
        "all 3 manifests should be chunked (inline_blob NULL)"
    );

    // Read each back via wopNarFromPath. Bytes MUST be identical —
    // round-tripped through FastCDC chunk → PG manifest →
    // MemoryChunkBackend → reassembly → sha256 whole-NAR verify.
    // If buffered() was buffer_unordered(), chunks would arrive scrambled
    // and this fails (different bytes, same length).
    for (path, original_nar, _) in &paths {
        wire_send!(&mut stack.stream;
            u64: 38,                       // wopNarFromPath
            string: path.as_str(),
        );
        // STDERR_LAST first, then RAW NAR bytes. No length prefix — Nix
        // client's copyNAR reads until the NAR's closing ')' sentinel.
        // (r[gw.opcode.nar-from-path.raw-bytes])
        drain_stderr_until_last(&mut stack.stream).await?;
        let mut received = vec![0u8; original_nar.len()];
        tokio::io::AsyncReadExt::read_exact(&mut stack.stream, &mut received).await?;
        assert_eq!(
            &received, original_nar,
            "NAR bytes for {path} must survive chunk+reassemble byte-for-byte"
        );
    }

    stack.finish().await;
    Ok(())
}

// r[verify store.nar.reassembly]
/// Single-path variant via `wopAddToStoreNar` (39) — the framed path
/// (not the unaligned-frames multi path). Same proof as above but through
/// the other write opcode. Smaller NAR (still over threshold).
#[tokio::test(flavor = "multi_thread")]
async fn add_single_then_nar_from_path_chunked() -> TestResult {
    let mut stack = RioStack::new_chunked_ready().await?;
    let path = test_store_path("func-nar-single");
    // 300 KiB — just over INLINE_THRESHOLD. Minimum viable chunking.
    let (nar, hash) = make_large_nar(99, 300 * 1024);

    add_to_store_nar(&mut stack.stream, &path, &nar, hash, &[]).await?;

    let chunk_backend = stack
        .chunk_backend
        .as_ref()
        .expect("new_chunked provides it");
    assert!(
        !chunk_backend.is_empty(),
        "300 KiB NAR > INLINE_THRESHOLD — must chunk"
    );

    wire_send!(&mut stack.stream; u64: 38, string: &path);
    drain_stderr_until_last(&mut stack.stream).await?;
    let mut received = vec![0u8; nar.len()];
    tokio::io::AsyncReadExt::read_exact(&mut stack.stream, &mut received).await?;
    assert_eq!(received, nar, "NAR must roundtrip through chunk+reassemble");

    stack.finish().await;
    Ok(())
}
