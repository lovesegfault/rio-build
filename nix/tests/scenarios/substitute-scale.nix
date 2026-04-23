# Substitution → ComponentScaler closed loop (P4 of the substitute-
# relayering work).
#
# Proves the FULL chain the unit tests can't: 30-leaf cascade hits the
# store's per-replica admission gate → scheduler reports
# `substituting_derivations` → ComponentScaler's predictive `builders`
# signal counts it (P1 c3f32c4a) → desiredReplicas RISES (not falls) →
# GetLoad's `substitute_admission_utilization` reaches the CR status
# (P2 6760e2a5). Zero builder pods spawn — every leaf substitutes.
#
# Why a NEW scenario, not a componentscaler.nix subtest: that scenario's
# load is intentionally NON-substitutable (`sleep 120` leaves) to keep
# queued+running stable across the controller-restart subtest. A
# substitutable leaf-set drains in seconds — incompatible. Fixture also
# differs (jwtEnabled, store admission cap, no busybox seed).
#
# Why a NEW scenario, not a substitute.nix subtest: substitute.nix is the
# standalone fixture (no k3s → no ComponentScaler CR, no controller
# reconciler, no /scale subresource). The fake-upstream-cache mechanics
# (lines ~92-144 there) ARE fixture-agnostic and lifted here onto
# upstream_v6 (already in CoreDNS test-vms.server, already serving
# /srv on :8080 — see common.mkUpstreamNode).
#
# Tracey: ctrl.scaler.signal-substituting / store.substitute.admission /
# store.admin.get-load — verify markers at default.nix wiring per the
# convention.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore nsBuilders;

  rioCli = "${common.rio-workspace}/bin/rio-cli";
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  jwtKeys = import ../lib/jwt-keys.nix;

  # Same JWT-minting helper as substitute.nix — sign with the test seed
  # so grpcurl-direct SubmitBuild carries x-rio-tenant-token (jwtEnabled
  # → require_tenant() rejects tokenless SchedulerService calls).
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
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-subscale"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';

  # 30 instant leaves + collector. Same shape as componentscaler.nix's
  # slowFanout but `echo` not `sleep` — we WANT these to be cheap so
  # building them locally on upstream_v6 (512MB) for cache-seeding is
  # trivial. Output paths are input-addressed → identical drv on
  # upstream_v6's local build and the client's ssh-ng submit (same
  # busybox store path, same currentSystem) → identical out-paths →
  # substitutable. The collector forces all 30 into the DAG; it never
  # builds in this test (the leaves substitute, then the collector
  # would build, but we cancel via systemctl stop before that).
  subFanout = pkgs.writeText "sub-fanout.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mkLeaf = i: derivation {
        name = "rio-subscale-leaf-''${toString i}";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" "''${bb} echo subscale-leaf-''${toString i}-v1 > $out" ];
      };
      leaves = builtins.genList mkLeaf 30;
    in
    derivation {
      name = "rio-subscale-root";
      system = builtins.currentSystem;
      builder = sh;
      args = [ "-c" "''${bb} cat ''${toString leaves} > $out" ];
    }
  '';

  # Depth-50 linear chain. Each link cats the previous output → IA so
  # out-paths are deterministic and identical between upstream_v6's
  # local build and the client's submit. Proves
  # sched.substitute.eager-probe end-to-end: links 0..48 are seeded on
  # upstream_v6, the root (49) is NOT — so r[sched.merge.substitute-
  # topdown] does NOT prune (root unsubstitutable → fall through), the
  # full DAG is merged, and check_cached_outputs probes all 50 in one
  # FindMissingPaths → 49 enter Substituting in one merge-time burst.
  # Without eager-probe, only the depth-0 leaf is Ready at merge →
  # cascade promotes ~BECAME_IDLE_INLINE_CAP=4 per tick.
  chainDepth = 50;
  subChain = pkgs.writeText "sub-chain.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      link = i:
        derivation {
          name = "rio-subchain-''${toString i}";
          system = builtins.currentSystem;
          builder = sh;
          args = [
            "-c"
            (
              if i == 0 then
                "''${bb} echo subchain-seed-v1 > $out"
              else
                "''${bb} cat ''${link (i - 1)} > $out; ''${bb} echo ''${toString i} >> $out"
            )
          ];
        };
    in
    link ${toString (chainDepth - 1)}
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-substitute-scale";
  skipTypeCheck = true;
  # k3s bring-up ~240s + cache-seed ~20s + scale-up wait ≤90s + drain
  # ≤90s + chain seed ~10s + chain submit ≤120s + slack. The 30-leaf
  # substitute is serialized (admission=1) over a 200ms tc-netem
  # upstream → ~24s, so the controller's 10s reconcile tick observes
  # substituting>0 deterministically.
  globalTimeout = 1100 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
      withSeed = false;
    }}

    # ══════════════════════════════════════════════════════════════════
    # Fake upstream: build 30 leaves on upstream_v6, publish to /srv
    # ══════════════════════════════════════════════════════════════════
    # mkUpstreamNode already runs `python3 -m http.server 8080 -d /srv`.
    # Build subFanout LOCALLY on upstream_v6 (root → direct local store,
    # no daemon; 30× echo is <1s), sign every leaf, `nix copy --to
    # file:///srv`. Store pods resolve `upstream-v6` via the CoreDNS
    # test-vms.server hosts block (k3s-full.nix coredns-custom).
    #
    # `nix-build` (old CLI) needs no experimental features; `nix key`/
    # `nix store sign`/`nix copy` do — pass on the cmdline so we don't
    # need a fixture extension hook for upstream_v6's nix.conf.
    NIX = "nix --extra-experimental-features nix-command"

    # upstream_v6 (mkUpstreamNode) doesn't have busybox in its system
    # closure → present on the 9p-shared /nix/store but NOT in its local
    # nix DB → `builtins.storePath` rejects it. closureInfo's
    # `registration` is the nix-store --dump-db format; --load-db
    # registers it without needing the file itself registered.
    upstream_v6.succeed(
        "nix-store --load-db < ${common.busyboxClosure}/registration"
    )

    upstream_v6.succeed(
        f"{NIX} key generate-secret --key-name subscale-1 > /tmp/sec && "
        f"{NIX} key convert-secret-to-public < /tmp/sec > /tmp/pub"
    )
    cache_pubkey = upstream_v6.succeed("cat /tmp/pub").strip()
    assert cache_pubkey.startswith("subscale-1:"), cache_pubkey

    # Build all 30 leaves + root locally. nix-build prints root only;
    # `nix-store -qR --include-outputs` on the .drv gives every leaf
    # out-path. Filter to rio-subscale-leaf-* (excludes busybox + root).
    # `--option substituters` empty: upstream_v6 is v6-only + airgapped,
    # cache.nixos.org is unreachable and the retry backoff adds ~5s.
    upstream_v6.succeed(
        "nix-build --no-out-link --option substituters ''' "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${subFanout} 2>&1"
    )
    drv = upstream_v6.succeed(
        "nix-instantiate "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${subFanout} 2>/dev/null"
    ).strip()
    leaf_paths = [
        p for p in upstream_v6.succeed(
            f"nix-store -qR --include-outputs {drv}"
        ).splitlines()
        if "rio-subscale-leaf-" in p and not p.endswith(".drv")
    ]
    assert len(leaf_paths) == 30, (
        f"expected 30 leaf out-paths, got {len(leaf_paths)}: {leaf_paths!r}"
    )

    # Sign + publish leaves ONLY. Root deliberately NOT in the cache —
    # if it were, FindMissingPaths would short-circuit the whole DAG to
    # one substitute and `substituting_derivations` would never reach
    # the ~30 the scaler needs to act on. compression=none avoids xz.
    leaves_sh = " ".join(leaf_paths)
    upstream_v6.succeed(f"{NIX} store sign --key-file /tmp/sec {leaves_sh}")
    upstream_v6.succeed(
        f"{NIX} copy --no-check-sigs "
        f"--to 'file:///srv?compression=none' {leaves_sh}"
    )
    # Sanity: a narinfo landed and carries the test sig.
    h0 = leaf_paths[0].removeprefix("/nix/store/").split("-", 1)[0]
    narinfo = upstream_v6.succeed(f"cat /srv/{h0}.narinfo")
    assert "Sig: subscale-1:" in narinfo, narinfo

    # Positive control: a k3s NODE can reach the upstream via the same
    # FQDN the store pod will use. Without this, every substitute-miss
    # below is VACUOUS (DNS/route broke, not the admission gate). Node-
    # level not pod-level: rio-store image is distroless (no sh/wget);
    # CoreDNS test-vms.server resolves upstream-v6 for both. NetPol is
    # disabled in vmtest-full so node-reach ⇔ pod-reach.
    upstream_v6.wait_for_open_port(8080)
    k3s_server.succeed(
        f"curl -sf 'http://upstream-v6:8080/{h0}.narinfo' "
        "| grep -q 'Sig: subscale-1:'"
    )

    # ══════════════════════════════════════════════════════════════════
    # Tenant + upstream + SSH wiring
    # ══════════════════════════════════════════════════════════════════
    # rio-cli for both AdminService (scheduler) and StoreAdminService
    # (store). Port-forward the leader scheduler (9001) + store (9002);
    # service-hmac for the AdminService token gate (same pattern as
    # cli.nix).
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

    # Create tenant FIRST so sshKeySetupFor's gateway resolve_tenant
    # finds it (jwtEnabled → gateway mints x-rio-tenant-token from the
    # SSH key comment; unknown tenant → Unauthenticated on every
    # ssh-ng op). Capture UUID from the echo line: "tenant <name> (<uuid>)".
    out = cli("create-tenant subscale-tenant")
    m = re.search(r"\(([0-9a-f-]{36})\)", out)
    assert m, f"create-tenant didn't echo a UUID:\n{out}"
    tid = m.group(1)
    print(f"substitute-scale: tenant subscale-tenant uuid={tid}")
    tenant_jwt = k3s_server.succeed(f"${signJwt} {tid}").strip()

    # tenant_upstreams row. helm upstreamCaches only opens a CNP egress
    # rule (not used here — networkPolicy.enabled=false in vmtest-full);
    # the store's FindMissingPaths reads the DB row.
    out = cli(
        f"upstream add --tenant {tid} "
        "--url http://upstream-v6:8080 --priority 50 "
        f"--trusted-key '{cache_pubkey}' --sig-mode keep"
    )
    assert "added upstream http://upstream-v6:8080" in out, out

    # SSH key with tenant-name comment → gateway mints JWT for this
    # tenant on every ssh-ng op. Uses fixture.sshKeySetupFor (k3s
    # variant: patches the rio-gateway-ssh Secret + scale-bounce).
    ${fixture.sshKeySetupFor "subscale-tenant"}

    # Seed busybox into rio's store (subFanout's only build input).
    # Now (not via mkBootstrap withSeed) because the seed `nix copy`
    # goes through ssh-ng which jwtEnabled rejects until the tenant
    # exists + key comment matches.
    ${common.seedBusybox "k3s-server"}

    # ══════════════════════════════════════════════════════════════════
    # cr-baseline — scaler at min, status populated
    # ══════════════════════════════════════════════════════════════════
    # Precondition for the never-dropped-while-substituting assertion:
    # learnedRatio populated AND desiredReplicas == min == 1. If the
    # scaler is already >1 (residual load from bring-up), the "rises
    # above min" check below is VACUOUS.
    with subtest("cr-baseline: ComponentScaler at min before load"):
        k3s_server.wait_until_succeeds(
            "r=$(k3s kubectl -n ${nsStore} get componentscaler store "
            "  -o jsonpath='{.status.learnedRatio}') && "
            'test -n "$r"',
            timeout=60,
        )
        desired0 = kubectl(
            "get componentscaler store -o jsonpath='{.status.desiredReplicas}'",
            ns="${nsStore}",
        ).strip()
        # min=1 (extraValuesTyped). bring-up has zero scheduler load.
        assert desired0 == "1", (
            f"baseline desiredReplicas={desired0!r}, expected 1 (min). "
            f"Scale-up assertion below would be VACUOUS."
        )
        print(f"cr-baseline: desiredReplicas={desired0} ✓")

    # ══════════════════════════════════════════════════════════════════
    # substitute-cascade — 30-leaf submit → all substitute
    # ══════════════════════════════════════════════════════════════════
    with subtest("substitute-cascade: leaves substitute, scaler rises, no builders"):
        # ── Submit the 30-leaf DAG via gRPC SubmitBuild ───────────────
        # NOT via `nix-build --store ssh-ng://`: the gateway's
        # wopQueryMissing reports the leaves as substitutable → the
        # client's nix runs its OWN wopEnsurePath substitution loop
        # (store ingests all 30 store-side) BEFORE wopBuildDerivation
        # → scheduler receives root.drv with inputs already-present →
        # zero Substituting (observed: 90 consecutive (0,1) samples).
        # `--option substitute false` doesn't help — that's a client-
        # store option; the substitution decision is the REMOTE store's
        # answer to wopQueryMissing.
        #
        # gRPC-direct is the only deterministic way to make the
        # SCHEDULER own the substitution decision: hand it the 30 leaf
        # nodes, its dispatch.rs FindMissingPaths sees them
        # substitutable, enters Substituting — the state P1 wires into
        # the ComponentScaler signal.
        #
        # First: instantiate leaf .drvs on client + resolve each drv's
        # output path. expectedOutputPaths MUST be in the proto node:
        # the gateway's translate.rs populates it from the ATerm, but
        # grpcurl-direct bypasses that. Without it,
        # output_paths_probeable() (state/derivation.rs) is false →
        # BOTH merge-time check_cached_outputs and dispatch-time
        # batch_probe_cached_ready skip the node → FindMissingPaths is
        # never called for the leaves → straight to build (observed:
        # 30× rio-builder pods, zero narinfo GETs to upstream-v6).
        # Then `nix copy --derivation` the .drv closure so the store
        # has the ATerm bytes (worker reads them at build-dispatch).
        leaf_drvs = sorted(
            p for p in client.succeed(
                "nix-instantiate "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                "${subFanout} 2>/dev/null | xargs nix-store -qR"
            ).splitlines()
            if "rio-subscale-leaf-" in p and p.endswith(".drv")
        )
        assert len(leaf_drvs) == 30, leaf_drvs
        # drv → out-path. `nix-store -q --outputs` over N drvs is N
        # lines but order is the input order only by accident; print
        # explicit drv:out pairs so the mapping is unambiguous.
        leaf_outs = dict(
            l.split(" ", 1)
            for l in client.succeed(
                "for d in " + " ".join(leaf_drvs) + "; do "
                'echo "$d $(nix-store -q --outputs $d)"; done'
            ).splitlines()
        )
        assert sorted(leaf_outs.values()) == sorted(leaf_paths), (
            "client out-paths diverge from upstream_v6 — input-addressed "
            f"drv should be identical on both. client={leaf_outs} "
            f"upstream={leaf_paths}"
        )
        client.succeed(
            "nix copy --derivation --no-check-sigs "
            "--to 'ssh-ng://k3s-server' " + " ".join(leaf_drvs)
        )
        # 30 independent nodes, no edges. drvHash=drvPath (input-
        # addressed; gateway translate.rs:361 does the same). The
        # scheduler walks each node Ready → FindMissingPaths(out-path)
        # → store says substitutable → DerivationStatus::Substituting.
        nodes = [
            {"drvPath": d, "drvHash": d,
             "system": "${pkgs.stdenv.hostPlatform.system}",
             "outputNames": ["out"],
             "expectedOutputPaths": [leaf_outs[d]]}
            for d in leaf_drvs
        ]
        payload = json.dumps({"nodes": nodes, "edges": []})
        k3s_server.succeed(f"cat > /tmp/subscale-dag.json <<'EOF'\n{payload}\nEOF")

        # Re-resolve the leader port-forward HERE: sshKeySetupFor +
        # seedBusybox above ran ~30s of gateway-bounce + ssh-ng I/O
        # since pf_open; a leader flip in that window leaves the
        # forward pinned to a now-standby pod (every sample reads 0).
        pf_close("pf-sched")
        pf_open(leader_pod(), 19001, 9001, tag="pf-sched")

        # ── Slow the upstream so the cascade outlives one controller
        # reconcile tick (10s). Tiny NARs over the local vlan drain in
        # ~400ms — faster than ComponentScaler can observe
        # substituting_derivations>0 → desiredReplicas never moves.
        # 200ms egress delay × admission=1 (serial) → each
        # narinfo+NAR fetch ≈ 2 RTTs ≈ 800ms → 30 leaves ≈ 24s.
        # eth1 = the test vlan (eth0 is QEMU user-net). qdisc replace
        # is idempotent across re-runs; deleted post-cascade so the
        # narinfo-row-count psql_k8s further down isn't slowed.
        upstream_v6.succeed(
            "tc qdisc replace dev eth1 root netem delay 200ms"
        )

        # SubmitBuild stream stays open until the build completes
        # (WatchBuild events). systemd-run detaches so the poll loop
        # below samples concurrently; -max-time 120 bounds the stream.
        # `-d @` = read JSON from stdin (grpcurl has no `-d @file`);
        # sh -c for the redirect under systemd-run.
        k3s_server.succeed(
            "systemd-run --unit=subscale-submit "
            "sh -c "
            f"'${grpcurl} -plaintext -max-time 120 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            "-d @ "
            "localhost:19001 rio.scheduler.SchedulerService/SubmitBuild "
            "< /tmp/subscale-dag.json'"
        )

        # ── (1) poll (substituting, desired) from t=0 ────────────────
        # Single Python loop sampling rio-cli status + CR.status. With
        # netem 200ms + admission=1 the cascade lasts ~24s, so both
        # the 1s poll here AND the controller's 10s reconcile reliably
        # observe sub>0. prost-serde keeps Rust snake_case. The
        # STRUCTURAL proof of Substituting is the WatchBuild event
        # count below; sub_peak from this poll drives the (5)
        # monotone-non-decreasing check.
        samples = []
        load_seen = ""
        sub_peak = 0
        for tick in range(90):
            rc, raw = k3s_server.execute(
                f"{CLI_ENV}${rioCli} status --json 2>&1"
            )
            try:
                s = json.loads(raw[raw.find("{"):])
                sub = int(s["substituting_derivations"])
            except (ValueError, KeyError):
                print(f"substitute-cascade: tick={tick} cli rc={rc} "
                      f"raw={raw[:200]!r}")
                sub = -1
            sub_peak = max(sub_peak, sub)
            desired = int(kubectl(
                "get componentscaler store -o "
                "jsonpath='{.status.desiredReplicas}'",
                ns="${nsStore}",
            ).strip() or "0")
            lf = kubectl(
                "get componentscaler store -o "
                "jsonpath='{.status.observedLoadFactor}'",
                ns="${nsStore}",
            ).strip()
            if lf:
                load_seen = lf
            samples.append((sub, desired))
            # Exit once sub drained AFTER both the scaler reacted and
            # the poll caught sub>0. Don't gate on the subscale-submit
            # unit — grpcurl keeps the WatchBuild stream open past
            # build-complete (server-side half-close vs -max-time).
            if sub_peak > 0 and sub == 0 and desired > 1:
                break
            k3s_server.sleep(1)
        upstream_v6.succeed("tc qdisc del dev eth1 root || true")
        print(f"substitute-cascade: samples (sub,desired) = {samples}")
        print(f"substitute-cascade: sub_peak={sub_peak} (best-effort)")

        # ── (1 structural) scheduler entered Substituting ────────────
        # Count DERIVATION_EVENT_KIND_SUBSTITUTING in the WatchBuild
        # stream. The grpcurl JSON event is one-per-transition;
        # journalctl captures the systemd-run stdout. ==30 means EVERY
        # leaf went Ready→Substituting (not Ready→Queued). Structural
        # — independent of poll timing (ci-failure-patterns
        # "structural > retry > widen").
        sub_events = int(k3s_server.succeed(
            "journalctl -u subscale-submit --no-pager "
            "| grep -c DERIVATION_EVENT_KIND_SUBSTITUTING || true"
        ).strip() or "0")
        if sub_events != 30:
            print("=== subscale-submit unit ===")
            print(k3s_server.execute(
                "journalctl -u subscale-submit --no-pager -n 200"
            )[1])
            print("=== scheduler leader (last 80) ===")
            print(k3s_server.execute(
                f"k3s kubectl -n ${ns} logs {leader_pod()} --tail=80 2>&1"
            )[1])
        assert sub_events == 30, (
            f"WatchBuild reported {sub_events} SUBSTITUTING events, "
            f"expected 30 — scheduler did NOT enter Substituting for "
            f"every leaf. Either expectedOutputPaths missing from "
            f"SubmitBuild (output_paths_probeable()→false skips the "
            f"probe), OR FindMissingPaths returned "
            f"substitutable_paths=[] (tenant_id not propagated / "
            f"upstream unreachable). samples={samples}"
        )

        # ── (2) desiredReplicas rose above min=1 ─────────────────────
        max_desired = max(d for _, d in samples)
        assert max_desired > 1, (
            f"ComponentScaler never scaled store above min=1 during the "
            f"substitute cascade. P1 (signal-substituting) regressed: "
            f"decide.rs builders-signal isn't counting Substituting. "
            f"samples={samples}"
        )

        # ── (5) never dropped while substituting > 0 ─────────────────
        # Vacuous when the cascade drained faster than the poll caught
        # sub>0; meaningful on slower hardware / larger NARs.
        prev_d = samples[0][1]
        for sub, d in samples:
            if sub > 0:
                assert d >= prev_d, (
                    f"desiredReplicas DROPPED {prev_d}→{d} while "
                    f"substituting={sub} > 0. ComponentScaler scaled "
                    f"store DOWN mid-cascade — the P1 closed-loop "
                    f"regression. samples={samples}"
                )
                prev_d = d

        # ── (3) observedLoadFactor populated → GetLoad wiring ─────────
        # max(pg_util, substitute_admission_util) per componentscaler/
        # mod.rs effective_load(). NON-EMPTY proves the GetLoad fan-out
        # reached a store pod and the P2 admission-util field
        # deserialized into the CR.status pipeline. NOT asserted > 0:
        # 30 tiny NARs over a local vlan drain through the 1-permit
        # gate (200ms netem ⇒ ~6s serial) — still racing the
        # controller's 10s reconcile tick. A `> 0` check is a wall-clock race
        # (ci-failure-patterns "structural > retry > widen"); the
        # structural proof of P2 is unit-level (decide.rs::tests
        # max-of-two), and the e2e proof of the WIRE is field-present.
        # Same precedent as componentscaler.nix:223-231.
        assert load_seen != "", (
            "observedLoadFactor never populated — GetLoad fan-out "
            "didn't reach a store pod (cross-ns DNS / store-admin "
            "headless-svc), OR status_changed() never fired."
        )
        print(f"substitute-cascade: observedLoadFactor={load_seen} (wired) ✓")

        # ── (4) zero builder pods spawned ────────────────────────────
        # Every leaf was substitutable → scheduler never emitted a
        # SpawnIntent → controller never created a Job. The DAG is
        # leaves-only (no root) so the strict bound is 0. `kubectl
        # get pod` shows Running+Completed; any pod here means a leaf
        # was BUILT not substituted.
        builder_pods = kubectl(
            "get pod -l rio.build/pool --no-headers 2>/dev/null "
            "| wc -l",
            ns="${nsBuilders}",
        ).strip()
        assert int(builder_pods) == 0, (
            f"{builder_pods} builder pods spawned — leaves were BUILT, "
            f"not substituted. Either tenant_upstreams didn't apply "
            f"(JWT not propagated?) or FindMissingPaths returned "
            f"substitutable_paths=[]. Check store + scheduler logs."
        )
        print(f"substitute-cascade: builder pods={builder_pods} (==0) ✓")

        # ── all leaves landed in rio's store ─────────────────────────
        # Ingest proof — narinfo row count == 30. NOT LIKE '%.drv'
        # excludes the 30 .drv ATerms `nix copy --derivation` PutPath'd
        # earlier (store_path '%-rio-subscale-leaf-N.drv' also matches
        # the bare LIKE). psql_k8s execs into rio-postgresql-0.
        n = psql_k8s(
            k3s_server,
            "SELECT count(*) FROM narinfo "
            "WHERE store_path LIKE '%rio-subscale-leaf-%' "
            "AND store_path NOT LIKE '%.drv'",
        )
        assert n == "30", (
            f"expected 30 leaf narinfo rows, got {n} — some substitutes "
            f"failed or the cascade was cut short. samples={samples}"
        )

        # ── (6) no demote-to-cache-miss ──────────────────────────────
        # P2 widened retry so admission RESOURCE_EXHAUSTED is absorbed,
        # not surfaced as fetch-fail. dispatch.rs:847 logs this exact
        # line on the give-up branch.
        sched_logs = kubectl(
            f"logs {leader_pod()} --since=5m 2>&1 || true"
        )
        assert "demoting to cache-miss" not in sched_logs, (
            "scheduler demoted ≥1 substitute to cache-miss — the "
            "widened-retry (df9141f5) regressed, OR admission cap=1 "
            "+ 30 leaves exceeds SUBSTITUTE_FETCH_RETRIES × backoff."
        )
        print("substitute-cascade: no demote-to-cache-miss ✓")

    k3s_server.execute("systemctl stop subscale-submit 2>/dev/null || true")

    # ══════════════════════════════════════════════════════════════════
    # substitute-deep-chain — depth-50 linear chain → eager whole-DAG
    # ══════════════════════════════════════════════════════════════════
    # Tracey: sched.substitute.eager-probe — verify marker at the
    # default.nix wiring per the convention.
    with subtest("substitute-deep-chain: 49 links enter Substituting at merge"):
        # ── seed all 50 chain outputs on upstream_v6 ──────────────────
        # Same mechanism as the fanout seed above (busybox already
        # registered, signing key already at /tmp/sec). netem was
        # removed at the end of the cascade subtest.
        upstream_v6.succeed(
            "nix-build --no-out-link --option substituters ''' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${subChain} 2>&1"
        )
        chain_drv = upstream_v6.succeed(
            "nix-instantiate "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${subChain} 2>/dev/null"
        ).strip()
        # Seed links 0..48 ONLY — NOT the root (subchain-49). With the
        # root seeded, r[sched.merge.substitute-topdown] fires (chain
        # has edges → topdown runs → root substitutable → submission
        # pruned to 1 node → spawned_total delta=1, deterministically).
        # The eager-probe property under test is the FALL-THROUGH case:
        # root not substitutable → full DAG merged → check_cached_outputs
        # probes all 50 → 49 substitutable spawned in one merge burst.
        chain_root_out = upstream_v6.succeed(
            f"nix-store -q --outputs {chain_drv}"
        ).strip()
        chain_paths = [
            p for p in upstream_v6.succeed(
                f"nix-store -qR --include-outputs {chain_drv}"
            ).splitlines()
            if "rio-subchain-" in p and not p.endswith(".drv")
            and p != chain_root_out
        ]
        assert len(chain_paths) == ${toString (chainDepth - 1)}, (
            f"expected ${toString (chainDepth - 1)} non-root chain out-paths, "
            f"got {len(chain_paths)}: {chain_paths!r}"
        )
        chain_sh = " ".join(chain_paths)
        upstream_v6.succeed(f"{NIX} store sign --key-file /tmp/sec {chain_sh}")
        upstream_v6.succeed(
            f"{NIX} copy --no-check-sigs "
            f"--to 'file:///srv?compression=none' {chain_sh}"
        )
        # Seed sanity: 30 cascade leaves + 49 chain links = 79 narinfos.
        # `nix copy --to file:` is synchronous (local dir write) so no
        # race with SubmitBuild — this guards against future async drift.
        ni = int(upstream_v6.succeed("ls /srv/*.narinfo | wc -l").strip())
        assert ni == 79, f"/srv has {ni} narinfos, expected 79 (30+49)"

        # ── instantiate on client, build SubmitBuild payload ──────────
        # Same expectedOutputPaths population pattern as the cascade
        # subtest. Edges: link i depends on link i-1 (linear chain).
        chain_drvs = sorted(
            p for p in client.succeed(
                "nix-instantiate "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                "${subChain} 2>/dev/null | xargs nix-store -qR"
            ).splitlines()
            if "rio-subchain-" in p and p.endswith(".drv")
        )
        assert len(chain_drvs) == ${toString chainDepth}, chain_drvs
        chain_outs = dict(
            l.split(" ", 1)
            for l in client.succeed(
                "for d in " + " ".join(chain_drvs) + "; do "
                'echo "$d $(nix-store -q --outputs $d)"; done'
            ).splitlines()
        )
        # drv → its single inputDrv (the previous link). Depth-0 has no
        # rio-subchain inputDrv (only busybox).
        chain_deps = {}
        for d in chain_drvs:
            refs = [
                r for r in client.succeed(
                    f"nix-store -q --references {d}"
                ).splitlines()
                if "rio-subchain-" in r and r.endswith(".drv")
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
            f"cat > /tmp/subchain-dag.json <<'EOF'\n{payload}\nEOF"
        )

        # Re-resolve leader pf — same staleness concern as the cascade
        # subtest after the cascade ran (~90s of activity). Forward
        # metrics (9091) too: scheduler image is distroless (no curl
        # in-pod), so scrape via host-side port-forward.
        pf_close("pf-sched")
        leader = leader_pod()
        pf_open(leader, 19001, 9001, tag="pf-sched")
        pf_open(leader, 19091, 9091, tag="pf-sched-metrics")

        # ── (1) eager burst: substitute_spawned_total jumps by 49 at merge ──
        # Counter increments inside spawn_substitute_fetches at merge
        # time — NOT subject to WatchBuild stream serialization (gRPC
        # stream → grpcurl → journalctl trickles events serially even
        # when the actor dispatches all 49 in one burst, so an anchored
        # `grep -c` sees only the first 1-2 under gate contention).
        # Eager: one MergeDag burst → +49. Lazy: depth-50 chain at
        # ~1/tick → +1-4 at first sighting. cache_check_failures /
        # topdown_prune are diagnostic deltas printed on failure.
        m_before = scrape_metrics(k3s_server, 19091)
        spawned_before = metric_value(
            m_before, "rio_scheduler_substitute_spawned_total"
        ) or 0.0
        prune_before = metric_value(
            m_before, "rio_scheduler_topdown_prune_total"
        ) or 0.0
        ccf_before = metric_value(
            m_before, "rio_scheduler_cache_check_failures_total"
        ) or 0.0

        k3s_server.succeed(
            "systemd-run --unit=subchain-submit "
            "sh -c "
            f"'${grpcurl} -plaintext -max-time 180 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            "-d @ "
            "localhost:19001 rio.scheduler.SchedulerService/SubmitBuild "
            "< /tmp/subchain-dag.json'"
        )

        # Anchor: first SUBSTITUTING event in the WatchBuild stream —
        # proves MergeDag completed (absorbs SubmitBuild + merge-phase
        # latency under gate builder contention). The counter scrape
        # AFTER this point sees the full burst.
        k3s_server.wait_until_succeeds(
            "journalctl -u subchain-submit --no-pager "
            "  | grep -q DERIVATION_EVENT_KIND_SUBSTITUTING",
            timeout=90,
        )
        m_after = scrape_metrics(k3s_server, 19091)
        spawned_delta = (
            metric_value(m_after, "rio_scheduler_substitute_spawned_total")
            or 0.0
        ) - spawned_before
        prune_delta = (
            metric_value(m_after, "rio_scheduler_topdown_prune_total") or 0.0
        ) - prune_before
        ccf_delta = (
            metric_value(m_after, "rio_scheduler_cache_check_failures_total")
            or 0.0
        ) - ccf_before
        if spawned_delta < 45 or prune_delta != 0:
            print("=== scheduler leader (since=60s, merge/probe lines) ===")
            print(k3s_server.execute(
                f"k3s kubectl -n ${ns} logs {leader} --since=60s 2>&1 "
                "| grep -Ei 'find_missing|breaker|top-?down|check_cached"
                "|substitute|probe' | tail -60"
            )[1])
        # Root NOT seeded → topdown-prune MUST NOT fire. If it did,
        # check_available reported subchain-49 substitutable (a real
        # bug) or the seed-exclusion above is broken.
        assert prune_delta == 0, (
            f"rio_scheduler_topdown_prune_total delta={prune_delta} — "
            f"topdown pruned despite root not seeded on upstream"
        )
        assert spawned_delta >= 45, (
            f"eager-probe burst: rio_scheduler_substitute_spawned_total "
            f"delta={spawned_delta} at first event (expected ≥45; lazy "
            f"mode shows ~1-4). cache_check_failures Δ={ccf_delta}, "
            f"topdown_prune Δ={prune_delta}"
        )
        print(
            f"substitute-deep-chain: spawned_total Δ={spawned_delta} "
            f"topdown_prune Δ={prune_delta} ccf Δ={ccf_delta} ✓"
        )

        # ── (2) all 49 links entered Substituting ─────────────────────
        # The root is NOT substitutable (not seeded) so it will dispatch
        # to a builder; we don't wait for that — the eager-probe proof
        # is the 49-link merge-time verdict, not root completion. Poll
        # the WatchBuild stream until all 49 SUBSTITUTING events have
        # serialized through journalctl (admission=1 + tiny NARs ≈ 2s;
        # 60s slack), then assert exactly 49. Build cancelled after.
        k3s_server.wait_until_succeeds(
            "n=$(journalctl -u subchain-submit --no-pager "
            "  | grep -c DERIVATION_EVENT_KIND_SUBSTITUTING || true); "
            'test "$n" -ge ${toString (chainDepth - 1)}',
            timeout=60,
        )
        sub_n = int(k3s_server.succeed(
            "journalctl -u subchain-submit --no-pager "
            "| grep -c DERIVATION_EVENT_KIND_SUBSTITUTING || true"
        ).strip() or "0")
        build_n = int(k3s_server.succeed(
            "journalctl -u subchain-submit --no-pager "
            "| grep -c DERIVATION_EVENT_KIND_BUILDING || true"
        ).strip() or "0")
        assert sub_n == ${toString (chainDepth - 1)}, (
            f"{sub_n} SUBSTITUTING events, expected "
            f"${toString (chainDepth - 1)} — some chain links were NOT "
            f"probed at merge time"
        )
        # Only the unseeded root may dispatch; ≥2 means a LINK was
        # built instead of substituted. ≤1 (not ==1) — the root may
        # not have provisioned a builder yet at this point.
        assert build_n <= 1, (
            f"{build_n} BUILDING events — chain LINKS were dispatched, "
            f"not substituted (eager-probe regressed)"
        )
        print(f"substitute-deep-chain: {sub_n} substituted, {build_n} built ✓")

        # Stop the WatchBuild stream. The root's build is left in-flight
        # scheduler-side; harmless (test is ending, collectCoverage next).
        k3s_server.execute("systemctl stop subchain-submit 2>/dev/null || true")
        pf_close("pf-sched-metrics")

    pf_close("pf-sched")
    pf_close("pf-store")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
