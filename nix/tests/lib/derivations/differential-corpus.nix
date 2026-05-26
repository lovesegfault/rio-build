# Differential-harness corpus (vm-differential-standalone).
#
# Every attribute is one derivation that gets built TWICE inside the VM —
# once by real Nix (`nix-build`, the oracle) and once by the native
# executor stack (`differential-driver`: request glue → rio-exec sandbox →
# result pipeline) — and compared: NAR hashes, reference sets, failure
# classifications, and (for the entries that copy them into $out) the
# materialized environment and .attrs.{json,sh} bytes.
#
# Each entry's comment names the failure mode it exists to discriminate
# (DESIGN.md §5.1). Per-entry expectations (parity / expected-failure /
# known-divergence) live in scenarios/differential.nix, next to the
# comparison logic — this file is pure derivations.
#
# Evaluated IN THE VM via `nix-instantiate --impure` with `--arg
# busybox / bash / busybox32 'builtins.storePath "…"'`. Keep every
# builder script inline (args = [ "-c" ... ]) — source-file inputs
# would land in the VM's writable upper store layer, which the driver's
# closure copy handles fine, but inline scripts keep the input closure
# to exactly the declared builders, which keeps the per-entry copy
# fast.
#
# The 32-bit (i686-busybox) entries at the bottom of this file and the
# real-stdenv probe (differential-stdenv-probe.nix, instantiated in the
# VM by the same scenario) are the heavyweight tail of the corpus: they
# make the check slower, but they run in the same merge-gate scenario
# as everything else (checks.x86_64-linux.vm-differential-standalone).
{
  busybox,
  bash,
  busybox32,
}:
let
  sh = "${bash}/bin/bash";

  mkDrv =
    name: script: extra:
    derivation (
      {
        inherit name;
        system = builtins.currentSystem;
        builder = sh;
        args = [
          "-c"
          script
        ];
        PATH = "${busybox}/bin";
      }
      // extra
    );

  # 32-bit variant: builder and PATH come from an i686 busybox
  # (pkgsi686Linux — glibc, non-static; its toolchain is substitutable,
  # unlike the from-source musl32 static variants), so every syscall in
  # the i686-* entries goes through the i386 ABI — the multi-ABI seccomp
  # filter and 32-bit personality handling get no coverage from the
  # x86_64 entries.
  sh32 = "${busybox32}/bin/sh";

  mkDrv32 =
    name: script: extra:
    derivation (
      {
        inherit name;
        # The derivation still targets the host platform — only the
        # builder binary is 32-bit. Real Nix needs no extra-platforms
        # entry this way, and the native executor sees an ordinary
        # x86_64-linux job whose process happens to make i386-ABI
        # syscalls.
        system = builtins.currentSystem;
        builder = sh32;
        args = [
          "-c"
          script
        ];
        PATH = "${busybox32}/bin";
      }
      // extra
    );

  # Flat-mode FOD payload + its hash, computed at eval time so content
  # and declared hash can never drift.
  flatPayload = "rio-differential flat payload\n";
  flatPayloadSha256 = builtins.hashString "sha256" flatPayload;

  # Recursive-mode FOD: NAR hash of a directory containing exactly
  # `payload` = "rio-differential fixed payload\n" (0644). Precomputed
  # with `nix hash path` on an identical tree; if the script below
  # changes, recompute the hash the same way.
  recursivePayloadNar = "sha256-0wcFwKpF9x0WvKUR3UrbkuhyheXpQufJFkhZyFneOYk=";
