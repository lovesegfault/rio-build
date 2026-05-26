# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # seed-core — generate + push the core split's inputs
  # ══════════════════════════════════════════════════════════════════
  # All inputs are urandom-fresh per run (a node cache can never be warm
  # from a previous life):
  #   big_a    32 MiB  — > stream_threshold_bytes (8 MiB), the streaming
  #                      open / chunk-cache subject. Sized 4× threshold
  #                      instead of the plan's 100 MB: the property only
  #                      needs ">threshold" and this keeps the seed push
  #                      and fill inside the split's budget.
  #   head4k_a  4 KiB  — the first 4 KiB of big_a, pushed separately so
  #                      cold-read can cmp the streamed prefix against it.
  #   dedup_a/b 1 MiB  — identical bytes under two store-path names
  #                      (inode-dedup: same file_digest ⇒ same inode).
  #   meta_tree        — 5 dirs × 10 files for stat-dcache-absorbed.
  #   sent_cold/sent_dcache 4 KiB — sentinel inputs read AFTER a
  #                      script's interesting section (cold-read's
  #                      dd|cmp, stat-dcache's five traversals); their
  #                      node-cache appearance is the host-side
  #                      "script got past it" gate (build stdout is not
  #                      reliably visible in pod logs).
  #   coreutils (static) — GNU cp for the listxattr branch (xattr-copy).
  with subtest("seed-core: generate + push core inputs as vm-castore"):
      client.succeed(
          "dd if=/dev/urandom of=/tmp/castore-big-a.bin bs=1M count=32 2>/dev/null && "
          "dd if=/tmp/castore-big-a.bin of=/tmp/castore-head4k-a.bin bs=4096 count=1 2>/dev/null && "
          "dd if=/dev/urandom of=/tmp/castore-dedup-payload.bin bs=1M count=1 2>/dev/null && "
          "cp /tmp/castore-dedup-payload.bin /tmp/castore-dedup-a.bin && "
          "cp /tmp/castore-dedup-payload.bin /tmp/castore-dedup-b.bin && "
          "dd if=/dev/urandom of=/tmp/castore-sent-cold.bin bs=4096 count=1 2>/dev/null && "
          "dd if=/dev/urandom of=/tmp/castore-sent-dcache.bin bs=4096 count=1 2>/dev/null"
      )
      client.succeed(
          "mkdir -p /tmp/castore-meta-tree && "
          "for d in 1 2 3 4 5; do "
          "  mkdir -p /tmp/castore-meta-tree/d$d; "
          "  for f in 1 2 3 4 5 6 7 8 9 10; do "
          "    echo meta-$d-$f > /tmp/castore-meta-tree/d$d/f$f; "
          "  done; "
          "done"
      )
      META_FILES = 50
      META_DIRS = 6  # 5 subdirs + the tree root

      b3_big_a = client_b3("/tmp/castore-big-a.bin")
      b3_head4k = client_b3("/tmp/castore-head4k-a.bin")
      b3_payload = client_b3("/tmp/castore-dedup-payload.bin")
      b3_sent_cold = client_b3("/tmp/castore-sent-cold.bin")
      b3_sent_dcache = client_b3("/tmp/castore-sent-dcache.bin")

      (
          p_big_a,
          p_head4k,
          p_dedup_a,
          p_dedup_b,
          p_meta_tree,
          p_sent_cold,
          p_sent_dcache,
      ) = add_and_push(
          "/tmp/castore-big-a.bin",
          "/tmp/castore-head4k-a.bin",
          "/tmp/castore-dedup-a.bin",
          "/tmp/castore-dedup-b.bin",
          "/tmp/castore-meta-tree",
          "/tmp/castore-sent-cold.bin",
          "/tmp/castore-sent-dcache.bin",
      )

      # GNU coreutils (static): registered into the client store from the
      # closureInfo registration (the client VM closure does not carry it),
      # then pushed like any other tenant source.
      client.succeed("nix-store --load-db < ${coreutilsClosure}/registration")
      coreutils_paths = client.succeed(
          "cat ${coreutilsClosure}/store-paths"
      ).split()
      client.succeed(
          "nix copy --no-check-sigs --to 'ssh-ng://k3s-server' "
          + " ".join(coreutils_paths)
      )
      wait_nar_indexed(coreutils_paths)

      # None of the core inputs may be node-cached yet — the cold/warm
      # distinction below depends on it.
      assert_not_cached(b3_big_a, "big_a before any build")
      assert_not_cached(b3_payload, "dedup payload before any build")
      print(
          f"seed-core: big_a={p_big_a} (b3 {b3_big_a[:12]}), "
          f"head4k={p_head4k}, dedup={p_dedup_a}|{p_dedup_b}, "
          f"meta_tree={p_meta_tree} ({META_FILES} files/{META_DIRS} dirs), "
          "coreutils=${coreutilsStatic}"
      )
''
