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

  # Sandbox identity probe: one entry that dumps the builder-observable
  # sandbox ABI facts that must be byte-identical between the CppNix
  # oracle and rio-exec, so any executor refactor that perturbs the
  # environment shows up as a one-line diff here instead of an opaque
  # NAR mismatch in some unrelated package later. Every fact below was
  # verified against a real sandboxed CppNix build (uid/gid 1000/100
  # via the user-namespace mapping, hostname "localhost", domainname
  # "(none)", umask 0022, the nixbld passwd/group lines, NoNewPrivs=1,
  # writable store root, loopback up with 127.0.0.1 + ::1, nested user
  # namespaces allowed, setuid chmod denied while plain chmod works,
  # /bin/sh a regular file).
  #
  # Also probed: /etc/hosts — both sandboxes synthesize the identical
  # "127.0.0.1 localhost / ::1 localhost" file for non-network builds
  # (verified against a real CppNix 2.34 sandboxed build, written by
  # chroot-derivation-builder.cc, and pinned on the rio side by
  # rio-exec's plan tests) — and the NOFILE rlimits: rio-exec pins the
  # daemon-era 1048576 inside every sandbox, and the differential VM
  # sets DefaultLimitNOFILE=1048576 so the oracle arm inherits the same
  # value a real NixOS daemon host delivers.
  #
  # Deliberately NOT probed (would diverge or is delivery-path
  # dependent): full /etc/passwd and /etc/group (CppNix's root/nobody
  # GECOS fields differ from rio's — accepted cosmetic deviation),
  # ulimit -c (CppNix zeroes the soft core limit while the hard limit
  # stays delivery-dependent), and xattr probes (busybox carries no
  # setfattr).
  sandbox-identity = mkDrv "rio-diff-sandbox-identity" ''
    {
      echo "id=$(id -u) $(id -g) $(id -un) $(id -gn)"
      read h < /proc/sys/kernel/hostname; echo "hostname=$h"
      read d < /proc/sys/kernel/domainname; echo "domainname=$d"
      echo "umask=$(umask)"
      echo "etc-hosts=$(tr '\n' ';' < /etc/hosts)"
      echo "ulimit-n=$(ulimit -n)"
      echo "ulimit-Hn=$(ulimit -Hn)"
      echo "passwd-nixbld=$(grep '^nixbld:' /etc/passwd)"
      echo "group-nixbld=$(grep '^nixbld:' /etc/group)"
      echo "no-new-privs=$(grep NoNewPrivs /proc/self/status)"
      if [ -L /bin/sh ]; then echo "bin-sh=symlink"
      elif [ -f /bin/sh ]; then echo "bin-sh=regular"
      else echo "bin-sh=missing"; fi
      if touch /nix/store/.rio-diff-identity-write-probe 2>/dev/null; then
        rm -f /nix/store/.rio-diff-identity-write-probe
        echo "store-root=writable"
      else
        echo "store-root=read-only"
      fi
      if command -v ip > /dev/null; then
        echo "lo-inet=$(ip -o -4 addr show lo | grep -c '127.0.0.1/8')"
        echo "lo-inet6=$(ip -o -6 addr show lo | grep -c '::1/128')"
      else
        echo "lo-inet=skipped"
        echo "lo-inet6=skipped"
      fi
      if ! command -v unshare > /dev/null; then
        echo "nested-userns=skipped"
      elif unshare -r true 2>/dev/null; then
        echo "nested-userns=ok"
      else
        echo "nested-userns=denied"
      fi
      touch scratch-file
      if chmod 0755 scratch-file 2>/dev/null; then echo "plain-chmod=ok"; else echo "plain-chmod=denied"; fi
      if chmod u+s scratch-file 2>/dev/null; then echo "suid-chmod=ok"; else echo "suid-chmod=denied"; fi
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

  # Group/other execute WITHOUT owner execute (0655): CppNix keys the
  # store/NAR executable bit on owner-x only, so canonicalisation must
  # land this file at 0444 (not 0555) in both arms — the NAR hashes
  # diverge if the native side keys on any execute bit.
  group-exec-file = mkDrv "rio-diff-group-exec" ''
    mkdir -p $out
    echo "not actually executable" > $out/tool
    chmod 0655 $out/tool
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
        # Boundary-value discipline (see the header further down): the
        # only entry that byte-compares .attrs.sh also carries the
        # numeric edges. The shell rendering is a 32-bit surface
        # (oracle handleSimpleType: emit iff the f32 view is integral,
        # text = int32 conversion), so bigInt/negBigInt wrap modulo
        # 2^32 (→ 705032704 / -705032704) while .attrs.json keeps the
        # 64-bit values; roundEdgeFloat 16777217.5 is non-integral as a
        # double but its f32 view rounds integral, so BOTH sides must
        # emit its truncation (16777217) rather than skip it.
        bigInt = 5000000000;
        negBigInt = -5000000000;
        roundEdgeFloat = 16777217.5;
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

  # __structuredAttrs + exportReferencesGraph: in structured-attrs mode the
  # closure info is rendered INTO .attrs.json (closure_info_json on the
  # native side, writeStructuredAttrs on the oracle side) instead of a flat
  # registration file. Copying .attrs.json into $out makes the NAR
  # comparison pin the JSON renderer's field set (colon-form nixbase32
  # narHash, narSize, references, closureSize, `valid`, and `ca` for
  # content-addressed members, key ordering) byte-for-byte against the
  # oracle.
  erg-structured =
    mkDrv "rio-diff-erg-structured"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        cp "$NIX_ATTRS_JSON_FILE" ''${outputs[out]}/attrs.json
      ''
      {
        __structuredAttrs = true;
        exportReferencesGraph.refs = [ "${busybox}" ];
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

  # Flat-mode FOD whose bytes match the declared hash but which is made
  # executable: CppNix rejects a flat fixed-output that is not exactly
  # one NON-executable regular file even when the hash matches
  # (derivation-builder.cc, CAFixed branch) — otherwise an executable
  # and a non-executable file with identical bytes would collide on the
  # same store path. The native FOD gate must reject the same shape.
  fod-flat-executable =
    mkDrv "rio-diff-fod-flat-exec"
      ''
        printf '%s' '${flatPayload}' > $out
        chmod 0755 $out
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        outputHash = flatPayloadSha256;
      };

  # Flat-mode FOD whose $out is a symlink (to an input-closure path, so
  # the link target always exists when either side inspects it): CppNix
  # rejects any flat fixed-output that is not a regular file, before
  # looking at content at all. The native side must not follow the
  # symlink and accept the target's bytes.
  fod-flat-symlink =
    mkDrv "rio-diff-fod-flat-symlink"
      ''
        ln -s ${busybox}/bin/busybox $out
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        outputHash = flatPayloadSha256;
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

  # ── Boundary-value discipline ────────────────────────────────────────
  # Entries below this header exist because a porter once read the C++
  # and got a corner wrong. They feed the oracle the BOUNDARY inputs the
  # friendly entries above never sample — env-precedence collisions,
  # numeric edge values, wrong-typed structured attrs — so a parity
  # claim is pinned by executing the oracle, not by re-reading it.
  # Add the discriminating input HERE in the same commit as any future
  # "matches CppNix" code comment.

  # Env-precedence probe: the output bytes ARE the final values of the
  # contested env vars, and the declared FOD hash is computed at eval
  # time from the values the ORACLE's initEnv statement order produces
  # ("1|2|xterm-256color"). The drv attrs adversarially set all three
  # (drv attrs must lose to the forced layers), and impureEnvVars lists
  # NIX_LOG_FD/TERM (impure assignment must lose to initEnv's final
  # NIX_LOG_FD/TERM writes; both unset in the VM environment, so the
  # oracle's getEnv fallback contributes "" either way). Any precedence
  # drift on either side changes the output bytes → FOD hash mismatch →
  # red gate. NIX_OUTPUT_CHECKED is deliberately NOT in impureEnvVars:
  # the forced "1" (set after the drv env) must survive.
  fod-env-precedence =
    mkDrv "rio-diff-fod-env-precedence"
      ''
        printf '%s|%s|%s\n' "$NIX_OUTPUT_CHECKED" "$NIX_LOG_FD" "$TERM" > $out
      ''
      {
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        outputHash = builtins.hashString "sha256" "1|2|xterm-256color\n";
        NIX_OUTPUT_CHECKED = "0";
        NIX_LOG_FD = "7";
        TERM = "dumb";
        impureEnvVars = [
          "NIX_LOG_FD"
          "TERM"
        ];
      };

  # outputChecks.out.maxSize as a FLOAT: nlohmann's implicit uint64
  # conversion truncates (1024.9 → 1024) and the truncated cap is
  # ENFORCED. The output is 4096 bytes > 1024, so both sides fail with
  # the size violation. Pre-fix rio dropped the non-u64 value entirely
  # — no cap at all — and SUCCEEDED while the oracle failed.
  outputchecks-maxsize-float =
    mkDrv "rio-diff-maxsize-float"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        head -c 4096 /dev/zero > ''${outputs[out]}/blob
      ''
      {
        __structuredAttrs = true;
        outputChecks.out.maxSize = 1024.9;
      };

  # outputChecks.out.maxSize as a STRING: nlohmann get<uint64_t> throws
  # (no string→number coercion); the build must fail on both sides.
  # Pre-fix rio silently skipped the cap and succeeded.
  outputchecks-maxsize-string =
    mkDrv "rio-diff-maxsize-string"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        head -c 4096 /dev/zero > ''${outputs[out]}/blob
      ''
      {
        __structuredAttrs = true;
        outputChecks.out.maxSize = "1024";
      };

  # outputChecks list with a wrong-typed element: getStringList throws
  # on the 42 — the element is never dropped. Pre-fix rio filter_map'd
  # it away, silently widening the allowed set.
  outputchecks-list-wrong-type =
    mkDrv "rio-diff-oc-list-wrong-type"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo ok > ''${outputs[out]}/f
      ''
      {
        __structuredAttrs = true;
        outputChecks.out.allowedReferences = [
          "out"
          42
        ];
      };

  # outputChecks.<name> that is not an object: getObject throws.
  outputchecks-spec-not-object =
    mkDrv "rio-diff-oc-spec-not-object"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo ok > ''${outputs[out]}/f
      ''
      {
        __structuredAttrs = true;
        outputChecks.out = "not-an-object";
      };

  # unsafeDiscardReferences flag that is a string, not a bool:
  # getBoolean throws. Pre-fix rio unwrap_or(false)'d it — the discard
  # silently OFF where the oracle fails the build.
  unsafe-discard-wrong-type =
    mkDrv "rio-diff-unsafe-discard-wrong-type"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo ok > ''${outputs[out]}/f
      ''
      {
        __structuredAttrs = true;
        unsafeDiscardReferences.out = "true";
      };

  # exportReferencesGraph value that is a number: the oracle's flatten
  # throws ("'exportReferencesGraph' value is not an array or a
  # string"); rio rejects in the request glue before any build runs.
  erg-wrong-type =
    mkDrv "rio-diff-erg-wrong-type"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo ok > ''${outputs[out]}/f
      ''
      {
        __structuredAttrs = true;
        exportReferencesGraph.refs = 42;
      };

  # exportReferencesGraph with a NESTED array: CppNix-legal (flatten
  # recurses), so the exported closure must be byte-identical to the
  # flat spelling. Pre-fix rio silently emptied nested arrays — an
  # empty closure file where the oracle exports busybox's closure.
  erg-nested-array =
    mkDrv "rio-diff-erg-nested-array"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        cp "$NIX_ATTRS_JSON_FILE" ''${outputs[out]}/attrs.json
      ''
      {
        __structuredAttrs = true;
        exportReferencesGraph.refs = [ [ "${busybox}" ] ];
      };

  # ── Floating content-addressed derivations ───────────────────────────
  # These entries exercise the native result pipeline's CA finalization:
  # scratch-path builds, apply-rewrites-before-hashing ordering,
  # self-reference modulo hashing, and sibling-reference remapping —
  # compared byte-for-byte against the oracle's realised CA paths.
  # Per-entry expectations live in scenarios/differential.nix entryMeta
  # (the single source of truth per the header above) — this file states
  # none.

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

  # Floating-CA + unsafeDiscardReferences with a textual self-reference:
  # the output embeds its own path, but the discard empties the recorded
  # reference set — so the `:self` fingerprint flag must come from the
  # (empty) recorded references while the content hash is still computed
  # modulo the scratch hash and the embedded path is still rewritten.
  # Both arms must mint the same final path, and the registered
  # reference set must be empty.
  ca-discard-self =
    mkDrv "rio-diff-ca-discard-self"
      ''
        . "$NIX_ATTRS_SH_FILE"
        mkdir -p ''${outputs[out]}
        echo "''${outputs[out]}" > ''${outputs[out]}/self
      ''
      {
        __structuredAttrs = true;
        __contentAddressed = true;
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
        unsafeDiscardReferences.out = true;
      };

  # The flat-mode sibling of ca-discard-self: the output is a single
  # regular file whose bytes embed the output's own path, with the
  # recorded references discarded. CppNix still applies rewriteOutput
  # before the flat hash, so the native side must hash the rewritten
  # bytes too — path, descriptor, and registered (empty) reference set
  # must match the oracle.
  ca-discard-self-flat =
    mkDrv "rio-diff-ca-discard-self-flat"
      ''
        . "$NIX_ATTRS_SH_FILE"
        echo "I live at ''${outputs[out]}" > ''${outputs[out]}
      ''
      {
        __structuredAttrs = true;
        __contentAddressed = true;
        outputHashMode = "flat";
        outputHashAlgo = "sha256";
        unsafeDiscardReferences.out = true;
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

  # An @nix frame far larger than the executor's 1 MiB pending-line cap:
  # the splitter force-emits it in fragments, and the log filter must
  # consume EVERY fragment (head classifies, tails inherit) — none of
  # the frame body may reach the forwarded log. The driver's report
  # carries a filter-independent leak oracle (atnix_tail_forwarded) and
  # a split counter (split_lines); the harness asserts the former is
  # false and the latter nonzero, so raising the splitter cap above
  # this frame's size fails the entry instead of passing it vacuously.
  # Built by doubling: 64 bytes shifted left 16 times = 4 MiB of
  # payload, no shell loops over megabyte strings byte-by-byte.
  oversized-atnix-frame = mkDrv "rio-diff-oversized-atnix" ''
    big=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    i=0
    while [ $i -lt 16 ]; do
      big=$big$big
      i=$((i+1))
    done
    printf '@nix {"action":"msg","level":7,"msg":"%s"}\n' $big >&2
    echo "after the frame" >&2
    echo done > $out
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