in
rec {
  # ── Plain builds ─────────────────────────────────────────────────────

  # Trivial single-output build: the baseline "anything works at all"
  # entry; catches gross env/mount/exec divergence.
  trivial = mkDrv "rio-diff-trivial" ''
    echo "hello from the differential corpus" > $out
  '' { };

  # Multi-output input-addressed derivation with an inter-output
  # reference (out → dev): exercises output ordering, per-output
  # reference scanning, and the topological order both sides must agree
  # on.
  multi-output =
    mkDrv "rio-diff-multi-output"
      ''
        mkdir -p $dev $out
        echo "development half" > $dev/marker
        echo "$dev" > $out/link-to-dev
      ''
      {
        outputs = [
          "out"
          "dev"
        ];
      };

  # Environment materialization order: the derivation sets TMPDIR/TERM
  # in its own attrs (which Nix forces back to the sandbox values) and
  # dumps the sorted environment into $out. Catches the forced-after
  # ordering, NIX_BUILD_TOP/PWD values, and any stray variables either
  # side adds.
  env-dump =
    mkDrv "rio-diff-env-dump"
      ''
        env | sort > $out
      ''
      {
        TMPDIR = "/should-be-overridden";
        TERM = "definitely-not-the-default";
        SOME_USER_ATTR = "survives";
      };

  # Setuid denial: the seccomp purity filter must EPERM the chmod on
  # both sides while the build itself still succeeds; the recorded exit
  # code in $out must match.
  setuid-attempt = mkDrv "rio-diff-setuid" ''
    touch $out
    if chmod 4755 $out 2>/dev/null; then
      echo "setuid-succeeded" > $out
    else
      echo "setuid-denied rc=$?" > $out
    fi
  '' { };

  # /dev/ptmx must be openable (devpts ptmxmode=0666): pty-allocating
  # test suites depend on it.
  ptmx-open = mkDrv "rio-diff-ptmx" ''
    if head -c 0 /dev/ptmx 2>/dev/null; then
      echo "ptmx-ok" > $out
    else
      echo "ptmx-failed rc=$?" > $out
    fi
  '' { };

  # Output that is a bare symlink: the ownership/mode checks must exempt
  # symlinks (always 0777) on both sides.
  symlink-output = mkDrv "rio-diff-symlink" ''
    ln -s /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-target $out
  '' { };

  # A build that writes a stray path into the store besides $out: the
  # stray must not be registered or referenced by either side.
  stray-store-path = mkDrv "rio-diff-stray" ''
    mkdir -p /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-rio-diff-stray-scratch
    echo leftover > /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-rio-diff-stray-scratch/file
    echo "real output" > $out
  '' { };

  # Consumes an input store path whose ROOT is itself a symlink
  # (symlink-output above): the sandbox must present that input as the
  # symlink it is — not a host-resolved copy of its target — exactly
  # like CppNix's doBind. The consumer records link-ness and the link
  # target, so a sandbox that resolves (or fails to materialize) the
  # symlink diverges in $out.
  symlink-input-consumer = mkDrv "rio-diff-symlink-consumer" ''
    {
      if [ -L ${symlink-output} ]; then echo "input-is-symlink"; else echo "input-not-symlink"; fi
      readlink ${symlink-output} || echo "readlink-failed"
    } > $out
  '' { };

  # Build-user identity as observed from inside the sandbox: CppNix's
  # sandbox /etc/passwd and /etc/group name uid/gid as nixbld/nixbld,
  # and builds do embed `id`/`whoami` output (configure scripts, test
  # suites). The native sandbox must agree byte-for-byte.
  build-user = mkDrv "rio-diff-build-user" ''
    {
      id -un
      id -gn
    } > $out
  '' { };

  # Hard-linked pair inside one output: canonicalisation chowns the
  # first name to root, then must accept the second name of the same
  # inode (CppNix's inodesSeen escape) instead of rejecting it as
  # foreign-owned.
  hard-link-pair = mkDrv "rio-diff-hard-link-pair" ''
    mkdir -p $out
    echo "linked content" > $out/a
    ln $out/a $out/b
  '' { };

  # Hard link ACROSS two outputs of the same derivation: the inode is
  # first seen while processing one output and reappears in the other;
  # both CppNix and the native pipeline share the seen-inode set across
  # a build's outputs, so this must succeed on both sides.
  hard-link-across-outputs =
    mkDrv "rio-diff-hard-link-across"
      ''
        mkdir -p $out $dev
        echo "shared content" > $out/shared
        ln $out/shared $dev/shared
      ''
      {
        outputs = [
          "out"
          "dev"
        ];
      };

  # Group-writable file INSIDE the output: canonicalisation normalizes
  # inner modes (to 0444 here); only the output root is subject to the
  # reject-don't-fix rule. Both sides must succeed.
  inner-group-writable = mkDrv "rio-diff-inner-writable" ''
    mkdir -p $out/sub
    echo "writable for the group" > $out/sub/file
    chmod 664 $out/sub/file
  '' { };

  # Group-writable OUTPUT ROOT: CppNix rejects the build output
  # ("suspicious ownership or permission"), and so must the native
  # result pipeline.
  group-writable-root = mkDrv "rio-diff-writable-root" ''
    mkdir -p $out
    echo x > $out/file
    chmod 775 $out
  '' { };

  # ── structuredAttrs / passAsFile / placeholders ──────────────────────

  # __structuredAttrs: .attrs.json and .attrs.sh are copied into $out so
  # the harness can byte-compare them against what real Nix wrote.
  structured-attrs =
    mkDrv "rio-diff-structured"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        cp "$NIX_ATTRS_JSON_FILE" ''${outputs[out]}/attrs.json
        cp "$NIX_ATTRS_SH_FILE" ''${outputs[out]}/attrs.sh
        env | sort | grep -v -e '^NIX_ATTRS_' > ''${outputs[out]}/environ
      ''
      {
        __structuredAttrs = true;
        anInt = 42;
        aList = [
          "x"
          "y z"
        ];
        nested = {
          deep = true;
        };
      };

  # passAsFile with an embedded output placeholder: the placeholder must
  # be rewritten to the real output path inside the materialized file.
  pass-as-file =
    mkDrv "rio-diff-passasfile"
      ''
        mkdir -p $out
        cp "$messagePath" $out/message
      ''
      {
        passAsFile = [ "message" ];
        message = "the output will live at ${placeholder "out"}";
      };

  # exportReferencesGraph whose exported closure contains a .drv file:
  # the closure-expansion rule. Both sides expand the graph with the
  # .drv's output closures (CppNix `exportReferences`; the glue's
  # ClosureIndex mirrors it), so the registration file — and therefore
  # the output NAR — must be byte-identical. No fallback on the cp: a
  # missing graph file must fail the build, not converge on a stub.
  erg-with-drv =
    mkDrv "rio-diff-erg-drv"
      ''
        mkdir -p $out
        cp refs $out/refs
      ''
      {
        exportReferencesGraph = [
          "refs"
          trivial.drvPath
        ];
      };

  # exportReferencesGraph whose target is a SUB-PATH of a store path
  # ("${pkg}/bin/tool" style, as used by nixos module system images):
  # CppNix runs toStorePath() on the target first, so the exported
  # closure is the containing store path's closure. The registration
  # file is copied into $out, so the NAR comparison pins the
  # normalization byte-for-byte.
  erg-subpath =
    mkDrv "rio-diff-erg-subpath"
      ''
        mkdir -p $out
        cp refs $out/refs
      ''
      {
        exportReferencesGraph = [
          "refs"
          "${busybox}/bin/sh"
        ];
      };

  # ── Output policy checks ─────────────────────────────────────────────

  # disallowedRequisites violation: the output references busybox, which
  # is disallowed — both sides must fail with an output-rejection.
  disallowed-requisites = mkDrv "rio-diff-disallowed-req" ''
    mkdir -p $out
    echo "${busybox}" > $out/forbidden-ref
  '' { disallowedRequisites = [ busybox ]; };

  # allowedReferences entry that is neither a store path nor an output
  # name: CppNix raises "illegal reference specifier" and fails the
  # build; the native policy checks must reject it the same way instead
  # of silently treating it as an unmatchable literal.
  illegal-ref-specifier = mkDrv "rio-diff-illegal-spec" ''
    echo "no references at all" > $out
  '' { allowedReferences = [ "definitely-not-a-store-path-or-output" ]; };

  # builtin:fetchurl WITHOUT a fixed-output hash: CppNix refuses to run
  # it ("must be a fixed-output derivation") and rio's request glue must
  # reject it before any network-enabled request exists — this is the
  # SSRF gate for Builder pods. The URL is never contacted on either
  # side.
  builtin-fetchurl-no-hash = derivation {
    name = "rio-diff-fetchurl-nohash";
    system = builtins.currentSystem;
    builder = "builtin:fetchurl";
    url = "http://127.0.0.1:1/never-fetched";
    PATH = "${busybox}/bin";
  };

  # structuredAttrs outputChecks.out.maxSize violation: the output is
  # bigger than the declared cap — both sides must fail.
  outputchecks-maxsize =
    mkDrv "rio-diff-maxsize"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        head -c 4096 /dev/zero > ''${outputs[out]}/blob
      ''
      {
        __structuredAttrs = true;
        outputChecks.out.maxSize = 1024;
      };

  # unsafeDiscardReferences: the output embeds an input store path but
  # the recorded reference set must be EMPTY on both sides.
  unsafe-discard =
    mkDrv "rio-diff-unsafe-discard"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo "${busybox}" > ''${outputs[out]}/embedded-input
      ''
      {
        __structuredAttrs = true;
        unsafeDiscardReferences.out = true;
      };

  # ── Fixed-output derivations (script FODs, no network needed) ────────

  # Flat-mode FOD with the correct declared hash.
  fod-flat =
    mkDrv "rio-diff-fod-flat"
      ''
        printf '%s' '${flatPayload}' > $out
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        outputHash = flatPayloadSha256;
      };

  # Recursive-mode FOD with the correct declared hash.
  fod-recursive =
    mkDrv "rio-diff-fod-recursive"
      ''
        mkdir -p $out
        printf 'rio-differential fixed payload\n' > $out/payload
      ''
      {
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
        outputHash = recursivePayloadNar;
      };

  # FOD whose produced content does NOT match the declared hash: both
  # sides must fail the build (hash mismatch / output rejection).
  fod-mismatch =
    mkDrv "rio-diff-fod-mismatch"
      ''
        printf 'this is not the declared payload\n' > $out
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        outputHash = flatPayloadSha256;
      };

  # FOD declaring an outputHashAlgo rio deliberately does not verify:
  # real Nix supports md5 and will succeed; rio is fail-closed and must
  # reject. Recorded as a deliberate divergence, asserted on the rio
  # side only.
  fod-unknown-algo =
    mkDrv "rio-diff-fod-md5"
      ''
        printf '%s' '${flatPayload}' > $out
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "md5";
        outputHash = builtins.hashString "md5" flatPayload;
      };

  # Non-builtin FOD whose builder exits non-zero: a network-dependent
  # build failing must classify as transient on the rio side (the
  # scheduler retries it elsewhere) and as a plain failure for Nix.
  fod-builder-fails =
    mkDrv "rio-diff-fod-fails"
      ''
        echo "pretending the network is down" >&2
        exit 7
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        outputHash = flatPayloadSha256;
      };

  # ── Floating content-addressed derivations ───────────────────────────
  # Native-side CA finalization lands with the M6b milestone; until then
  # these entries are recorded as known divergences (the native side
  # builds them at scratch paths and cannot produce the final CA path).

  # Multi-output floating-CA where one output references its sibling:
  # the apply-rewrites-before-hashing order (the self-reference-only
  # case cannot catch it).
  ca-multi-output =
    mkDrv "rio-diff-ca-multi"
      ''
        mkdir -p $lib $out
        echo "library half" > $lib/marker
        echo "$lib" > $out/uses-lib
      ''
      {
        __contentAddressed = true;
        outputs = [
          "out"
          "lib"
        ];
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
      };

  # Floating-CA with a self-reference.
  ca-selfref =
    mkDrv "rio-diff-ca-selfref"
      ''
        mkdir -p $out
        echo "$out" > $out/self
      ''
      {
        __contentAddressed = true;
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
      };

  # __structuredAttrs + floating-CA combination: the inputRewrites
  # application points under structuredAttrs.
  ca-structured =
    mkDrv "rio-diff-ca-structured"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo "ca + structuredAttrs" > ''${outputs[out]}/marker
      ''
      {
        __structuredAttrs = true;
        __contentAddressed = true;
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
      };

  # setPhase emission: stdenv-style phase reporting through the @nix
  # side-channel. The native side's log filter must consume the @nix
  # line (no @nix prefix may reach the forwarded log) and report the
  # phase; the build output must be identical on both sides.
  phase-reporter = mkDrv "rio-diff-phases" ''
    echo '@nix {"action":"setPhase","phase":"buildPhase"}' >&2
    echo "built during buildPhase" > $out
    echo '@nix {"action":"setPhase","phase":"installPhase"}' >&2
  '' { };

  # ── 32-bit (i686) builds ─────────────────────────────────────────────

  # Baseline 32-bit build: catches gross divergence in exec/personality
  # handling (a seccomp filter that kills i386-ABI syscalls shows up
  # here as SIGSYS on the native side only).
  i686-trivial = mkDrv32 "rio-diff-i686-trivial" ''
    {
      echo "hello from a 32-bit builder"
      uname -m
    } > $out
  '' { };

  # Setuid denial through the 32-bit ABI: the purity filter must EPERM
  # the i386 fchmodat/chmod exactly like the x86_64 one (setuid-attempt
  # above only proves the 64-bit branch). The build itself still
  # succeeds; the recorded result must match the oracle byte-for-byte.
  i686-setuid-attempt = mkDrv32 "rio-diff-i686-setuid" ''
    touch $out
    if chmod 4755 $out 2>/dev/null; then
      echo "setuid-succeeded" > $out
    else
      echo "setuid-denied rc=$?" > $out
    fi
  '' { };

  # Multi-output + inter-output reference, 32-bit edition: output
  # registration order and reference scanning are ABI-independent, but
  # the path-scanning runs against NARs produced by 32-bit tools (mkdir,
  # echo, cp from the i686 busybox) — cheap to include, catches "works
  # on 64-bit coreutils only" assumptions in canonicalisation.
  i686-multi-output =
    mkDrv32 "rio-diff-i686-multi-output"
      ''
        mkdir -p $dev $out
        echo "development half (32-bit)" > $dev/marker
        echo "$dev" > $out/link-to-dev
      ''
      {
        outputs = [
          "out"
          "dev"
        ];
      };
}
