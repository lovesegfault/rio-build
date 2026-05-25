# log-service — rio-store LogService end-to-end (AppendLog/TailLog).
#
# Standalone fixture, no workers: the store's log data plane is driven
# directly with grpcurl, the way a builder will drive it from commit 4
# onward. Proves, against a real store process + PostgreSQL + the
# filesystem chunk backend:
#
#   1. An authenticated AppendLog stream (real HMAC assignment token,
#      the binding gate's assignments/derivations lookup) ingests
#      batches and acks durability on the stream-end drain.
#   2. TailLog returns the ingested lines in order, gapless, with the
#      correct content, via both the pinned-exec and the
#      latest-exec-resolution paths.
#   3. A store process restart loses nothing: the chunks are on disk
#      (FilesystemLogChunkStore), the manifest is in PG, and a second
#      AppendLog session resumes the same execution at the next line
#      number (the session lease's same-pod re-acquire + the read
#      path's cross-session line-number dedup).
#   4. The ingest metrics are emitted and registered.
#
# Token minting: a real assignment token, signed at Nix build time with
# the deterministic vm-test HMAC key (lib/hmac-keys.nix) — the same
# pattern as the standalone fixture's executorTokenEnv. The dev-mode
# (no HMAC key) path would skip the binding gate entirely, which is
# exactly the part of AppendLog worth proving end-to-end.
#
# grpcurl drives the bidi AppendLog stream with `-d @`: it sends every
# request message from stdin, half-closes, then prints the response
# messages (the acks). No interleaving is needed — the only ack for a
# small log is the stream-end drain's — so grpcurl's batch bidi mode is
# sufficient and no bespoke Rust test client is required.
#
# r[verify ...] markers are deliberately absent: the store.log.* spec
# rules land with the commit-5 spec rewrite. Markers go at the
# default.nix wiring point when they exist.
{
  pkgs,
  common,
  fixture,
}:
let
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";

  # ── Test identity constants ─────────────────────────────────────────
  # The 32-char hash is valid nixbase32 (no e/o/u/t) so drv_log_hash's
  # StorePath::parse fast path is exercised, not the split-on-dash
  # fallback. claims.drv_hash carries the DAG-key basename form (what
  # the scheduler signs and what derivations.drv_hash stores);
  # header.derivation_path carries the full store path; both normalize
  # to the bare hash, which is what drv_executions.drv_hash and the
  # chunk keys use.
  drvHash32 = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm";
  drvBasename = "${drvHash32}-vmtest-log.drv";
  drvFullPath = "/nix/store/${drvBasename}";
  builderId = "vm-log-test-builder";
  # UUIDv7-shaped, fixed. The latest-exec resolution orders by exec_id
  # DESC; with a single execution the ordering is irrelevant.
  execId = "01900000-0000-7000-8000-00000000aaaa";
  derivationId = "01900000-0000-7000-8000-00000000dddd";

  # ── Assignment token ────────────────────────────────────────────────
  # AssignmentClaims (rio-auth/src/hmac.rs): executor_id, drv_hash,
  # expected_outputs, is_ca, expiry_unix [, tenant — omitted when None
  # via skip_serializing_if, so it MUST be absent here too]. Signed
  # b64url_nopad(claims_json) "." b64url_nopad(hmac_sha256(key, json)).
  # The key file carries a trailing LF that load_key() trims — mirror it.
  assignmentTokenEnv =
    pkgs.runCommand "rio-log-test-assignment-token"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 - ${fixture.hmacKeys}/hmac.key > $out <<'EOF'
        import base64, hashlib, hmac, json, sys
        key = open(sys.argv[1], "rb").read()
        for suf in (b"\r\n", b"\n"):
            if key.endswith(suf):
                key = key[: -len(suf)]
                break
        claims = json.dumps(
            {
                "executor_id": "${builderId}",
                "drv_hash": "${drvBasename}",
                "expected_outputs": [],
                "is_ca": False,
                "expiry_unix": 9999999999,
            },
            separators=(",", ":"),
        ).encode()
        sig = hmac.new(key, claims, hashlib.sha256).digest()
        b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
        print(b64(claims) + "." + b64(sig))
        EOF
      '';
