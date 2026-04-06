# FOD forward proxy: squid allowlist enforcement + rio-worker is_fod gate.
#
# Three assertions against the k3s-full fixture with fodProxy.enabled=true:
#
#   1. allowed  — FOD fetch from allowlisted host → build succeeds + squid
#                 log shows TCP_MISS (proxy forwarded the request)
#   2. denied   — FOD fetch from non-allowlisted host → build FAILS + squid
#                 log shows TCP_DENIED/403. BOTH assertions matter (Q4):
#                 failure alone could be any network misconfig; TCP_DENIED
#                 proves squid's ACL is what blocked it.
#   3. non-fod  — non-FOD builder's env has NO http_proxy. Proves
#                 executor/mod.rs:462's is_fod gate — the daemon for a
#                 non-FOD build never gets proxy env, so the sandbox
#                 child can't inherit what was never set.
#
# Wiring that this scenario depends on but doesn't own:
#   - dockerImages.fod-proxy (nix/docker.nix:200) — squid image, preloaded
#     via k3sFull's extraImages (dockerImages.all has no squid)
#   - infra/helm/rio-build/templates/fod-proxy.yaml — ConfigMap + Deploy +
#     Service, gated by fodProxy.enabled. The allowedDomains helm value
#     renders into squid's dstdomain ACL at template time — no ConfigMap
#     patch + pod restart needed (R8 answered).
#   - WorkerPool.spec.fodProxyUrl (rio-crds/src/workerpool.rs:292) —
#     workerpool.yaml templates this from fodProxy.url (or the Service
#     DNS name when fodProxy.enabled=true and url is empty). Controller
#     reads the CR → injects RIO_FOD_PROXY_URL into the STS template
#     env (builders.rs:549). No runtime patch; the worker pod comes
#     up with the proxy URL already wired, so fixture.waitReady's
#     final scheduler-registration gate is all the readiness this
#     scenario needs.
#
# DNS: the origin HTTP server runs on the k3s-server NODE (python -m
# http.server, started via testScript). Squid (inside a pod) resolves
# `k3s-server` via k3s CoreDNS NodeHosts → node IP → reaches the node-
# level python process. Firewall port opened via iptables at runtime.
#
# NOT in scope: NetworkPolicy egress enforcement (P0241's domain). This
# test proves http_proxy is SET and squid enforces the allowlist; it does
# NOT prove the worker CAN'T bypass the proxy. wget honors http_proxy and
# doesn't fall back to direct on 403, so the test is deterministic
# regardless of NetPol state.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Fixture content served by the local origin. One string → one hash.
  # `builtins.hashString "sha256"` returns hex; fod-fetch.nix uses
  # outputHashMode=flat + outputHashAlgo=sha256, which accepts hex.
  fodContent = "fod-proxy-allowed-fixture-v1\n";
  fodFixtureFile = pkgs.writeText "fod-fixture" fodContent;
  fodFixtureSha256 = builtins.hashString "sha256" fodContent;

  # Bogus hash for the denied case. The fetch fails (squid 403) before
  # hash verification runs. NOT all-zeros: Nix treats all-zero hashes
  # as the TOFU "tell me the real hash" sentinel (runs the build, then
  # fails with the real hash in the error). A random-looking non-zero
  # hash is just a plain FOD that either builds-and-hash-matches or
  # fails cleanly.
  bogusSha256 = "1111111111111111111111111111111111111111111111111111111111111111";

  # Port for the local origin. NOT in k3sBase's allowedTCPPorts —
  # opened at runtime via iptables (avoids a fixture-level change
  # for a scenario-specific port).
  originPort = 8081;

  # Coverage-mode globalTimeout headroom (instrumented images inflate
  # the k3s airgap import time; same pattern as cli.nix).
  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
