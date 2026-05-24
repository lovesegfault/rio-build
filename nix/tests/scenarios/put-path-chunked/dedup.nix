# put-path-chunked subtest fragment — composed by
# scenarios/put-path-chunked.nix mkTest.
#
# Plan §P0586 scenario subtest (vi): a second build whose output shares
# a multi-chunk blob with the already-committed first build probes
# HasChunks, finds the blob chunks durable, and sends only the chunks
# the store does not have. The shared chunks gain a second manifest
# reference (refcount 2) instead of a second store object.
#
# Must run AFTER the roundtrip fragment (its chunks seed the dedup).
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # dedup — identical content in a second build is not re-uploaded
  # ══════════════════════════════════════════════════════════════════
  with subtest("chunked-dedup: shared chunks are referenced, not re-uploaded"):
      chunks_before = chunk_count()
      assert chunks_before > 0, "roundtrip must have committed chunks first"

      built = build("${dedupDrv}")
      assert built.startswith("/nix/store/"), f"unexpected build output: {built!r}"

      status = manifest_row(built, "m.status::text")
      assert status == "complete", (
          f"dedup output manifest status is {status!r}, expected complete"
      )

      # The blob bytes are identical to the roundtrip build's blob, so
      # every blob content chunk already exists. The second commit must
      # bump their refcount to 2 — the structural proof that the
      # builder's HasChunks probe excluded them from `novel` and the
      # store reused the existing rows instead of inserting new ones.
      # (Worker-side novel/deduped counters are unobservable here:
      # rio-builder is one-shot and its metrics reset on restart.)
      shared = int(psql(${gatewayHost},
          "SELECT count(*) FROM chunks WHERE refcount >= 2"))
      assert shared > 0, (
          "no chunk reached refcount >= 2 after a second build with "
          "identical blob content — dedup did not happen (every chunk "
          "was re-uploaded as novel)"
      )

      # The second build must not have re-created the shared content
      # chunks as new rows. It legitimately adds SOME rows (its own
      # framing runs + the marker file + any blob boundary chunks that
      # shift with the different tree), but strictly fewer than a
      # from-scratch upload of the same blob would: the blob alone is
      # ~349 KiB / 64 KiB avg = ~6 content chunks, all shared.
      chunks_after = chunk_count()
      added = chunks_after - chunks_before
      assert added < chunks_before, (
          f"second build added {added} chunk rows on top of {chunks_before} "
          "— that is a full re-upload, not a dedup"
      )

      # And the deduped output still reads back correctly through the
      # gateway: reassembly resolves the shared chunks from the CAS.
      blob_tail = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {built}/blob | "
          "tail -n 1"
      ).strip()
      assert blob_tail == "60000", (
          f"deduped blob corrupted: last line {blob_tail!r}, expected 60000"
      )
      marker = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {built}/marker"
      )
      assert "dedup-second-build" in marker, f"marker corrupted: {marker!r}"
''
