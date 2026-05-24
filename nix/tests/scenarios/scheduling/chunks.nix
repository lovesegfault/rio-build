# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # chunks — 300KiB output forces chunked PutPath, not inline
  # ══════════════════════════════════════════════════════════════════
  #   300 KiB > INLINE_THRESHOLD (256 KiB) → nar_data.len() >= cas::
  #   INLINE_THRESHOLD at put_path.rs:494 is true → chunked path.
  #   chunk_after > chunk_baseline proves the threshold gate fired.
  #   The chunked PutPath increments rio_store_put_path_bytes_total
  #   (put_path.rs:574). bytes_after - bytes_before ≥ 300*1024 proves
  #   the volume counter runs on the chunked path (tiny text-file
  #   builds above probably went inline, so this is the first check
  #   that hits the counter inside the chunked branch).
  with subtest("chunks: 300KiB bigblob writes chunk files to disk"):
      # All builds above are tiny text files, likely inline — a
      # post-build `chunk_count > 0` check would NOT prove the chunked
      # path ran. Capture baseline, build bigblob (300 KiB >
      # INLINE_THRESHOLD = 256 KiB), assert chunk count increased.
      chunk_baseline = int(${gatewayHost}.succeed(
          "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
      ).strip())
      bytes_before = scrape_metrics(${gatewayHost}, 9092)

      build("${drvs.bigblob}")

      chunk_after = int(${gatewayHost}.succeed(
          "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
      ).strip())
      assert chunk_after > chunk_baseline, (
          f"bigblob (300 KiB) MUST write chunks to disk "
          f"(>INLINE_THRESHOLD). baseline={chunk_baseline}, "
          f"after={chunk_after} — chunk backend not wired, or "
          f"INLINE_THRESHOLD changed?"
      )

      # transfer-volume: bigblob is 300 KiB of zeros. NAR framing
      # adds a few hundred bytes of overhead. ≥300000 is a loose
      # floor — chunk dedup doesn't change what the upload RECEIVES.
      # Emitted by both the legacy PutPath commit and the
      # PutPathChunked commit (P0586), so the assertion holds
      # whichever RPC the builder picked for this fixture.
      bytes_after = scrape_metrics(${gatewayHost}, 9092)
      b_before = metric_value(bytes_before, "rio_store_put_path_bytes_total") or 0.0
      b_after = metric_value(bytes_after, "rio_store_put_path_bytes_total") or 0.0
      assert b_after - b_before >= 300000, (
          f"expected ≥300000 bytes delta for 300 KiB bigblob upload; "
          f"before={b_before}, after={b_after}, delta={b_after - b_before}"
      )

      # NOTE: the rio_store_chunk_dedup_ratio presence assertion that
      # used to live here is gone. Since P0586 the builder uploads via
      # PutPathChunked on chunk-backend stores (this fixture has one),
      # and that path never calls cas::put_chunked — the gauge is only
      # set by the legacy PutPath chunked-storage path and the
      # substitution ingest. The chunk-file-count delta above is the
      # structural proof that the chunk backend received this upload.
''
