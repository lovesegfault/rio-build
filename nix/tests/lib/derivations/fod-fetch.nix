# busybox-wget FOD. In-VM nix-build target for the fod-proxy scenario.
#
# NOT builtin:fetchurl — that needs system="builtin" which the vmtest-full
# workerPool.systems=[x86_64-linux] doesn't advertise. busybox wget honors
# http_proxy env (the whole point: rio-builder's is_fod gate sets it on the
# daemon, Nix's FOD sandbox passes it through to the builder).
#
# The proxy chain under test:
#   1. executor/mod.rs:462 — is_fod gate → fod_proxy_url passed to spawn
#   2. spawn.rs:158 — http_proxy/https_proxy set on daemon env
#   3. Nix FOD sandbox — passes http_proxy through to builder (Nix spec)
#   4. busybox wget — reads http_proxy → connects to squid, NOT direct
#   5. squid — dstdomain ACL → TCP_MISS (forward) or TCP_DENIED/403
#
# Evaluated IN THE VM via nix-build. `url` and `sha256` are `--argstr`-ed
# at nix-build time so the scenario controls both the allowed and denied
# cases from one file.
{
  busybox,
  url,
  sha256,
}:
derivation {
  name = "rio-fod-fetch";
  builder = "${busybox}/bin/sh";
  # busybox wget: -q quiet, -O $out, -T 15 network timeout. The timeout
  # bounds any squid-side hang (DNS resolution, upstream connect) — the
  # denied case's whole point is that squid returns 403 FAST, so 15s is
  # way more than needed for success AND way less than globalTimeout so
  # a hang surfaces as a clean build-failure instead of a test timeout.
  # Exit nonzero on HTTP error (403 from squid on deny → build fails).
  args = [
    "-c"
    "${busybox}/bin/busybox wget -q -T 15 -O $out ${url}"
  ];
  system = builtins.currentSystem;

  # FOD markers. outputHashMode=flat: sha256 of the raw file bytes (what
  # the scenario's `builtins.hashString "sha256" content` produces).
  # recursive would be NAR-hash — different value for the same bytes.
  outputHashMode = "flat";
  outputHashAlgo = "sha256";
  outputHash = sha256;

  # CRITICAL: without this, Nix's FOD sandbox strips http_proxy even
  # though it's a FOD. `impureEnvVars` is the list of env vars Nix
  # passes through from the daemon's env to the builder's env. It's
  # NOT automatic for FODs — nixpkgs `fetchurl` sets
  # `impureEnvVars = lib.fetchers.proxyImpureEnvVars` explicitly.
  # Without this, wget connects direct (k3s-server resolves via
  # CoreDNS NodeHosts from inside the worker pod) and the test
  # passes build-succeeds but fails the squid-log-grep.
  impureEnvVars = [
    "http_proxy"
    "https_proxy"
    "HTTP_PROXY"
    "HTTPS_PROXY"
  ];
}
