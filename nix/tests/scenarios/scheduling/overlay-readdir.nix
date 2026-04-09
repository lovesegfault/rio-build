# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # overlay-readdir-correctness — does ls INSIDE a build see ALL files?
  # ══════════════════════════════════════════════════════════════════
  # fuse-direct above proves readdir() CAN fire. This probes whether
  # the per-build overlay (lowerdir=/nix/store:{fuse}) serves a
  # CORRECT listing. multifile.nix: dep has 5 files, consumer ls's it
  # with a cold overlay dcache (no child names previously looked up).
  # If overlay shortcuts via its own dcache, count<5 = correctness bug.
  with subtest("overlay-readdir-correctness: ls in build sees all files"):
      out = build("${drvs.multifile}", capture_stderr=False).strip()
      count = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {out}"
      ).strip()
      # count=5: overlay reads the FULL lower listing (correct). May
      #   or may not go through /dev/fuse — coverage tells us which.
      # count<5: overlay serves from dcache (H1). Correctness bug:
      #   builds that `ls` a FUSE-served dep see only names they've
      #   already touched. None of our tests would have caught this
      #   — they all cat known filenames.
      assert count == "5", (
          f"overlay readdir returned {count} entries, expected 5. "
          f"If <5: overlay serves from stale dcache (CORRECTNESS BUG). "
          f"If =5 but ops.rs readdir still 0: overlay reads lower "
          f"via a path that skips /dev/fuse (coverage gap only)."
      )
''
