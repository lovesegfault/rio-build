# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # setoptions-unreachable — ssh-ng NEVER sends wopSetOptions
  # ══════════════════════════════════════════════════════════════════
  # Regression guard for the ClientOptions helpers (handler/mod.rs).
  # Nix SSHStore OVERRIDES RemoteStore::setOptions() with an EMPTY
  # body — ssh-store.cc, unchanged since 088ef8175 ("ssh-ng: Don't
  # forward options to the daemon", 2018-03-05). Base-class
  # RemoteStore::initConnection calls setOptions(conn) via virtual
  # dispatch → SSHStore's no-op → wopSetOptions never hits the wire.
  # Confirmed in our pinned flake input at ssh-store.cc:81-88.
  #
  # The empty override is INTENTIONAL upstream (NixOS/nix#1713, #1935:
  # forwarding options broke shared builders). Consequence for rio:
  # ALL ClientOptions extraction is dead code for ssh-ng sessions.
  # --option flags below are dropped client-side before the SSH pipe
  # ever opens. opcodes_read.rs:226 info-log never fires.
  #
  # Upstream fix: NixOS/nix 32827b9fb (fixes #5600) replaces the
  # empty override with selective forwarding — BUT gated on the daemon
  # advertising a new set-options-map-only protocol feature.
  # rio-gateway does not advertise it. IF this assert flips: the
  # flake was bumped past 32827b9fb AND the gateway started
  # negotiating the feature. build_timeout()/max_silent_time() are
  # then LIVE and need auditing — notably build_timeout() keys on
  # alias "build-timeout" but Config::getSettings (configuration.cc
  # getSettings, !isAlias filter) emits canonical "timeout" on wire.
  #
  # Assertion scope: greps ALL rio-gateway journal since boot. Every
  # subtest before this one also went through ssh-ng:// (build()
  # helper). If ANY of them triggered wopSetOptions, we'd see it.
  # The explicit --option pass here is belt-and-suspenders — proves
  # the negative even under the most favorable client args.
  with subtest("setoptions-unreachable: ssh-ng --option is a no-op"):
      # cgroupDrv: sleep 3, succeeds. If already built (cgroup
      # subtest in core), this cache-hits and returns instantly —
      # which is FINE, the ssh-ng handshake still runs (initConnection
      # → setOptions virtual call) before the cache check.
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://${gatewayHost}' "
          "--option build-timeout 10 "
          "--option max-silent-time 5 "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${cgroupDrv} 2>&1"
      )

      # opcodes_read.rs:226 logs literal "wopSetOptions" at info on
      # every receipt. JSON log format, info+ filter → in journalctl.
      setopts_hits = ${gatewayHost}.succeed(
          "journalctl -u rio-gateway --no-pager | "
          "grep -c 'wopSetOptions' || true"
      ).strip()
      assert int(setopts_hits or "0") == 0, (
          f"wopSetOptions fired {setopts_hits}x — ssh-ng IS now "
          f"sending SetOptions. Either the flake nix input passed "
          f"32827b9fb AND rio-gateway advertises set-options-map-only, "
          f"or a non-ssh-ng path was taken. AUDIT before relying on "
          f"propagation: build_timeout() keys on 'build-timeout' but "
          f"wire sends canonical 'timeout' (getSettings !isAlias)."
      )
      print("setoptions-unreachable PASS: wopSetOptions never hit wire (empty SSHStore override)")
''
