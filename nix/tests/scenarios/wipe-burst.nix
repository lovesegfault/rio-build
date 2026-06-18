# live_055(c) — the wipe + min-floor + deep-chain-burst corpus scenario.
#
# The incident shape, miniaturized and made deterministic: a WIPED
# store instance left a dead zero-progress claim (claim-then-no-first-
# byte; the owner pod is gone, so no heartbeats, no graceful abort, no
# progress evidence) on the HEAD of a deep runtime-reference chain,
# while the fleet sat at its MINIMUM floor (one replica). A burst of
# materialization walks then piled behind the head: every walk's
# closure contains the wedged path, so one dead claim gated the whole
# closure (94% of raced losses concentrated on the chain heads in the
# live capture).
#
# What this pins at deployment level (the unit batteries prove the
# arms; this proves the composition under chart-rendered config):
#
#   1. RECLAIM LATENCY <= TYPED WINDOW: the dead claim is taken over
#      by a competing walk within the chart-set stall window
#      (store.substituteStallSecs=60, the validate() floor) of its
#      eligibility — NOT at the 300s heartbeat reap. The completion
#      budget below (240s) sits strictly UNDER the 300s reap, so the
#      pre-live_055(a) tree (zero-progress claims invisible to every
#      takeover arm) fails this scenario structurally — the disclosed
#      pre-fix red: reclaim could only happen at updated_at+300s =
#      cushion(30) + 300 = T0+330 > the 240s budget.
#   2. HEAD-PATH-CANNOT-WEDGE > WINDOW: the burst drains — every
#      seeded link's manifest reaches status='complete' once the head
#      frees; the wedge cannot outlive the window by more than the
#      priced overhead (fallback-poll cadence + drain).
#   3. The raced burst PARKS instead of poll-racing (live_055(b)): the
#      walks blocked on the head subscribe (raced_parks_total > 0)
#      during the pre-eligibility window. The eligibility cushion (the
#      wedge row's updated_at is stamped now()+30s, so takeover arms
#      open at T0+90s) guarantees the first attempts find a held,
#      not-yet-reclaimable claim — they MUST park, never take over on
#      first contact.
#
# STRUCTURAL COUNTS ONLY (the N13 flake lesson, applied at birth):
# cascade entries — counter increments (stale_reclaimed{stall_reclaim}
# == 1, raced parks >= 1) and durable row evidence (stall_count == 1,
# manifests-complete counts) — never gauge samples.
#
# Chain shape: depth ${chainDepth}; link i's OUTPUT CONTAINS link
# (i-1)'s out-path string (echo, not cat) so the links form a RUNTIME
# reference chain — the executor's closure walk for ANY link reaches
# link-0 (the head). Links 0..N-2 are seeded+signed on upstream_v6;
# the root is NOT (so topdown-prune cannot collapse the DAG to one
# root substitution and the burst actually fans out).
#
# Tracey: markers at the default.nix wiring entry per the house rule —
# store.substitute.stale-reclaim+4 (the zero-progress takeover under
# deployment config) and store.substitute.raced-subscribe (the parked
# burst). This header is prose, never marker-bearing.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore;

  rioCli = "${common.rio-workspace}/bin/rio-cli";
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  jwtKeys = import ../lib/jwt-keys.nix;

  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );
  signJwt = pkgs.writeScript "sign-jwt" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-wipeburst"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';

  # Depth-24 RUNTIME-reference chain. Each link ECHOES the previous
  # link's out-path into its output (a path string → the reference
  # scanner registers it), so link i's runtime closure is links 0..i —
  # every materialization walk reaches the head. IA derivations →
  # identical out-paths between upstream_v6's local build and the
  # client's submit (same busybox store path, same currentSystem).
  chainDepth = 24;
  wipeChain = pkgs.writeText "wipe-chain.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      link = i:
        derivation {
          name = "rio-wipeburst-''${toString i}";
          system = builtins.currentSystem;
          builder = sh;
          args = [
            "-c"
            (
              if i == 0 then
                "''${bb} echo wipeburst-head-v1 > $out"
              else
                "''${bb} echo ''${link (i - 1)} > $out; ''${bb} echo ''${toString i} >> $out"
            )
          ];
        };
    in
    link ${toString (chainDepth - 1)}
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-wipe-burst";
  skipTypeCheck = true;
  # k3s bring-up ~240s + cache seed ~30s + tenant/ssh wiring ~60s +
  # merge ~60s + eligibility cushion 30s + stall window 60s + fallback
  # poll 15s + drain ~60s + slack. The completion budget inside the
  # script is 240s (MUST stay < the 300s heartbeat reap — that gap IS
  # the assertion, see the header).
  globalTimeout = 1000 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    import hashlib

    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
      withSeed = false;
    }}

    NIX = "nix --extra-experimental-features nix-command"

    # ── Upstream cache: build + sign + publish links 0..N-2 ──────────
    upstream_v6.succeed(
        "nix-store --load-db < ${common.busyboxClosure}/registration"
    )
    upstream_v6.succeed(
        f"{NIX} key generate-secret --key-name wipeburst-1 > /tmp/sec && "
        f"{NIX} key convert-secret-to-public < /tmp/sec > /tmp/pub"
    )
    cache_pubkey = upstream_v6.succeed("cat /tmp/pub").strip()
    assert cache_pubkey.startswith("wipeburst-1:"), cache_pubkey

    upstream_v6.succeed(
        "nix-build --no-out-link --option substituters ''' "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${wipeChain} 2>&1"
    )
    drv = upstream_v6.succeed(
        "nix-instantiate "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${wipeChain} 2>/dev/null"
    ).strip()
    link_paths = sorted(
        p for p in upstream_v6.succeed(
            f"nix-store -qR --include-outputs {drv}"
        ).splitlines()
        if "rio-wipeburst-" in p and not p.endswith(".drv")
        and not p.endswith("-rio-wipeburst-${toString (chainDepth - 1)}")
    )
    assert len(link_paths) == ${toString (chainDepth - 1)}, (
        f"expected ${toString (chainDepth - 1)} seeded link out-paths, "
        f"got {len(link_paths)}: {link_paths!r}"
    )
    links_sh = " ".join(link_paths)
    upstream_v6.succeed(f"{NIX} store sign --key-file /tmp/sec {links_sh}")
    upstream_v6.succeed(
        f"{NIX} copy --no-check-sigs "
        f"--to 'file:///srv?compression=none' {links_sh}"
    )
    # The runtime-reference chain MUST be visible in the published
    # narinfos (link 1 references link 0) — without it every walk is a
    # singleton and the head gates nothing (vacuity guard).
    head_path = next(
        p for p in link_paths if p.endswith("-rio-wipeburst-0")
    )
    h1 = next(
        p for p in link_paths if p.endswith("-rio-wipeburst-1")
    ).removeprefix("/nix/store/").split("-", 1)[0]
    ni1 = upstream_v6.succeed(f"cat /srv/{h1}.narinfo")
    assert head_path.removeprefix("/nix/store/") in ni1, (
        f"link-1 narinfo does not reference the head — the chain has "
        f"no runtime references (vacuous corpus):\n{ni1}"
    )

    upstream_v6.wait_for_open_port(8080)
    h0 = head_path.removeprefix("/nix/store/").split("-", 1)[0]
    k3s_server.succeed(
        f"curl -sf 'http://upstream-v6:8080/{h0}.narinfo' "
        "| grep -q 'Sig: wipeburst-1:'"
    )

    # ── Tenant + upstream + SSH wiring (the substitute-scale order) ──
    pf_open(leader_pod(), 19001, 9001, tag="pf-sched")
    pf_open("svc/rio-store", 19002, 9002, ns="${nsStore}", tag="pf-store")
    k3s_server.succeed(
        "k3s kubectl -n ${ns} get secret rio-service-hmac "
        "-o jsonpath='{.data.service-hmac\\.key}' | base64 -d "
        "> /tmp/service-hmac.key"
    )
    CLI_ENV = (
        "${common.covShellEnv}"
        "RIO_SCHEDULER_ADDR=localhost:19001 "
        "RIO_STORE_ADDR=localhost:19002 "
        "RIO_SERVICE_HMAC_KEY_PATH=/tmp/service-hmac.key "
    )

    def cli(args):
        return k3s_server.succeed(f"{CLI_ENV}${rioCli} {args} 2>&1")

    out = cli("create-tenant wipeburst-tenant")
    m = re.search(r"\(([0-9a-f-]{36})\)", out)
    assert m, f"create-tenant didn't echo a UUID:\n{out}"
    tid = m.group(1)
    tenant_jwt = k3s_server.succeed(f"${signJwt} {tid}").strip()

    out = cli(
        f"upstream add --tenant {tid} "
        "--url http://upstream-v6:8080 --priority 50 "
        f"--trusted-key '{cache_pubkey}' --sig-mode keep"
    )
    assert "added upstream http://upstream-v6:8080" in out, out

    ${fixture.sshKeySetupFor "wipeburst-tenant"}
    ${common.seedBusybox "k3s-server"}

    # ── min-floor: the store fleet sits at ONE replica ────────────────
    with subtest("min-floor: store at one replica"):
        n = kubectl(
            "get deploy rio-store -o jsonpath='{.spec.replicas}'",
            ns="${nsStore}",
        ).strip()
        assert n == "1", (
            f"store replicas={n!r}, expected the chart-default floor of 1 "
            f"— the min-floor face of this corpus row is gone"
        )

    # ── THE WEDGE: a wiped instance's dead zero-progress claim ───────
    # The post-wipe stranded-claim shape, planted directly as rows (no
    # kill-timing race): an 'uploading' manifest on the chain HEAD with
    # a live claim_id, claimed_by a pod that no longer exists,
    # claim_phase='downloading', and NULL progress evidence — claim-
    # then-no-first-byte. updated_at is stamped now()+30s (the
    # ELIGIBILITY CUSHION): takeover arms open at cushion + window =
    # T0+90s, so every burst attempt that lands within 90s of the
    # submit finds a held-but-not-yet-reclaimable claim and MUST park
    # (assertion 3); a future-dated heartbeat also pushes the 300s
    # reap to T0+330s, keeping the reap plane strictly outside the
    # 240s completion budget (assertion 1's separation).
    with subtest("wedge: plant the dead zero-progress claim on the head"):
        wedge_hex = hashlib.sha256(head_path.encode()).hexdigest()
        zero32 = "00" * 32
        psql_k8s(
            k3s_server,
            f"INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) "
            f"VALUES ('\\x{wedge_hex}', '{head_path}', '\\x{zero32}', 0)",
        )
        psql_k8s(
            k3s_server,
            f"INSERT INTO manifests (store_path_hash, status, claim_id, "
            f"claimed_by, claim_phase, updated_at) "
            f"VALUES ('\\x{wedge_hex}', 'uploading', gen_random_uuid(), "
            f"'wiped-pod-live055', 'downloading', now() + interval '30 seconds')",
        )
        held = psql_k8s(
            k3s_server,
            f"SELECT count(*) FROM manifests WHERE store_path_hash = "
            f"'\\x{wedge_hex}' AND status = 'uploading' AND claim_id IS NOT NULL",
        )
        assert held == "1", f"wedge row not planted: {held!r}"

    # ── The burst: submit the chain, walks pile behind the head ──────
    with subtest("burst: deep-chain submit fans out behind the wedge"):
        chain_drvs = sorted(
            p for p in client.succeed(
                "nix-instantiate "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                "${wipeChain} 2>/dev/null | xargs nix-store -qR"
            ).splitlines()
            if "rio-wipeburst-" in p and p.endswith(".drv")
        )
        assert len(chain_drvs) == ${toString chainDepth}, chain_drvs
        chain_outs = dict(
            l.split(" ", 1)
            for l in client.succeed(
                "for d in " + " ".join(chain_drvs) + "; do "
                'echo "$d $(nix-store -q --outputs $d)"; done'
            ).splitlines()
        )
        chain_deps = {}
        for d in chain_drvs:
            refs = [
                r for r in client.succeed(
                    f"nix-store -q --references {d}"
                ).splitlines()
                if "rio-wipeburst-" in r and r.endswith(".drv")
            ]
            chain_deps[d] = refs
        client.succeed(
            "nix copy --derivation --no-check-sigs "
            "--to 'ssh-ng://k3s-server' " + " ".join(chain_drvs)
        )
        nodes = [
            {"drvPath": d, "drvHash": d,
             "system": "${pkgs.stdenv.hostPlatform.system}",
             "outputNames": ["out"],
             "expectedOutputPaths": [chain_outs[d]]}
            for d in chain_drvs
        ]
        edges = [
            {"parentDrvPath": d, "childDrvPath": dep}
            for d, deps in chain_deps.items() for dep in deps
        ]
        payload = json.dumps({"nodes": nodes, "edges": edges})
        k3s_server.succeed(
            f"cat > /tmp/wipeburst-dag.json <<'EOF'\n{payload}\nEOF"
        )

        pf_close("pf-sched")
        leader = leader_pod()
        pf_open(leader, 19001, 9001, tag="pf-sched")
        store_pod = kubectl(
            "get pod -l app.kubernetes.io/name=rio-store "
            "-o jsonpath='{.items[0].metadata.name}'",
            ns="${nsStore}",
        ).strip()
        pf_open(store_pod, 19092, 9092, ns="${nsStore}", tag="pf-store-metrics")

        k3s_server.succeed(
            "systemd-run --unit=wipeburst-submit "
            "sh -c "
            f"'${grpcurl} -plaintext -max-time 600 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            "-d @ "
            "localhost:19001 rio.scheduler.SchedulerService/SubmitBuild "
            "< /tmp/wipeburst-dag.json'"
        )
        # Merge done + jobs dispatched (absorbs SubmitBuild latency
        # under gate builder contention).
        k3s_server.wait_until_succeeds(
            "journalctl -u wipeburst-submit --no-pager "
            "  | grep -q DERIVATION_EVENT_KIND_SUBSTITUTING",
            timeout=120,
        )

    # ── (3) the blocked walks PARK (live_055(b) at corpus scale) ─────
    # Counter entries, not gauge samples: any wake reason counts — the
    # park is observable the moment the first walk meets the held
    # claim. 90s wait << the 90s eligibility horizon: a park recorded
    # here happened against a claim no takeover arm could yet free, so
    # it cannot be takeover-laundered.
    with subtest("burst parks on the wedge instead of poll-racing"):
        k3s_server.wait_until_succeeds(
            "curl -sf http://localhost:19092/metrics "
            "| grep -q '^rio_store_substitute_raced_parks_total'",
            timeout=90,
        )
        m = scrape_metrics(k3s_server, 19092)
        parks = sum(
            m.get("rio_store_substitute_raced_parks_total", {}).values()
        )
        assert parks >= 1, (
            f"raced_parks_total={parks} — the burst poll-raced the "
            f"wedge (the live_055(b) subscription plane is not engaged)"
        )
        print(f"wipe-burst: raced parks = {parks} ✓")

    # ── (1) reclaim <= typed window: the head frees by TAKEOVER ──────
    # Budget derivation: eligibility cushion 30s + stall window 60s +
    # one fallback-poll slice 15s + head download/persist + budget-
    # exhaust re-dispatch slack ≈ 120s typical → 240s = 2× typical
    # (tail-budgeted, the ci-failure-patterns discipline) and
    # STRICTLY < the 300s heartbeat reap + 30s cushion — completion
    # inside this budget is only reachable through the zero-progress
    # takeover arm (the pre-live_055(a) red arithmetic, see header).
    with subtest("head reclaimed within the typed window"):
        k3s_server.wait_until_succeeds(
            f"""k3s kubectl -n rio-system exec rio-postgresql-0 -- """
            f"""env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc """
            f""""SELECT count(*) FROM manifests WHERE store_path_hash = """
            f"""'\\x{wedge_hex}' AND status = 'complete'" | grep -qx 1""",
            timeout=240,
        )
        strikes = psql_k8s(
            k3s_server,
            f"SELECT stall_count FROM manifests WHERE store_path_hash = "
            f"'\\x{wedge_hex}'",
        )
        assert strikes == "1", (
            f"wedge stall_count={strikes!r}, expected exactly 1 — the "
            f"head must be freed by EXACTLY ONE takeover (0 = it was "
            f"reaped/re-inserted, losing the strike plane; >1 = "
            f"spurious takeovers of the healthy successor claim)"
        )
        m = scrape_metrics(k3s_server, 19092)
        reclaims = metric_value(
            m, "rio_store_substitute_stale_reclaimed_total",
            labels='{reason="stall_reclaim"}',
        )
        assert reclaims == 1.0, (
            f"stale_reclaimed{{stall_reclaim}}={reclaims} — expected "
            f"exactly one takeover entry for the planted wedge"
        )
        print(f"wipe-burst: takeover entries = {reclaims}, strikes = {strikes} ✓")

    # ── (2) head-path-cannot-wedge>window: the burst drains ──────────
    # Every seeded link completes once the head frees (their closures
    # all contained it). The ROOT is excluded by path — the unseeded
    # root BUILDS on the k3s builder and may upload its own manifest
    # mid-wait (observed in the founding solo run), and the law here
    # is about the SEEDED links' closures.
    #
    # Budget 300s, priced: a walk that parked against the wedge and
    # exhausted its park budget (stall 60 + 2x15 fallback = 90s)
    # surfaced Raced to the scheduler's RetryLater plane — its NEXT
    # dispatch rides the scheduler's own retry cadence, which is
    # outside this scenario's plane and can stack one more park slice
    # on contention. 300s = park budget + re-dispatch tail + drain,
    # ~2.5x the observed solo tail. On timeout the per-link state
    # dumps so the failure carries data, not just a count.
    with subtest("burst drains: every seeded link completes"):
        root_out = chain_outs[next(
            d for d in chain_drvs
            if d.endswith("-rio-wipeburst-${toString (chainDepth - 1)}.drv")
        )]
        # NOT LIKE '%.drv': the client's `nix copy --derivation` lands
        # the .drv files as complete manifests under the same name
        # prefix (caught by the founding runs' diagnostics — the
        # unscoped count could never equal the link count).
        try:
            k3s_server.wait_until_succeeds(
                f"""k3s kubectl -n rio-system exec rio-postgresql-0 -- """
                f"""env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc """
                f""""SELECT count(*) FROM manifests m JOIN narinfo n """
                f"""USING (store_path_hash) WHERE n.store_path LIKE """
                f"""'%rio-wipeburst-%' AND n.store_path NOT LIKE '%.drv' """
                f"""AND n.store_path <> '{root_out}' """
                f"""AND m.status = 'complete'" """
                "| grep -qx ${toString (chainDepth - 1)}",
                timeout=300,
            )
        except Exception:
            print("=== wipe-burst drain timeout: per-link manifest state ===")
            print(psql_k8s(
                k3s_server,
                "SELECT n.store_path, m.status, m.claim_phase, m.stall_count "
                "FROM manifests m JOIN narinfo n USING (store_path_hash) "
                "WHERE n.store_path LIKE '%rio-wipeburst-%' "
                "ORDER BY n.store_path",
            ))
            print("=== wipe-burst drain timeout: materialization jobs ===")
            print(psql_k8s(
                k3s_server,
                "SELECT drv_hash, state, origin FROM materialization_jobs "
                "WHERE drv_hash LIKE '%rio-wipeburst-%' ORDER BY drv_hash",
            ))
            raise
        print("wipe-burst: all ${toString (chainDepth - 1)} links complete OK")

    # Root build left in-flight scheduler-side; the laws are proven.
    k3s_server.execute("systemctl stop wipeburst-submit 2>/dev/null || true")
    pf_close("pf-store-metrics")
    pf_close("pf-sched")
    pf_close("pf-store")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