in
pkgs.testers.runNixOSTest {
  name = "rio-log-service";
  skipTypeCheck = true;
  # 2-VM boot (~90s) + control-plane wait + two AppendLog/TailLog
  # round-trips + one store restart (~10s). Observed well under 300s
  # on KVM; 600 leaves margin for CI jitter.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import base64

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}

    token = control.succeed("cat ${assignmentTokenEnv}").strip()

    # ── Helpers ───────────────────────────────────────────────────────

    def grpcurl_json_stream(out: str) -> list[dict]:
        """Parse grpcurl's concatenated-JSON stream output (one
        pretty-printed object per response message). Leading non-JSON
        is skipped by seeking to the first brace."""
        dec, objs = json.JSONDecoder(), []
        idx = out.find("{")
        while 0 <= idx < len(out):
            obj, idx = dec.raw_decode(out, idx)
            objs.append(obj)
            idx = out.find("{", idx)
        return objs

    def gen_batches(path, first_line, count, lines_per_batch=2):
        """Generate `count` lines starting at `first_line` as
        newline-delimited AppendLogRequest batch messages, appended to
        `path` on the control VM. Line N's content is log-line-NNNNN so
        the read-back assertion can verify both order and content.
        Built VM-side with a shell loop so the test driver never ships
        a multi-KB command."""
        assert count % lines_per_batch == 0
        control.succeed(
            "set -e; i=" + str(first_line) + "; "
            "end=" + str(first_line + count) + "; "
            "while [ $i -lt $end ]; do "
            "  l1=$(printf 'log-line-%05d' $i | base64 -w0); "
            "  l2=$(printf 'log-line-%05d' $((i + 1)) | base64 -w0); "
            '  printf \'{"batch":{"lines":["%s","%s"],"firstLineNumber":"%s"}}\\n\' '
            '    "$l1" "$l2" "$i" >> ' + path + "; "
            "  i=$((i + 2)); "
            "done"
        )

    def append_log(reqfile, max_time=60):
        """Drive one AppendLog bidi stream from a request-message file.
        grpcurl sends every stdin message, half-closes, and prints the
        ack stream. Returns the parsed acks."""
        out = control.succeed(
            "${grpcurl} -plaintext -max-time " + str(max_time) + " "
            "-protoset ${protoset}/rio.protoset "
            "-H 'x-rio-assignment-token: " + token + "' "
            "-d @ localhost:9002 rio.store.LogService/AppendLog "
            "< " + reqfile + " 2>&1"
        )
        return grpcurl_json_stream(out)

    def tail_log(derivation, exec_id="", since_line=0):
        """One-shot (follow=false) TailLog. Returns the ordered list of
        (line_number, content) pairs reassembled from the response
        chunks, plus the final chunk's isComplete."""
        req = json.dumps(
            {"derivation": derivation, "execId": exec_id,
             "sinceLine": str(since_line), "follow": False}
        )
        out = control.succeed(
            "${grpcurl} -plaintext -max-time 30 "
            "-protoset ${protoset}/rio.protoset "
            "-d '" + req + "' "
            "localhost:9002 rio.store.LogService/TailLog 2>&1"
        )
        chunks = grpcurl_json_stream(out)
        lines = []
        for c in chunks:
            first = int(c.get("firstLineNumber", "0"))
            for j, b in enumerate(c.get("lines", [])):
                lines.append((first + j, base64.b64decode(b).decode()))
        is_complete = bool(chunks[-1].get("isComplete", False)) if chunks else False
        return lines, is_complete

    def assert_contiguous(lines, expect_count):
        """Every line 0..expect_count present exactly once, in order,
        with the content the writer generated for that line number."""
        assert len(lines) == expect_count, (
            f"expected {expect_count} lines, got {len(lines)}: "
            f"first/last = {lines[:2]} .. {lines[-2:]}"
        )
        for i, (n, content) in enumerate(lines):
            assert n == i, f"line-number gap or reorder at index {i}: got {n}"
            want = f"log-line-{i:05d}"
            assert content == want, (
                f"line {i} content mismatch: got {content!r}, want {want!r}"
            )

    # ── Seed the assignment the binding gate verifies against ─────────
    # The gate joins assignments → derivations on derivation_id, filters
    # on derivations.drv_hash = claims.drv_hash (the DAG-key form), and
    # requires the latest attempt's (exec_id, builder_id) to match the
    # header + claims. drv_executions seeds the latest-exec resolution
    # (its drv_hash is the BARE 32-char form — a different column, a
    # different format, per M_064).
    with subtest("seed assignment + execution rows"):
        psql(control,
            "INSERT INTO derivations "
            "(derivation_id, drv_hash, drv_path, system, status) VALUES "
            "('${derivationId}', '${drvBasename}', '${drvFullPath}', "
            " 'x86_64-linux', 'running')")
        psql(control,
            "INSERT INTO assignments "
            "(derivation_id, builder_id, generation, status, exec_id) VALUES "
            "('${derivationId}', '${builderId}', 1, 'acknowledged', "
            " '${execId}')")
        psql(control,
            "INSERT INTO drv_executions "
            "(exec_id, drv_hash, executor_id, started_at) VALUES "
            "('${execId}', '${drvHash32}', '${builderId}', now())")

    # ── Round 1: ingest lines 0..199, read them back ──────────────────
    with subtest("AppendLog ingests and acks 200 lines"):
        control.succeed(
            "printf '%s\\n' "
            '\'{"header":{"derivationPath":"${drvFullPath}","execId":"${execId}"}}\' '
            "> /tmp/append1.json"
        )
        gen_batches("/tmp/append1.json", 0, 200)
        acks = append_log("/tmp/append1.json")
        assert acks, "AppendLog returned no acks (expected the stream-end drain ack)"
        last = int(acks[-1].get("durableThroughLine", "0"))
        assert last == 199, f"final ack durable_through_line = {last}, want 199"

    with subtest("TailLog (pinned exec) returns 200 lines in order"):
        lines, _ = tail_log("${drvFullPath}", exec_id="${execId}")
        assert_contiguous(lines, 200)

    # ── Restart the store: the log must survive on disk + in PG ───────
    with subtest("store restart preserves the chunks and the manifest"):
        control.succeed("systemctl restart rio-store.service")
        control.wait_for_unit("rio-store.service")
        control.wait_for_open_port(9002)
        lines, _ = tail_log("${drvFullPath}", exec_id="${execId}")
        assert_contiguous(lines, 200)

    # ── Round 2: a new session resumes the same execution ─────────────
    with subtest("a second AppendLog session appends lines 200..399"):
        control.succeed(
            "printf '%s\\n' "
            '\'{"header":{"derivationPath":"${drvFullPath}","execId":"${execId}"}}\' '
            "> /tmp/append2.json"
        )
        gen_batches("/tmp/append2.json", 200, 200)
        acks = append_log("/tmp/append2.json")
        assert acks, "second AppendLog returned no acks"
        last = int(acks[-1].get("durableThroughLine", "0"))
        assert last == 399, f"final ack durable_through_line = {last}, want 399"

    # ── The full log: both sessions, deduped, ordered, gapless ────────
    with subtest("TailLog (latest-exec resolution) returns all 400 lines"):
        lines, _ = tail_log("${drvHash32}")
        assert_contiguous(lines, 400)

    with subtest("ingest metrics are emitted"):
        # Prometheus counters are per-process in-memory state and the
        # store was restarted between the two ingest rounds, so the
        # counter only reflects the CURRENT process's ingest — round
        # 2's 200 lines. >= 200 still proves the metric is emitted,
        # registered, and counts real lines (an unregistered or typo'd
        # metric name would be absent → sum() over nothing → 0).
        scraped = scrape_metrics(control, 9092)
        total = sum(
            scraped.get("rio_store_log_ingest_lines_total", {}).values()
        )
        assert total >= 200, (
            f"rio_store_log_ingest_lines_total = {total}, want >= 200; "
            f"series: {scraped.get('rio_store_log_ingest_lines_total')}"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
