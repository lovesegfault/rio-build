# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # chunks — 300KiB output writes multiple chunk files to disk
  # ══════════════════════════════════════════════════════════════════
  #   Every NAR is chunked; a 300 KiB output spans several FastCDC
  #   chunks (CHUNK_AVG = 64 KiB), so chunk_after > chunk_baseline
  #   proves the upload reached the filesystem chunk backend. The
  #   PutPath commit increments rio_store_put_path_bytes_total;
  #   bytes_after - bytes_before ≥ 300*1024 proves the volume counter
  #   runs at real upload volume.
  with subtest("chunks: 300KiB bigblob writes chunk files to disk"):
      # Capture baseline, build bigblob (300 KiB), assert chunk count
      # increased.
      chunk_baseline = int(${gatewayHost}.succeed(
          "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
      ).strip())
      bytes_before = scrape_metrics(${gatewayHost}, 9092)

      build("${drvs.bigblob}")

      chunk_after = int(${gatewayHost}.succeed(
          "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
      ).strip())
      assert chunk_after > chunk_baseline, (
          f"bigblob (300 KiB) MUST write chunks to disk. "
          f"baseline={chunk_baseline}, "
          f"after={chunk_after} — chunk backend not wired?"
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
