"""Substitute every coverage lcov from the binary cache and upload each
to Codecov under its own flag.

The {name: outPath} map comes from gen-matrix (which already ran
nix-eval-jobs over the coverage attrset) via $COVERAGE_PATHS, so this
script does zero flake evaluation — each entry is a bare
`nix-store -r <outPath>` (pure substitution) which parallelises
trivially. Uploads run serially once everything is realised so
codecovcli output isn't interleaved and ::group:: blocks stay
coherent.

Runs on a no-KVM rio-ci runner; nix-store -r either substitutes from
S3 or fails fast (it has no .drv to build from). A missing lcov means
the coverage build matrix entry failed or was cancelled — log and
skip so one broken VM scenario doesn't blank the whole report.
"""
import json
import os
import subprocess
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

COVERAGE: dict[str, str] = json.loads(os.environ["COVERAGE_PATHS"])
# Baked by replaceVars (packages.coverage-upload) so the CLI version is
# pinned to nixpkgs alongside everything else.
CODECOVCLI = "@codecovcli@"

# Ambient nix-store (the one setup-nix installed). NOT pkgs.nix —
# a baked-in nix would have its own conf defaults and might not see
# the NIX_USER_CONF_FILES that niks3-action exported, so it'd miss
# rio-nix-cache as a substituter.
NIX_STORE = "nix-store"


def fetch_oidc_token() -> str:
    base = os.environ["ACTIONS_ID_TOKEN_REQUEST_URL"]
    req = urllib.request.Request(f"{base}&audience=https://codecov.io", headers={
        "Authorization":
            f"Bearer {os.environ['ACTIONS_ID_TOKEN_REQUEST_TOKEN']}",
    })
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)["value"]


def realise(name: str, out_path: str) -> tuple[str, str | None]:
    r = subprocess.run([NIX_STORE, "-r", out_path],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return name, None
    return name, out_path


def upload(name: str, path: str, token: str) -> None:
    print(f"::group::codecov upload: {name}")
    sys.stdout.flush()
    # upload-coverage, NOT upload-process: both reach do_upload_logic
    # but upload-coverage sets upload_coverage=True which selects the
    # /upload-coverage endpoint (new ingest pipeline) instead of the
    # legacy /commits/{sha}/reports/{code}/uploads. Our lcov paths are
    # `source/src/...` (per-crate sandbox prefix; crate name lost) and
    # only the new endpoint's path-fixing matches them against the
    # repo tree. The legacy endpoint returns "unusable report".
    # Matches codecov-action@v6's subcommand. slug/sha are passed
    # explicitly: the action's wrapper feeds them via CC_* env that
    # its bundled CLI reads, but click's required=True on --commit-sha
    # fires before the GHA CI-adapter fallback in plain codecovcli.
    subprocess.run(
        [CODECOVCLI, "upload-coverage",
         "--git-service", "github",
         "--slug", os.environ["GITHUB_REPOSITORY"],
         "--commit-sha", os.environ["GITHUB_SHA"],
         "--token", token,
         "--flag", name,
         "--file", path,
         "--disable-search",
         # Default plugins (xcode/gcov/pycoverage) probe for binaries
         # we don't ship and warn; we pass a finished lcov so none of
         # them would run on anything anyway.
         "--plugin", "noop",
         "--fail-on-error"],
        check=True,
    )
    print("::endgroup::")


def main() -> int:
    n = len(COVERAGE)
    token = fetch_oidc_token()
    print(f"realising {n} lcov outputs from cache (concurrency=8):")
    sys.stdout.flush()

    have: dict[str, str] = {}
    missing: list[str] = []
    with ThreadPoolExecutor(max_workers=8) as pool:
        futs = [pool.submit(realise, name, p) for name, p in COVERAGE.items()]
        for i, fut in enumerate(as_completed(futs), 1):
            name, path = fut.result()
            tag = "ok" if path else "MISSING"
            print(f"  [{i:2}/{n}] {name:40s} {tag}")
            sys.stdout.flush()
            if path is None:
                missing.append(name)
            else:
                have[name] = path

    for name, path in sorted(have.items()):
        upload(name, path, token)

    print(f"::notice::uploaded {len(have)}/{n} coverage flags")
    if missing:
        gone = " ".join(sorted(missing))
        print(f"::warning::not uploaded (missing from cache): {gone}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
