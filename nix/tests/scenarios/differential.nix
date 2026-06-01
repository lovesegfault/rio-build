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

  # 32-bit busybox: every syscall in the i686-* entries goes through
  # the i386 ABI, exercising the multi-ABI seccomp filter. The glibc
  # (non-static) i686 build is used because its toolchain is
  # substitutable — the static musl32 variants would bootstrap a cross
  # toolchain from source on every cold cache.
  busybox32 = pkgs.pkgsi686Linux.busybox;

  # Real stdenv probe: a genuine stdenv.mkDerivation (setup.sh, phases,
  # cc-wrapper, fixupPhase) instead of an inline-busybox script. The
  # expression lives in its own corpus file and is instantiated INSIDE
  # the VM (so no derivation closure has to be exported from the host);
  # the host evaluates the same file only to reach `.inputDerivation`,
  # whose closure ships the probe's build-time dependencies into the VM
  # store. See the header of differential-stdenv-probe.nix.
  stdenvProbeFile = ../lib/derivations/differential-stdenv-probe.nix;
  stdenvProbe =
    (import stdenvProbeFile {
      pkgsPath = pkgs.path;
      inherit (pkgs.stdenv.hostPlatform) system;
    }).stdenv-probe;

  # Per-entry expectations, consumed by the testScript.
  #
  #   expect = "parity"      → both succeed; NAR hashes + references equal
  #   expect = "both-fail"   → both fail; the native side must report
  #                            `rio_status` (or, for rejections that
  #                            happen in the request glue before any
  #                            build runs, a glue error containing
  #                            `rio_glue_error`)
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
    symlink-input-consumer = {
      expect = "parity";
    };
    build-user = {
      expect = "parity";
    };
    sandbox-identity = {
      expect = "parity";
    };
    hard-link-pair = {
      expect = "parity";
    };
    hard-link-across-outputs = {
      expect = "parity";
    };
    inner-group-writable = {
      expect = "parity";
    };
    group-writable-root = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    group-exec-file = {
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
      expect = "parity";
    };
    erg-subpath = {
      expect = "parity";
    };
    erg-structured = {
      expect = "parity";
    };
    disallowed-requisites = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    illegal-ref-specifier = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    builtin-fetchurl-no-hash = {
      expect = "both-fail";
      # Rejected by the request glue (no outputHash → no network grant);
      # the driver reports the glue error instead of a classification.
      rio_glue_error = "outputHash";
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
    fod-flat-executable = {
      # CppNix: "output path ... should be a non-executable regular
      # file" (flat CA shape rule); the native FOD gate enforces the
      # same shape, so the matching content hash must not save it.
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    fod-flat-symlink = {
      # CppNix rejects a non-regular flat fixed output before hashing;
      # the native side must not follow the symlink to its target.
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    fod-mismatch = {
      expect = "both-fail";
      rio_status = "OutputRejected";
    };
    fod-unknown-algo = {
      expect = "diverge";
      nix = "succeeds";
      # Since the declared-hash validation moved into the request glue,
      # the unverifiable algo is rejected before execution (closer to
      # CppNix, which refuses the algo when parsing the drv) — so the
      # native side reports a glue error, not a build classification.
      rio_glue_error = "unsupported outputHashAlgo";
      note = "rio is fail-closed on unverifiable outputHashAlgo by design";
    };
    fod-builder-fails = {
      expect = "both-fail";
      rio_status = "TransientFailure";
    };
    # The output bytes are the contested env values; the declared hash
    # is the oracle-order answer, so success on both sides IS the
    # precedence proof (any drift = FOD hash mismatch on that side).
    fod-env-precedence = {
      expect = "parity";
    };
    ca-multi-output = {
      # M6b floating-CA finalization is merged: the native side must
      # now realize the same content-addressed paths as the oracle.
      expect = "parity";
    };
    ca-selfref = {
      # M6b floating-CA finalization is merged: the native side must
      # now realize the same content-addressed paths as the oracle.
      expect = "parity";
    };
    ca-structured = {
      # M6b floating-CA finalization is merged: the native side must
      # now realize the same content-addressed paths as the oracle.
      expect = "parity";
    };
    ca-discard-self = {
      # Self-reference textually present but discarded: both arms must
      # mint the path without the `:self` fingerprint flag and register
      # an empty reference set.
      expect = "parity";
      references_must_be_empty = true;
    };
    ca-discard-self-flat = {
      # Flat-mode variant: the single-file output embeds its own path,
      # references discarded. The flat hash must be computed over the
      # rewritten bytes (CppNix runs rewriteOutput before hashing in
      # flat mode too), so both arms mint the same path.
      expect = "parity";
      references_must_be_empty = true;
    };
    # ── Heavyweight entries: 32-bit ABI + real stdenv ──────────────────
    i686-trivial = {
      expect = "parity";
    };
    i686-setuid-attempt = {
      expect = "parity";
    };
    i686-multi-output = {
      expect = "parity";
    };
    stdenv-probe = {
      expect = "parity";
      corpus = "stdenv";
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-differential";
  # Two real sandboxed builds per corpus entry plus the driver's own
  # closure copies; generous but bounded. The stdenv probe (a real cc
  # invocation per side) and the i686 entries are the heavy tail.
  globalTimeout = 7200;

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation = {
        cores = 4;
        memorySize = 6144;
        diskSize = 16384;
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
          busybox32
          sandboxShell
          "${corpusFile}"
          "${stdenvProbeFile}"
          # The nixpkgs source tree, so the VM can instantiate the
          # stdenv probe itself …
          "${pkgs.path}"
          # … and the OUTPUTS of everything the probe builds with
          # (cc-wrapper, binutils, glibc, coreutils, …), which is
          # exactly what inputDerivation's closure carries. The probe's
          # own output is deliberately NOT shipped — both sides must
          # genuinely build it inside the VM.
          stdenvProbe.inputDerivation
        ];
      };

      environment.systemPackages = [
        rio-workspace
        pkgs.nix
      ];

      # Both arms are launched from the test backdoor service, so both
      # inherit ITS file-descriptor limits. The systemd default (1024
      # soft) is an artifact of the harness, not of any real deployment:
      # daemon-era builders ran under nix-daemon.service's
      # LimitNOFILE=1048576, and rio-exec now pins the same value inside
      # the sandbox. Give the whole VM that limit so the oracle arm is
      # representative of a real NixOS daemon host and the
      # sandbox-identity corpus entry can compare `ulimit -n` across the
      # two arms.
      systemd.settings.Manager.DefaultLimitNOFILE = 1048576;

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
    BUSYBOX32 = "${busybox32}"
    CORPUS_STDENV = "${stdenvProbeFile}"
    PKGS_PATH = "${pkgs.path}"
    META = json.loads('${builtins.toJSON entryMeta}')

    divergences = []
    failures = []


    def sri_to_hex(sri):
        return base64.b64decode(sri.split("-", 1)[1]).hex()


    def instantiate(attr, meta):
        if meta.get("corpus") == "stdenv":
            # The probe file does a pristine `import <nixpkgs>` itself;
            # instantiation takes a while the first time (full stdenv
            # eval) but builds against dependencies already shipped via
            # additionalPaths.
            return machine.succeed(
                "nix-instantiate --impure "
                f"--arg pkgsPath 'builtins.storePath \"{PKGS_PATH}\"' "
                f"-A {attr} {CORPUS_STDENV}"
            ).strip()
        return machine.succeed(
            "nix-instantiate --impure "
            f"--arg busybox 'builtins.storePath \"{BUSYBOX}\"' "
            f"--arg bash 'builtins.storePath \"{BASH}\"' "
            f"--arg busybox32 'builtins.storePath \"{BUSYBOX32}\"' "
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
            # floating-CA outputs only exist in the realisation DB after
            # the build, so resolve those through `nix build --json` on the
            # already-realised derivation (a cheap lookup). The CA corpus
            # entries assert full parity, so every output must resolve.
            path = out_meta.get("path", "") or ""
            if path and not path.startswith("/nix/store/"):
                path = "/nix/store/" + path
            if not path:
                built = json.loads(
                    machine.succeed(f"nix build --json --no-link '{drv}^*'")
                )
                path = built[0].get("outputs", {}).get(name, "")
                if not path:
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
            drv = instantiate(name, meta)
            # stdenv-probe is exempt from the input pre-realisation: its
            # toolchain inputs are pre-seeded via additionalPaths
            # (inputDerivation closure) with exactly the outputs the
            # build uses (bash[out], stdenv[out]). `nix-store --realise`
            # on those input drvs would demand ALL their outputs
            # (dev/man/doc/info/debug too), which this network-less,
            # substituter-less VM could only satisfy by building bash
            # and its whole bootstrap from source. Every other entry's
            # inputs are single-output and fully shipped (or are sibling
            # corpus entries), so the pre-realise stays for them.
            if meta.get("corpus") != "stdenv":
                # Later corpus entries depend on earlier entries' outputs
                # (e.g. erg-with-drv exports the trivial entry's graph), and
                # the native driver computes its input closure with
                # `nix-store -qR`, which requires every input to be a valid
                # store path. Realise the entry's input derivations first so
                # the native side sees the same materialized inputs the
                # oracle build would create on demand.
                refs = machine.succeed(f"nix-store -q --references {drv}")
                input_drvs = " ".join(r for r in refs.split() if r.endswith(".drv"))
                if input_drvs:
                    machine.succeed(f"nix-store --realise {input_drvs} >/dev/null")
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
                    # The realized path must match, not just the content:
                    # for floating-CA outputs an identical NAR with a wrong
                    # computed content-address is exactly the registration
                    # bug this harness exists to catch.
                    assert native["store_path"] == expected["path"], (
                        f"{name}!{oname}: realized store path differs: "
                        f"{native['store_path']} != {expected['path']}"
                    )
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
                if "rio_glue_error" in meta:
                    # Rejected by the request glue before any build ran:
                    # the driver reports glue_error and no classification.
                    assert report["glue_error"] and meta["rio_glue_error"] in report["glue_error"], (
                        f"{name}: expected glue error containing {meta['rio_glue_error']!r}, "
                        f"got {report['glue_error']!r}"
                    )
                else:
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
