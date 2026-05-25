# Differential parity harness: the same derivation corpus is built by
# real Nix (the oracle) and by the native executor stack (request glue →
# rio-exec sandbox → result pipeline, via the `differential-driver`
# binary), inside one VM, and the results are compared entry by entry —
# NAR hashes, reference sets, failure classifications, and the @nix
# log-filter behaviour.
#
# This is the parity gate for the nix-daemon removal: the activation
# milestone may not flip the dispatch until this scenario is green, and
# every `knownDivergence` recorded below is an explicit punch-list item
# for that milestone (the harness prints them; it does not hide them).
#
# Differences from production deliberately accepted here (documented in
# rio-builder/src/executor/differential.rs as well): no scheduler/store
# control plane, the input closure is copied into a scratch directory
# instead of the FUSE/overlay merged view, and the oracle results are
# computed in the same VM run instead of from committed fixtures (the
# design's record/replay split becomes worthwhile once the corpus is
# stable; recomputing keeps the fixtures from going stale while the
# corpus is still growing).
#
# The verify markers live at the default.nix wiring per the tracey
# convention.
{
  pkgs,
  rio-workspace,
}:
let
  # The corpus builders: a static bash (the builder executable, so
  # structuredAttrs' `.attrs.sh` — bash syntax — sources cleanly, exactly
  # like stdenv builds) and a static busybox (PATH utilities).
  bashStatic = pkgs.pkgsStatic.bash;
  busyboxStatic = pkgs.pkgsStatic.busybox;
  # The same sandbox shell real Nix is configured with at build time, so
  # /bin/sh is observationally identical in both sandboxes.
  sandboxShell = pkgs.busybox-sandbox-shell;

  corpusFile = ../lib/derivations/differential-corpus.nix;

  # Per-entry expectations, consumed by the testScript.
  #
  #   expect = "parity"      → both succeed; NAR hashes + references equal
  #   expect = "both-fail"   → both fail; the native side must report
  #                            `rio_status`
  #   expect = "diverge"     → a documented divergence: `nix` describes
  #                            the oracle's behaviour, `rio_status` /
  #                            `rio_glue_error` the native side's; the
  #                            entry is reported, not failed
  entryMeta = {
    trivial = {
      expect = "parity";
    };
    multi-output = {
      expect = "parity";
    };
    env-dump = {
      expect = "parity";
    };
    setuid-attempt = {
      expect = "parity";
    };
    ptmx-open = {
      expect = "parity";
    };
    symlink-output = {
      expect = "parity";
    };
    stray-store-path = {
      expect = "parity";
    };
    structured-attrs = {
      expect = "parity";
    };
    pass-as-file = {
      expect = "parity";
    };
    phase-reporter = {
      expect = "parity";
      phases = [
        "buildPhase"
        "installPhase"
      ];
    };
    erg-with-drv = {
      expect = "diverge";
      nix = "succeeds";
      rio_glue_error = "exportReferencesGraph";
      note = "drv-closure expansion is unimplemented in the request glue; M7 punch list";
    };
    disallowed-requisites = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    outputchecks-maxsize = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    unsafe-discard = {
      expect = "parity";
      references_must_be_empty = true;
    };
    fod-flat = {
      expect = "parity";
    };
    fod-recursive = {
      expect = "parity";
    };
    fod-mismatch = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    fod-unknown-algo = {
      expect = "diverge";
      nix = "succeeds";
      rio_status = "OutputRejected";
      note = "rio is fail-closed on unverifiable outputHashAlgo by design";
    };
    fod-builder-fails = {
      expect = "both-fail";
      rio_status = "TransientFailure";
    };
    ca-multi-output = {
      expect = "diverge";
      nix = "succeeds";
      rio_status = "success";
      note = "floating-CA finalization lands in M6b; native side builds at scratch paths";
    };
    ca-selfref = {
      expect = "diverge";
      nix = "succeeds";
      rio_status = "success";
      note = "floating-CA finalization lands in M6b; native side builds at scratch paths";
    };
    ca-structured = {
      expect = "diverge";
      nix = "succeeds";
      rio_status = "success";
      note = "floating-CA finalization lands in M6b; native side builds at scratch paths";
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-differential";
  # Two real sandboxed builds per corpus entry plus the driver's own
  # closure copies; generous but bounded.
  globalTimeout = 3600;

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation = {
        cores = 4;
        memorySize = 4096;
        diskSize = 8192;
        # The oracle (`nix-build`) needs a writable store. The native
        # side does NOT build against /nix/store (the driver copies the
        # closure into a scratch dir), so the worker fixtures'
        # overlay-on-overlay concern does not apply here.
        writableStore = true;
        # Make the corpus inputs and the oracle's sandbox shell valid
        # store paths inside the VM.
        additionalPaths = [
          bashStatic
          busyboxStatic
          sandboxShell
          "${corpusFile}"
        ];
      };

      environment.systemPackages = [
        rio-workspace
        pkgs.nix
      ];

      nix.settings = {
        sandbox = true;
        experimental-features = [
          "nix-command"
          "ca-derivations"
        ];
        # The driver pins build_cores = 1; pin the oracle to the same so
        # NIX_BUILD_CORES (visible in the env-dump entry) matches.
        cores = 1;
        max-jobs = 2;
        substituters = pkgs.lib.mkForce [ ];
      };
    };

  testScript = ''
    import json
    import base64

    machine.start()
    machine.wait_for_unit("multi-user.target")

    BASH = "${bashStatic}"
    BUSYBOX = "${busyboxStatic}"
    SANDBOX_SHELL = "${sandboxShell}/bin/busybox"
    CORPUS = "${corpusFile}"
    META = json.loads('${builtins.toJSON entryMeta}')

    divergences = []
    failures = []


    def sri_to_hex(sri):
        return base64.b64decode(sri.split("-", 1)[1]).hex()


    def instantiate(attr):
        return machine.succeed(
            "nix-instantiate --impure "
            f"--arg busybox 'builtins.storePath \"{BUSYBOX}\"' "
            f"--arg bash 'builtins.storePath \"{BASH}\"' "
            f"-A {attr} {CORPUS}"
        ).strip()


    def oracle_build(drv):
        """Build with real Nix. Returns (ok, outputs|err) where outputs is
        {name: {path, nar_hex, nar_size, references}}."""
        rc, out = machine.execute(f"nix-store --realise {drv} 2>&1")
        if rc != 0:
            return False, out
        outputs = {}
        # `nix derivation show` keys its result by derivation path, but the
        # exact key can differ from the path we passed (observed with
        # ca-derivations enabled); there is exactly one entry either way.
        # Nix 2.3x wraps the result as {"derivations": {...}, "version": 4}
        # and emits store paths without the /nix/store/ prefix; older
        # versions key by full drv path directly. Handle both layouts.
        shown = json.loads(machine.succeed(f"nix derivation show {drv}"))
        if "derivations" in shown:
            shown = shown["derivations"]
        meta = next(iter(shown.values()))
        for name, out_meta in meta["outputs"].items():
            # Input-addressed outputs carry their path in the derivation;
            # floating-CA outputs do not (and the corpus only compares CA
            # entries as documented divergences, never path-by-path), so
            # resolve through `nix-store -q --outputs` only as a fallback
            # for the single-output case.
            path = out_meta.get("path", "") or ""
            if path and not path.startswith("/nix/store/"):
                path = "/nix/store/" + path
            if not path:
                # Floating-CA outputs have no static path and `nix-store -q
                # --outputs` refuses CA derivations entirely; those entries
                # are only ever compared as documented divergences, so skip.
                # stderr is dropped: the refusal is expected and its error
                # text in the console log reads like a test failure.
                rc_out, out_lines = machine.execute(
                    f"nix-store -q --outputs {drv} 2>/dev/null"
                )
                lines = out_lines.strip().splitlines() if rc_out == 0 else []
                if len(lines) == 1:
                    path = lines[0]
                else:
                    continue
            info = json.loads(machine.succeed(f"nix path-info --json {path}"))
            entry = info[path] if isinstance(info, dict) else info[0]
            outputs[name] = {
                "path": path,
                "nar_hex": sri_to_hex(entry["narHash"]),
                "nar_size": entry["narSize"],
                "references": sorted(entry.get("references", [])),
            }
        return True, outputs


    def native_build(name, drv):
        """Build with the native executor stack. Returns the driver report."""
        machine.succeed(f"mkdir -p /tmp/native/{name}")
        rc, out = machine.execute(
            f"differential-driver --drv {drv} --work-dir /tmp/native/{name} "
            f"--sandbox-shell {SANDBOX_SHELL} "
            f"> /tmp/native/{name}/report.json 2> /tmp/native/{name}/driver.log"
        )
        if rc != 0:
            log = machine.succeed(f"cat /tmp/native/{name}/driver.log || true")
            raise AssertionError(f"differential-driver crashed for {name}: {log}")
        return json.loads(machine.succeed(f"cat /tmp/native/{name}/report.json"))


    def dump_native_failure(name, report):
        """Print everything the driver knows about a failed entry, so the
        test log alone explains a failure (no VM re-run needed)."""
        print(f"--- {name}: native report (failure evidence) ---")
        print(f"  classification: {report.get('classification')}")
        print(f"  error_msg:      {report.get('error_msg')}")
        print(f"  glue_error:     {report.get('glue_error')}")
        print(f"  fod_check:      {report.get('fod_check')}")
        for line in report.get("log", {}).get("tail", []):
            print(f"  build| {line}")
        rc_log, driver_log = machine.execute(f"cat /tmp/native/{name}/driver.log")
        if rc_log == 0 and driver_log.strip():
            for line in driver_log.strip().splitlines()[-40:]:
                print(f"  driver| {line}")


    def check_entry(name, meta):
        with subtest(f"corpus entry: {name}"):
            drv = instantiate(name)
            report = native_build(name, drv)
            try:
                run_entry_assertions(name, meta, drv, report)
            except AssertionError:
                dump_native_failure(name, report)
                raise


    def run_entry_assertions(name, meta, drv, report):
            ok, oracle = oracle_build(drv)

            # The @nix side-channel must never reach the forwarded log.
            assert not report["log"]["forwarded_atnix"], (
                f"{name}: an @nix line leaked into the forwarded log"
            )

            expect = meta["expect"]
            if expect == "parity":
                assert ok, f"{name}: oracle build failed unexpectedly:\n{oracle}"
                assert report["classification"] == "success", (
                    f"{name}: native build did not succeed: "
                    f"{report['classification']} {report.get('error_msg')}"
                )
                native_outputs = {o["name"]: o for o in report["outputs"]}
                assert set(native_outputs) == set(oracle), (
                    f"{name}: output sets differ: {sorted(native_outputs)} vs {sorted(oracle)}"
                )
                for oname, native in native_outputs.items():
                    expected = oracle[oname]
                    if native["nar_hash"] != expected["nar_hex"]:
                        # Show the divergence before failing.
                        native_dir = f"/tmp/native/{name}/store/" + expected["path"].split("/")[-1]
                        diff = machine.execute(
                            f"diff -ru {expected['path']} {native_dir} 2>&1 | head -100"
                        )[1]
                        failures.append(f"{name}!{oname}: NAR hash mismatch\n{diff}")
                        continue
                    assert native["nar_size"] == expected["nar_size"], (
                        f"{name}!{oname}: narSize differs"
                    )
                    native_refs = sorted(
                        r.split("/")[-1] for r in native["references"]
                    )
                    oracle_refs = sorted(
                        r.split("/")[-1] for r in expected["references"]
                    )
                    assert native_refs == oracle_refs, (
                        f"{name}!{oname}: references differ: {native_refs} vs {oracle_refs}"
                    )
                    if meta.get("references_must_be_empty"):
                        assert native_refs == [], f"{name}: references must be empty"
                if meta.get("phases"):
                    assert report["log"]["phases"] == meta["phases"], (
                        f"{name}: phases {report['log']['phases']} != {meta['phases']}"
                    )
                    assert report["log"]["atnix_lines"] >= len(meta["phases"])

            elif expect == "both-fail":
                assert not ok, f"{name}: oracle build unexpectedly succeeded"
                assert report["classification"] == meta["rio_status"], (
                    f"{name}: native classification {report['classification']} "
                    f"!= expected {meta['rio_status']} ({report.get('error_msg')})"
                )

            elif expect == "diverge":
                # Documented divergence: record it, assert the native side
                # behaves exactly as documented, and assert the oracle's
                # side too so a silent convergence is also noticed.
                if meta.get("nix") == "succeeds":
                    assert ok, f"{name}: oracle was expected to succeed:\n{oracle}"
                if "rio_glue_error" in meta:
                    assert report["glue_error"] and meta["rio_glue_error"] in report["glue_error"], (
                        f"{name}: expected glue error containing {meta['rio_glue_error']!r}, "
                        f"got {report['glue_error']!r}"
                    )
                elif "rio_status" in meta:
                    assert report["classification"] == meta["rio_status"], (
                        f"{name}: native classification {report['classification']} "
                        f"!= documented {meta['rio_status']}"
                    )
                divergences.append(f"{name}: {meta['note']}")

            else:
                raise AssertionError(f"unknown expectation {expect!r} for {name}")


    for entry_name in sorted(META):
        check_entry(entry_name, META[entry_name])

    print("=== documented divergences (M7 punch list) ===")
    for d in divergences:
        print("  " + d)

    if failures:
        print("=== parity failures ===")
        for f in failures:
            print(f)
        raise AssertionError(f"{len(failures)} corpus entries diverged; see above")
  '';
}
