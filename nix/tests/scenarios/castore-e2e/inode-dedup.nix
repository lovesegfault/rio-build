# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # inode-dedup — content-addressed inodes (ADR-022 §2.3)
  # ══════════════════════════════════════════════════════════════════
  # dedup_a and dedup_b are two store paths with byte-identical 1 MiB
  # payloads. Inside the build sandbox `stat -c %i` must return the SAME
  # inode for both (inodes keyed on file_digest, not path), and cmp must
  # agree on the bytes. Synchronous ssh-ng build — the in-script checks
  # are the assertion; afterwards the host must hold exactly one cache
  # entry for the shared payload digest.
  with subtest("inode-dedup: identical content under two paths shares one inode"):
      out_ino = build(
          "${inoDrv}",
          extra_args=(
              f"--arg dedupA '(builtins.storePath {p_dedup_a})' "
              f"--arg dedupB '(builtins.storePath {p_dedup_b})'"
          ),
      )
      print(f"inode-dedup: build output {out_ino}")

      # One payload digest ⇒ one backing-cache entry, sized 1 MiB. There
      # cannot be a second entry by construction (both paths share the
      # digest), so existence + size is the whole probe.
      assert_cached(b3_payload, "dedup payload", timeout=120)
      size = int(k3s_agent.succeed(f"stat -c %s {cache_path(b3_payload)}").strip())
      assert size == 1024 * 1024, (
          f"inode-dedup: cached payload is {size} bytes, expected 1 MiB"
      )
      print("inode-dedup PASS: same inode, same bytes, single cache entry")
''
