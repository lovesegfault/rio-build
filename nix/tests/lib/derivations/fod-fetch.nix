# busybox-wget FOD. In-VM nix-build target for the fod-proxy scenario.
#
# NOT builtin:fetchurl — that needs system="builtin" which the vmtest-full
# workerPool.systems=[x86_64-linux] doesn't advertise. busybox wget honors
# http_proxy env (the whole point: rio-worker's is_fod gate sets it on the
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
  # busybox wget: -q quiet (no progress bar noise in build log), -O stdout
  # → redirected to $out. Exit nonzero on HTTP error (403 from squid on
  # deny → build fails, which is the scenario's denied-case assertion).
  args = [
    "-c"
    "${busybox}/bin/busybox wget -q -O $out ${url}"
  ];
  system = builtins.currentSystem;

  # FOD markers. outputHashMode=flat: sha256 of the raw file bytes (what
  # the scenario's `builtins.hashString "sha256" content` produces).
  # recursive would be NAR-hash — different value for the same bytes.
  outputHashMode = "flat";
  outputHashAlgo = "sha256";
  outputHash = sha256;
}
