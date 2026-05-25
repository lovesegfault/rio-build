# put-path-chunked subtest fragment — composed by
# scenarios/put-path-chunked.nix mkTest.
#
# Plan §P0586 scenario subtests (i), (ii), and the positive half of (x):
# a real two-output build is committed atomically via PutPathChunked,
# both outputs round-trip through the gateway read path, a repeated
# chunk_manifest digest (two byte-identical files) commits cleanly, and
# the reference the builder scanned out of the output agrees with the
# store's independent rescan.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # roundtrip — 2-output build → PutPathChunked → servable
  # ══════════════════════════════════════════════════════════════════
  with subtest("chunked-roundtrip: two-output build commits via PutPathChunked"):
      built = build("${multiDrv}")
      assert built.startswith("/nix/store/"), f"unexpected build output: {built!r}"

      # Resolve both output paths from the store DB rather than from
      # nix-build stdout (whose multi-output print format is not a
      # contract). Non-empty ⇔ the output was registered at all.
      out_path = psql(${gatewayHost},
          "SELECT store_path FROM narinfo "
          "WHERE store_path LIKE '%-rio-chunked-multi'")
      dev_path = psql(${gatewayHost},
          "SELECT store_path FROM narinfo "
          "WHERE store_path LIKE '%-rio-chunked-multi-dev'")
      assert out_path.startswith("/nix/store/"), (
          f"out output not registered in narinfo: {out_path!r}"
      )
      assert dev_path.startswith("/nix/store/"), (
          f"dev output not registered in narinfo: {dev_path!r}"
      )

      # Both outputs committed atomically: status complete for both.
      for p in (out_path, dev_path):
          status = manifest_row(p, "m.status::text")
          assert status == "complete", (
              f"{p} manifest status is {status!r}, expected complete"
          )

      # The tiny single-file dev output has a chunk manifest. Every NAR
      # is chunked (P0583 dropped inline storage), so a complete
      # manifest with no manifest_data row would mean the commit txn
      # skipped the chunk-list write — an unreadable path.
      dev_chunked = psql(${gatewayHost},
          "SELECT count(*) FROM manifest_data md "
          "JOIN narinfo n USING (store_path_hash) "
          f"WHERE n.store_path = '{dev_path}'")
      assert dev_chunked == "1", (
          f"dev output has no manifest_data.chunk_list row ({dev_chunked})"
      )

      # Every chunk the commit produced is durable and not deleted —
      # the HasChunks durable-presence contract the dedup subtest
      # builds on.
      not_durable = psql(${gatewayHost},
          "SELECT count(*) FROM chunks WHERE NOT durable OR deleted")
      assert not_durable == "0", (
          f"{not_durable} chunks are not durable after a committed upload"
      )

      # Castore tables are written at commit time (not by a later
      # indexer pass): the directory DAG and per-file blobs of the
      # tree-shaped output are resolvable, which is what GetDirectory /
      # ReadBlob serve from.
      n_dirs = psql(${gatewayHost},
          "SELECT count(*) FROM directory_paths dp "
          "JOIN narinfo n USING (store_path_hash) "
          f"WHERE n.store_path = '{out_path}'")
      assert int(n_dirs) >= 3, (
          f"expected >=3 directory_paths rows for the tree output "
          f"(root, bin, share), got {n_dirs}"
      )
      n_blobs = psql(${gatewayHost},
          "SELECT count(*) FROM file_blobs fb "
          "JOIN narinfo n USING (store_path_hash) "
          f"WHERE n.store_path = '{out_path}'")
      # tool + blob + (share/a == share/b → one digest) = 3 distinct
      # file digests. The repeated digest for the two identical files
      # is subtest (ii): the commit must not double-insert or reject.
      assert n_blobs == "3", (
          f"expected 3 distinct file_blobs for the tree output, got {n_blobs}"
      )

      # Reference detection end-to-end: bin/tool embeds the busybox
      # store path; the builder fused-walk refscan declares it, the
      # store independently rescans the reconstructed NAR and agrees,
      # and narinfo carries it.
      refs = psql(${gatewayHost},
          "SELECT array_to_string(\"references\", ',') FROM narinfo "
          f"WHERE store_path = '{out_path}'")
      assert "${common.busybox}" in refs, (
          f"output references {refs!r} must include the busybox input "
          "(embedded in the tool shebang)"
      )

      # Read path: both outputs are servable back through the gateway.
      # The store reassembles the NAR from manifest_data.chunk_list
      # (interleaved framing + content chunks) — wrong framing or a
      # misordered chunk list yields garbage here.
      tool = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {out_path}/bin/tool"
      )
      assert "chunked-tool" in tool, f"tool content corrupted: {tool!r}"
      dev = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {dev_path}"
      )
      assert "chunked-dev-marker" in dev, f"dev content corrupted: {dev!r}"
      blob_tail = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {out_path}/blob | "
          "tail -n 1"
      ).strip()
      assert blob_tail == "60000", (
          f"multi-chunk blob corrupted: last line {blob_tail!r}, expected 60000"
      )
''