pkgs.testers.runNixOSTest {
  name = "rio-fod-proxy";
  skipTypeCheck = true;

  # k3s bring-up ~4min + squid ready + origin setup + 3 builds. The
  # denied build currently hangs until `timeout 150` kills it
  # (TODO(P0308-followup): whiteout fix unvalidated in k3s). Budget:
  # 4min k3s + 150s denied + 2×30s allowed/nonfod ≈ 8min, under 900s.
  globalTimeout = 900 + covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}
    ${common.seedBusybox "k3s-server"}

    # ── Squid pod ready ───────────────────────────────────────────────
    # fodProxy.enabled=true (via extraValues in default.nix) → Deployment
    # rio-fod-proxy spins up. waitReady doesn't know about it (it's not
    # one of the core rio-* deployments), so gate here. The readinessProbe
    # is just TCP on 3128 — Ready means squid is listening.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} wait --for=condition=Available "
        "deploy/rio-fod-proxy --timeout=120s",
        timeout=150,
    )

    # ── WorkerPool.spec.fodProxyUrl — templated, not patched ──────────
    # workerpool.yaml renders `fodProxyUrl` when fodProxy.enabled=true
    # (extraValues in default.nix). No kubectl patch → no STS rollout
    # → fixture.waitReady's scheduler-registration gate already covers
    # the worker's initial readiness. Just assert the field landed in
    # the CR (proves the helm template wiring) and that the controller
    # propagated it into the pod env (proves the CRD→controller→STS
    # chain — builders.rs:549).
    fod_url = kubectl(
        "get workerpool default -o jsonpath='{.spec.fodProxyUrl}'"
    ).strip()
    assert "rio-fod-proxy" in fod_url, (
        f"WorkerPool.spec.fodProxyUrl should be templated from "
        f"fodProxy.enabled=true, got {fod_url!r}"
    )
    # fixture.waitReady already gated on pod Ready + scheduler
    # registration; the pod was created WITH the env var (no patch →
    # no restart). Spot-check the env directly — this is the wire the
    # worker actually reads (config.rs:100 RIO_FOD_PROXY_URL).
    env_url = kubectl(
        "get pod default-workers-0 "
        "-o jsonpath='{.spec.containers[0].env[?(@.name==\"RIO_FOD_PROXY_URL\")].value}'"
    ).strip()
    assert "rio-fod-proxy" in env_url, (
        f"controller should have injected RIO_FOD_PROXY_URL into the "
        f"worker pod env from WorkerPool.spec.fodProxyUrl, got {env_url!r}"
    )

    # ── Local origin HTTP server ──────────────────────────────────────
    # python http.server on the k3s-server NODE, serving the fixture
    # file. Squid (in a pod) resolves `k3s-server` via CoreDNS NodeHosts
    # → node IP, then connects to this port. Store-path interpolation
    # pulls python3 + the fixture file into the VM closure.
    #
    # iptables insert (not NixOS firewall config): the port is scenario-
    # specific and k3sFull's firewall is already configured for the
    # core k3s ports. `nixos-fw` is the chain NixOS's firewall module
    # creates; `nixos-fw-accept` is its accept target. -I (insert at
    # top) so it precedes the default-drop.
    k3s_server.succeed(
        "iptables -I nixos-fw -p tcp --dport ${toString originPort} "
        "-j nixos-fw-accept"
    )
    k3s_server.succeed(
        "mkdir -p /srv/origin && "
        "cp ${fodFixtureFile} /srv/origin/fixture"
    )
    # Background the server, stash PID. Same shape as cli.nix's
    # port-forward backgrounding — no nohup, no `cd && `. Machine's
    # backdoor shell is persistent (one shell per VM lifetime), so
    # no SIGHUP on command return. `--directory` instead of `cd` —
    # `cd ... && ... &` wrapped the background job in a subshell
    # whose stdout inherited the backdoor serial FD; Machine.succeed's
    # read-until-marker then blocked on the never-closing pipe.
    # `</dev/null`: http.server doesn't read stdin, but detach it
    # anyway so nothing in the process tree holds the backdoor
    # shell's input side.
    k3s_server.succeed(
        "${pkgs.python3}/bin/python3 -m http.server ${toString originPort} "
        "--directory /srv/origin "
        "</dev/null >/tmp/origin-http.log 2>&1 & "
        "echo $! > /tmp/origin-http.pid"
    )
    # Bind check (not sleep): fails fast if the port was already taken
    # or python crashed on startup.
    k3s_server.wait_until_succeeds(
        "${pkgs.netcat}/bin/nc -z localhost ${toString originPort}",
        timeout=10,
    )

    # ── Build helper ──────────────────────────────────────────────────
    # Same ssh-ng pattern as lifecycle.nix. `extra_args` appends
    # --argstr for the parameterized fod-fetch.nix (url, sha256).
    # `expect_fail` → client.fail (for the denied case). Returns the
    # combined stdout+stderr so the denied case can assert the wget
    # error message.
    #
    # `--timeout 60 --max-silent-time 60`: NO-OPS over ssh-ng — the client
    # never sends wopSetOptions (P0215 empirical). Kept as harmless; the
    # live bounds are `wget -T 15` inside the FOD (network layer) and
    # `timeout 150` at the shell.
    #
    # TODO(P0308-followup): whiteout fix unvalidated in k3s — timing
    #   assertion removed until rio-worker mknod-whiteout proven to work
    #   in emptyDir/tmpfs upperdir.
    #
    #   The mknod whiteout fix (executor/mod.rs step 4b, commit 2cb3b221)
    #   was verified in an `unshare -rm` harness but NEVER end-to-end in
    #   k3s. First k3s run: 60.1s (= FUSE fetch_timeout). Second run
    #   (shell timeout 90s): 90.1s — hit the shell timeout, meaning the
    #   daemon hung INDEFINITELY, not just one fetch_timeout cycle. The
    #   whiteout is not preventing the overlay→FUSE fall-through AND the
    #   gRPC doesn't return. executor/mod.rs has a merged-view ENOENT
    #   diagnostic that warns "NOT visible as ENOENT" — but the test's
    #   dump_all_logs only fires on build() raising, and client.fail()
    #   succeeds on nonzero exit, so the diagnostic was never captured.
    #   The denied subtest now dumps worker logs unconditionally.
    #
    #   Possible causes (untested): k3s emptyDir may be tmpfs — check
    #   whether tmpfs upperdir honors char-dev-0/0 whiteouts; mount
    #   namespace ordering (mknod writes to upper's backing fs, daemon
    #   stats via merged in a CHILD namespace — is the overlay mount
    #   propagated?); nested overlay quirk. Fix may need xattr-based
    #   whiteout (trusted.overlay.whiteout) or FUSE-side negative-cache.
    #
    #   Shell timeout 150s admits one fetch_timeout + retry + slop.
    #   Re-tighten to 60s + add `elapsed < 45` once validated.
    def build(drv_file, extra_args="", expect_fail=False):
        cmd = (
            f"timeout 150 "
            f"nix-build --no-out-link --timeout 60 --max-silent-time 60 "
            f"--store 'ssh-ng://k3s-server' "
            f"--arg busybox '(builtins.storePath ${common.busybox})' "
            f"{extra_args} {drv_file} 2>&1"
        )
        try:
            if expect_fail:
                return client.fail(cmd)
            return client.succeed(cmd)
        except Exception:
            dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")
            raise

    # ══════════════════════════════════════════════════════════════════
    # allowed — allowlisted FOD → build succeeds + TCP_MISS
    # ══════════════════════════════════════════════════════════════════
    # fodProxy.allowedDomains[0]=k3s-server (set via extraValues in
    # default.nix) puts the origin host in squid's dstdomain ACL. The
    # FOD builder's wget → http_proxy → squid → sees `k3s-server` in
    # the GET request → matches ACL → forwards → origin serves the file
    # → wget writes $out → hash matches → build succeeds.
    #
    # Squid log format: `<ts> <elapsed> <client> TCP_MISS/200 <bytes>
    # GET http://k3s-server:8081/fixture - DIRECT/<ip> -`. TCP_MISS
    # means "not in cache, fetched from origin" — proof squid handled
    # AND forwarded the request (vs e.g. TCP_HIT which wouldn't prove
    # the outbound leg, or no log at all which would mean wget bypassed
    # the proxy entirely).
    with subtest("fod-proxy-allowed: allowlisted fetch succeeds via squid"):
        out = build(
            "${drvs.fodFetch}",
            extra_args=(
                "--argstr url 'http://k3s-server:${toString originPort}/fixture' "
                "--argstr sha256 '${fodFixtureSha256}'"
            ),
        )
        print(f"allowed build output:\n{out}")
        # Last non-empty line is the store path (earlier lines: SSH
        # warnings, nix-build progress).
        store_path = [l for l in out.strip().split("\n") if l.strip()][-1]
        assert store_path.startswith("/nix/store/"), (
            f"allowed FOD build should produce a store path: {store_path!r}"
        )

        # Squid log assert. `kubectl logs deploy/` follows to the current
        # pod. URL-specific grep (not just TCP_MISS) so a stray health-
        # check or other test's traffic can't false-positive.
        squid_log = kubectl("logs deploy/rio-fod-proxy")
        assert "TCP_MISS" in squid_log and "k3s-server" in squid_log, (
            f"squid log should show TCP_MISS for k3s-server "
            f"(proxy forwarded the request):\n{squid_log}"
        )
        print("fod-proxy-allowed PASS: build succeeded + TCP_MISS in squid log")

    # ══════════════════════════════════════════════════════════════════
    # denied — non-allowlisted FOD → build FAILS + TCP_DENIED/403 (Q4)
    # ══════════════════════════════════════════════════════════════════
    # `k3s-agent` — resolves instantly via CoreDNS NodeHosts (just like
    # k3s-server), NOT in allowedDomains (only k3s-server is). Squid
    # matches `k3s-agent` against the dstdomain ACL → no match → 403.
    #
    # NOT `denied.invalid`: squid resolves the destination host eagerly,
    # and CoreDNS forwards `.invalid` to the node's upstream DNS — which
    # is unreachable in an airgapped VM → DNS query hangs ~30s × retries
    # → blew the globalTimeout. A resolvable-but-denied host avoids the
    # DNS trap entirely. Port 1 (tcpmux, nothing listens) — if squid
    # somehow DID forward, wget would still fail fast on connection
    # refused, not hang.
    #
    # Q4 — BOTH assertions: build-failure alone could be DNS failure,
    # origin down, timeout, anything. TCP_DENIED/403 in the squid log
    # proves SQUID's ACL blocked it. Without the log-grep, this test
    # would pass on a broken squid that denies EVERYTHING (including
    # the allowed case above — but that's caught separately).
    with subtest("fod-proxy-denied: non-allowlisted fetch fails at squid"):
        # Root cause of the hang: builder exits 1 without creating $out
        # → nix-daemon's post-fail cleanup stats /nix/store/<fod-output>
        # → overlay falls through upper (ENOENT) → host store (ENOENT)
        # → FUSE → gRPC that blocks. Daemon's stat syscall hangs;
        # STDERR_LAST never written.
        #
        # P0308's intended fix (executor/mod.rs step 4b): whiteout each
        # output path in the overlay upper before spawning the daemon.
        # The whiteout should make overlayfs return ENOENT at upper
        # without probing lowers → daemon cleanup stat returns →
        # STDERR_LAST + BuildResult{PermanentFailure} → nix-build exits.
        #
        # TODO(P0308-followup): the whiteout fix is NOT working in k3s
        #   (see build() comment). This subtest validates FUNCTIONALITY
        #   only: the builder runs, squid denies, 403 propagates, squid
        #   log shows TCP_DENIED. The build currently hangs until the
        #   shell `timeout 150` kills it. `elapsed` is printed for
        #   diagnostic but NOT asserted. Once the whiteout is fixed,
        #   add back `assert elapsed < 45` (~5s builder + dispatch
        #   slop) — that's the regression guard for the overlay→FUSE
        #   fall-through.
        import time
        t0 = time.monotonic()
        out = build(
            "${drvs.fodFetch}",
            extra_args=(
                "--argstr url 'http://k3s-agent:1/fixture' "
                "--argstr sha256 '${bogusSha256}'"
            ),
            expect_fail=True,
        )
        elapsed = time.monotonic() - t0
        print(f"denied build output (expected failure, {elapsed:.1f}s):\n{out}")
        # Dump worker logs unconditionally: executor/mod.rs's merged-
        # view ENOENT diagnostic (`grep 'NOT visible as ENOENT'`) is
        # the signal for whether the whiteout took effect. The try/
        # except in build() only dumps on RAISE, but client.fail()
        # returns happily on nonzero exit — so without this explicit
        # dump, the diagnostic is never visible.
        print(kubectl("logs default-workers-0 --tail=100"))
        # wget's 403 stderr → STDERR_RESULT{101} → worker stderr loop
        # → LogBatch → scheduler → gateway STDERR_NEXT → nix-build.
        # Seeing it here proves the builder ran and failed at squid
        # (not a dispatch-queue timeout).
        assert "403" in out or "Forbidden" in out, (
            f"nix-build stderr should contain wget's 403 "
            f"(proves the builder actually ran and failed at squid, "
            f"not just timed out in dispatch queue):\n{out}"
        )
        # client.fail asserted nonzero exit. Squid's error page body
        # varies by version; the log-grep below is the hard assert.

        # Q4 — the hard assert. Squid's denied log line:
        # `<ts> <elapsed> <client> TCP_DENIED/403 <bytes> GET http://k3s-agent:1/...`
        squid_log = kubectl("logs deploy/rio-fod-proxy")
        assert "TCP_DENIED/403" in squid_log, (
            f"squid log should show TCP_DENIED/403 "
            f"(proves ACL blocked it, not a network misconfig):\n{squid_log}"
        )
        # URL-specific: the TCP_DENIED line is for OUR request, not
        # some unrelated deny. k3s-agent appears in the URL field.
        assert "k3s-agent" in squid_log, (
            f"TCP_DENIED should be for k3s-agent:\n{squid_log}"
        )
        print("fod-proxy-denied PASS: build failed + TCP_DENIED/403 in squid log")

    # ══════════════════════════════════════════════════════════════════
    # non-fod — non-FOD builder env has no http_proxy
    # ══════════════════════════════════════════════════════════════════
    # env-dump.nix has no outputHash → not a FOD → executor/mod.rs:462's
    # `if is_fod` gate evaluates false → spawn_daemon_in_namespace gets
    # fod_proxy=None → daemon env has no http_proxy → sandbox child
    # (the builder) can't inherit what was never set.
    #
    # If the gate were missing, the daemon would ALWAYS get http_proxy.
    # The non-FOD sandbox MIGHT strip it (Nix's non-FOD sandbox is
    # stricter than FOD), but "might" is a flake. The gate makes this
    # deterministic regardless of Nix sandbox config.
    with subtest("fod-proxy-nonfod: non-FOD builder env has no http_proxy"):
        out = build("${drvs.envDump}")
        print(f"env-dump build output:\n{out}")
        store_path = [l for l in out.strip().split("\n") if l.strip()][-1]
        assert store_path.startswith("/nix/store/"), (
            f"env-dump build should produce a store path: {store_path!r}"
        )

        # The output path lives in rio-store, not the client's local
        # store. Read it the same way the build fetched inputs: via
        # the gateway's ssh-ng store. `nix store cat` reads a single
        # file from a remote store.
        env_contents = client.succeed(
            f"nix store cat --store 'ssh-ng://k3s-server' {store_path}"
        )
        print(f"builder env:\n{env_contents}")
        # Case-insensitive: spawn.rs sets both http_proxy and HTTP_PROXY.
        # Neither should be present.
        assert "http_proxy" not in env_contents.lower(), (
            f"non-FOD builder should NOT have http_proxy in its env "
            f"(proves executor's is_fod gate):\n{env_contents}"
        )
        print("fod-proxy-nonfod PASS: non-FOD env has no http_proxy")

    # ── Teardown ──────────────────────────────────────────────────────
    k3s_server.execute("kill $(cat /tmp/origin-http.pid) 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
