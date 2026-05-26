# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # seed-faults — generate + push the faults split's inputs
  # ══════════════════════════════════════════════════════════════════
  #   big_f       24 MiB — the streaming subject for chunk-warm /
  #                        integrity-fail (>8 MiB threshold, 3× margin).
  #   eio_extra_1..7 64 KiB — never-read inputs whose fetches all fail
  #                        once the store is scaled away (six concurrent
  #                        reads trip the breaker, the seventh hits the
  #                        already-open breaker).
  #   eio-builder script  — a busybox-shebang script used as a drv's
  #                        BUILDER; its store-side chunk is taken offline
  #                        so the daemon's execve gets EIO from the
  #                        castore lower (eio-infra-retry).
  #   post_seed   64 KiB — fresh input for the post-mountd-restart build.
  with subtest("seed-faults: generate + push fault-injection inputs as vm-castore"):
      client.succeed(
          "dd if=/dev/urandom of=/tmp/castore-big-f.bin bs=1M count=24 2>/dev/null && "
          "for i in 1 2 3 4 5 6 7; do "
          "  dd if=/dev/urandom of=/tmp/castore-eio-extra-$i.bin bs=64k count=1 2>/dev/null; "
          "done && "
          "dd if=/dev/urandom of=/tmp/castore-post-seed.bin bs=64k count=1 2>/dev/null"
      )
      # The eio builder: unique content per run (the marker line) so its
      # file digest can never already be node-cached, executable so the
      # daemon can execve it directly as the derivation's builder.
      client.succeed(
          "rm -f /tmp/castore-eio-builder && "
          "echo '#!${busybox}/bin/sh' > /tmp/castore-eio-builder && "
          f"echo '# castore-eio-infra marker {int(time.time())}' >> /tmp/castore-eio-builder && "
          "echo 'echo eio-recovered > \"$out\"' >> /tmp/castore-eio-builder && "
          "chmod +x /tmp/castore-eio-builder"
      )

      b3_big_f = client_b3("/tmp/castore-big-f.bin")
      b3_eio_builder = client_b3("/tmp/castore-eio-builder")
      b3_post_seed = client_b3("/tmp/castore-post-seed.bin")

      seeded = add_and_push(
          "/tmp/castore-big-f.bin",
          "/tmp/castore-eio-extra-1.bin",
          "/tmp/castore-eio-extra-2.bin",
          "/tmp/castore-eio-extra-3.bin",
          "/tmp/castore-eio-extra-4.bin",
          "/tmp/castore-eio-extra-5.bin",
          "/tmp/castore-eio-extra-6.bin",
          "/tmp/castore-eio-extra-7.bin",
          "/tmp/castore-eio-builder",
          "/tmp/castore-post-seed.bin",
      )
      p_big_f = seeded[0]
      p_eio_extras = seeded[1:8]
      p_eio_builder = seeded[8]
      p_post_seed = seeded[9]

      assert_not_cached(b3_big_f, "big_f before any build")
      assert_not_cached(b3_eio_builder, "eio builder before any build")
      print(
          f"seed-faults: big_f={p_big_f} (b3 {b3_big_f[:12]}), "
          f"extras={len(p_eio_extras)}, eio_builder={p_eio_builder}, "
          f"post_seed={p_post_seed}"
      )
''
