# PutPathChunked end-to-end (ADR-022 §6, P0586).
#
# Everything here runs the real pipeline: the client submits builds over
# ssh-ng, the scheduler dispatches with HMAC assignment claims (the
# fixture sets withHmac = true), the worker's fused walk + HasChunks
# probe + PutPathChunked client uploads the outputs, and the store's
# sequential receive walk + single-transaction commit registers them.
# The VM test asserts only what the off-VM tests cannot: that this
# chain holds together on a real cluster and the committed data is
# usable afterwards.
#
# Subtests:
#   1. two-output upload      one chunked-commit event, served HasChunks probe
#   2. narinfo                cross-output + input refs and deriver recorded
#   3. byte-correct read-back gateway content matches the in-sandbox sha256
#   4. cross-derivation dedup identical payload streams ~no novel chunk bytes
#   5. floating-CA output     CA path check commits, reads back
#
# The builder's HasChunks probe is best-effort (a failed probe falls
# back to streaming every chunk as novel), so a spurious red in the
# dedup subtest should be triaged as probe degradation first.
#
# Deliberately NOT here (already covered at the handler/agreement
# level, and a real builder cannot produce them without a hand-rolled
# wire client): tampered chunks, wrong nar_hash, oversize Begin,
# out-of-order frames, mid-stream disconnect, idempotent re-drive —
# see rio-store/tests/grpc/put_path_chunked.rs and
# rio-builder/src/upload/agreement_tests.rs. Single-output refs/deriver
# are VM-verified by the lifecycle refs-end-to-end fragment.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Two outputs. $out embeds the $dev path and the busybox path
  # (cross-output + input references), carries the payload TWICE
  # (dup-a/dup-b → repeated chunk_manifest digests), and $dev records
  # the payload's sha256 computed inside the sandbox — read-back
  # compares post-upload content against this pre-upload hash.
  # `sha256sum < file` (stdin) so no store path leaks into $dev's
  # bytes (a $out path there would make the sibling outputs cyclic).
  multiOutDrv = drvs.mkCustom {
    name = "rio-ppc-multiout";
    extraAttrs.outputs = [
      "out"
      "dev"
    ];
    script = ''
      set -e
      bb=''${busybox}/bin/busybox
      $bb mkdir -p $out $dev
      $bb awk 'BEGIN { for (i = 0; i < 20000; i++) print "rio-multiout-payload-" i }' > $out/dup-a
      $bb cat $out/dup-a > $out/dup-b
      $bb sha256sum < $out/dup-a > $dev/payload.sha256
      $bb echo $dev > $out/dev-ref
      $bb echo ''${busybox} > $out/bb-ref
    '';
  };

  # Same ~700 KiB payload under two distinct derivations (distinct
  # marker file → distinct store paths). The payload content is unique
  # to this scenario, so build A's chunks can only have entered the
  # chunk store via build A — making build B's near-zero streamed
  # bytes attributable to the HasChunks probe.
  mkDedupDrv =
    marker:
    drvs.mkCustom {
      name = "rio-ppc-dedup-${marker}";
      script = ''
        set -e
        bb=''${busybox}/bin/busybox
        $bb mkdir -p $out
        $bb awk 'BEGIN { for (i = 0; i < 30000; i++) print "rio-dedup-payload-" i }' > $out/blob
        $bb echo dedup-${marker} > $out/marker
      '';
    };
  dedupADrv = mkDedupDrv "alpha";
  dedupBDrv = mkDedupDrv "beta";

  # Floating-CA, single output, >256 KiB payload so the CA path is
  # exercised together with a multi-chunk manifest. No store paths in
  # the output (a self-referencing floating CA is unsupported); the
  # recorded blob.sha256 again uses stdin so only the hash is stored.
  caDrv = drvs.mkCustom {
    name = "rio-ppc-ca";
    extraAttrs = {
      __contentAddressed = true;
      outputHashMode = "recursive";
      outputHashAlgo = "sha256";
    };
    script = ''
      set -e
      bb=''${busybox}/bin/busybox
      $bb mkdir -p $out
      $bb awk 'BEGIN { for (i = 0; i < 20000; i++) print "rio-ca-payload-" i }' > $out/blob
      $bb sha256sum < $out/blob > $out/blob.sha256
      $bb echo rio-ca-proof > $out/stamp
    '';
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-put-path-chunked";
  skipTypeCheck = true;
  # 4 small builds + read-backs; boot dominates. Same ceiling as the
  # other standalone build scenarios.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
    }}

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}, worker])";
    }}

    store_url = "ssh-ng://${gatewayHost}"

    # ── Worker-journal helpers ─────────────────────────────────────────
    # The rio-builder process is one-shot (exits after each build), so
    # its Prometheus counters reset; journald is the persistent record.
    # upload/chunked.rs logs exactly one "chunked upload committed
    # atomically" event per successful PutPathChunked stream, with an
    # outputs count and the number of novel chunk bytes streamed.

    def strip_ansi(s):
        return re.sub(r"\x1b\[[0-9;]*m", "", s)

    def chunked_commit_lines():
        rc, out = worker.execute(
            "journalctl -u rio-builder --no-pager"
            " | grep 'chunked upload committed atomically'"
        )
        return [strip_ansi(l) for l in out.splitlines() if l.strip()]

    def journal_field_int(line, name):
        m = re.search(name + r"[=: ]+([0-9]+)", line)
        assert m, (
            f"could not extract numeric field {name!r} from the worker "
            f"journal line: {line!r} — did the upload log fields change?"
        )
        return int(m.group(1))

    def chunk_file_count():
        # Filesystem chunk backend object count (standalone fixture
        # stores chunks under the store's StateDirectory).
        return int(${gatewayHost}.succeed(
            "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
        ).strip())

    def has_chunks_count(scraped):
        # HasChunks probe counter on the store: only the builder's
        # chunked upload client calls this RPC.
        return metric_value(
            scraped,
            "rio_store_directory_has_batch_size_count",
            labels='{rpc="HasChunks"}',
        ) or 0.0

    def remote_sha256(path):
        # Read a file back through the real client path (gateway →
        # store GetPath, NAR regenerated from the chunk manifest) and
        # hash it client-side.
        out = client.succeed(
            f"nix store cat --store '{store_url}' {path} | sha256sum"
        )
        return out.strip().split()[0]

    # ══════════════════════════════════════════════════════════════════
    # 1. Two-output derivation → one PutPathChunked stream
    # ══════════════════════════════════════════════════════════════════
    with subtest("two-output drv uploads via one PutPathChunked stream"):
        drv = client.succeed(
            "nix-instantiate --arg busybox '(builtins.storePath ${common.busybox})' "
            "${multiOutDrv} 2>/dev/null"
        ).strip().split("!")[0]
        assert drv.endswith(".drv"), f"expected a .drv path, got {drv!r}"

        out_lines = client.succeed(f"nix-store -q --outputs {drv}").split()
        out_paths = [p for p in out_lines if p.startswith("/nix/store/")]
        assert len(out_paths) == 2, f"expected 2 outputs, got {out_paths!r}"
        dev_path = next(p for p in out_paths if p.endswith("-dev"))
        out_path = next(p for p in out_paths if not p.endswith("-dev"))

        # Copy the .drv closure FIRST (this is where the legacy PutPath
        # ingest of the .drv + busybox happens), so the metric window
        # below contains only the dispatched build's own uploads.
        client.succeed(
            f"nix copy --no-check-sigs --derivation --to '{store_url}' {drv}"
        )

        before = scrape_metrics(${gatewayHost}, 9092)
        rc, build_out = client.execute(
            f"nix build --no-link --print-out-paths --store '{store_url}' "
            f"'{drv}^*' 2>&1"
        )
        if rc != 0:
            print(f"=== nix build failed rc={rc} ===\n{build_out}\n=== end ===")
            dump_all_logs([${gatewayHost}, worker])
            raise Exception("two-output build failed, see output above")
        after = scrape_metrics(${gatewayHost}, 9092)

        # Both outputs registered and queryable through the gateway.
        client.succeed(
            f"nix path-info --store '{store_url}' {out_path} {dev_path}"
        )

        # Exactly one chunked commit event → both outputs travelled in
        # ONE client stream (a regression splitting them per output
        # would log two).
        commits = chunked_commit_lines()
        assert len(commits) == 1, (
            f"expected exactly 1 'chunked upload committed atomically' "
            f"journal event for the 2-output build, got {len(commits)}: "
            f"{commits!r}"
        )

        # The store served the builder's HasChunks durable-presence
        # probe — only the chunked upload client calls that RPC, so
        # this pins the upload to the PutPathChunked pipeline.
        assert has_chunks_count(after) > has_chunks_count(before), (
            "rio_store_directory_has_batch_size_count{rpc=HasChunks} did "
            "not move during the build — the builder never probed "
            "HasChunks, so the upload cannot have used the chunked path "
            f"(before={has_chunks_count(before)}, after={has_chunks_count(after)})"
        )

        # Exactly two paths were created during the build window and
        # none resolved as "exists", so no legacy PutPath/PutPathBatch
        # re-ingest of these outputs landed after the chunked commit.
        # The positive evidence that the upload took the chunked path
        # is above: the commit journal event plus HasChunks movement.
        assert_metric_delta(
            before, after,
            "rio_store_put_path_total", 2.0,
            labels='{result="created"}',
        )
        assert_metric_delta(
            before, after,
            "rio_store_put_path_total", 0.0,
            labels='{result="exists"}',
        )

    # ══════════════════════════════════════════════════════════════════
    # 2. narinfo: cross-output reference + deriver
    # ══════════════════════════════════════════════════════════════════
    with subtest("narinfo records sibling-output + input refs and deriver"):
        # The store's verify scan and the builder's walk must AGREE on
        # the refs for the commit to happen at all; this asserts they
        # both agreed on the RIGHT set — in particular the sibling
        # output, which only the output_paths half of the candidate set
        # can produce.
        out_refs = psql(${gatewayHost},
            "SELECT array_to_string(\"references\", ' ') FROM narinfo "
            f"WHERE store_path = '{out_path}'"
        ).split()
        assert len(out_refs) == 2, (
            f"$out embeds exactly the $dev path and the busybox input, "
            f"so narinfo.references for {out_path} must have exactly 2 "
            f"entries; got: {out_refs!r}"
        )
        assert_set_eq(
            out_refs,
            [dev_path, "${common.busybox}"],
            context=f"narinfo.references for {out_path}",
        )

        dev_refs = psql(${gatewayHost},
            "SELECT array_to_string(\"references\", ' ') FROM narinfo "
            f"WHERE store_path = '{dev_path}'"
        ).split()
        assert dev_refs == [], (
            f"$dev embeds no store paths (the payload hash is read from "
            f"stdin), so narinfo.references for {dev_path} must be "
            f"empty; got: {dev_refs!r}"
        )

        deriver = psql(${gatewayHost},
            f"SELECT deriver FROM narinfo WHERE store_path = '{out_path}'"
        )
        assert deriver == drv, (
            f"narinfo.deriver for {out_path} should be the dispatched "
            f".drv path {drv}, got {deriver!r}"
        )

    # ══════════════════════════════════════════════════════════════════
    # 3. Byte-correct read-back (NAR regenerated from chunks)
    # ══════════════════════════════════════════════════════════════════
    with subtest("outputs read back byte-correct through the gateway"):
        # Hash recorded inside the build sandbox, BEFORE upload.
        recorded = client.succeed(
            f"nix store cat --store '{store_url}' {dev_path}/payload.sha256"
        ).strip().split()[0]
        assert len(recorded) == 64, (
            f"recorded payload hash looks malformed: {recorded!r}"
        )

        got_a = remote_sha256(f"{out_path}/dup-a")
        got_b = remote_sha256(f"{out_path}/dup-b")
        assert got_a == recorded, (
            f"dup-a read back through the gateway hashes to {got_a}, but "
            f"the build recorded {recorded} before upload — chunk "
            "reassembly or NAR-framing regeneration corrupted the file"
        )
        assert got_b == recorded, (
            f"dup-b (byte-identical twin, repeated chunk digests) hashes "
            f"to {got_b}, expected {recorded} — the repeated-chunk splice "
            "on the read path is broken"
        )

        dev_ref = client.succeed(
            f"nix store cat --store '{store_url}' {out_path}/dev-ref"
        ).strip()
        assert dev_ref == dev_path, (
            f"$out/dev-ref should contain the dev output path {dev_path}, "
            f"got {dev_ref!r}"
        )

    # ══════════════════════════════════════════════════════════════════
    # 4. Cross-derivation chunk dedup via HasChunks
    # ══════════════════════════════════════════════════════════════════
    with subtest("second drv with identical payload reuses chunks"):
        n_commits_before = len(chunked_commit_lines())
        chunks_before_a = chunk_file_count()
        build("${dedupADrv}")
        chunks_after_a = chunk_file_count()
        delta_a = chunks_after_a - chunks_before_a
        assert delta_a >= 3, (
            f"build A's ~700 KiB unique payload should add several chunk "
            f"objects (FastCDC max 256 KiB), got delta {delta_a} "
            f"(before={chunks_before_a}, after={chunks_after_a}) — was "
            "the payload not chunked at all?"
        )
        commits_a = chunked_commit_lines()
        assert len(commits_a) == n_commits_before + 1, (
            f"build A (dedup-alpha) should add exactly one chunked commit "
            f"event, had {n_commits_before}, now {len(commits_a)}"
        )
        bytes_a = journal_field_int(commits_a[-1], "bytes_streamed")
        assert bytes_a >= 600000, (
            f"build A should stream the full ~709 KB payload as novel "
            f"chunks, journal says bytes_streamed={bytes_a}"
        )

        before_b = scrape_metrics(${gatewayHost}, 9092)
        build("${dedupBDrv}")
        after_b = scrape_metrics(${gatewayHost}, 9092)
        chunks_after_b = chunk_file_count()
        delta_b = chunks_after_b - chunks_after_a

        commits = chunked_commit_lines()
        assert len(commits) == n_commits_before + 2, (
            f"builds A+B (dedup-alpha, dedup-beta) should add exactly two "
            f"chunked commit events, had {n_commits_before}, now {len(commits)}"
        )
        bytes_b = journal_field_int(commits[-1], "bytes_streamed")

        # Build B's blob is byte-identical → identical FastCDC chunk
        # set → HasChunks reports it durable → the builder streams only
        # the tiny unique marker file. Structural, not timing-based.
        assert bytes_b <= 65536, (
            f"build B re-streamed {bytes_b} bytes; with HasChunks dedup "
            "working it should stream only the ~10-byte marker chunk "
            f"(build A streamed {bytes_a})"
        )
        assert delta_b <= 2, (
            f"build B added {delta_b} chunk objects; the shared payload "
            "chunks must be reused, only the marker file may add one "
            f"(build A added {delta_a})"
        )
        # The dedup decision was driven by a served HasChunks probe.
        assert has_chunks_count(after_b) > has_chunks_count(before_b), (
            "HasChunks was not probed during build B — dedup cannot have "
            "been HasChunks-driven"
        )

    # ══════════════════════════════════════════════════════════════════
    # 5. Floating-CA output through PutPathChunked
    # ══════════════════════════════════════════════════════════════════
    with subtest("floating-CA output commits and reads back"):
        # With withHmac the assignment claims carry is_ca=true and an
        # empty expected_outputs list; the store derives the CA path
        # from the claimed nar_hash + refs and rejects a mismatch with
        # PERMISSION_DENIED — so this build succeeding is the positive
        # proof of the CA path check under real claims.
        # --impure mirrors the ca-cutoff scenario's CA builds.
        n_before = len(chunked_commit_lines())
        ca_out = build("${caDrv}", extra_args="--impure")
        assert ca_out.startswith("/nix/store/"), (
            f"CA build did not return a store path: {ca_out!r}"
        )
        assert "rio-ppc-ca" in ca_out, f"wrong drv built: {ca_out!r}"

        commits = chunked_commit_lines()
        assert len(commits) == n_before + 1, (
            f"CA build should add exactly one chunked commit event, "
            f"had {n_before}, now {len(commits)}"
        )

        stamp = client.succeed(
            f"nix store cat --store '{store_url}' {ca_out}/stamp"
        ).strip()
        assert stamp == "rio-ca-proof", (
            f"CA output stamp read back as {stamp!r}, expected rio-ca-proof"
        )
        ca_recorded = client.succeed(
            f"nix store cat --store '{store_url}' {ca_out}/blob.sha256"
        ).strip().split()[0]
        ca_got = remote_sha256(f"{ca_out}/blob")
        assert ca_got == ca_recorded, (
            f"CA blob read back hashes to {ca_got}, but the build "
            f"recorded {ca_recorded} before upload — CA chunk storage or "
            "read-back is corrupting content"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
