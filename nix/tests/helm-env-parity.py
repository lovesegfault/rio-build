#!/usr/bin/env python3
"""helm-env-schema-parity (merged_bug_161 class close).

For each chart Deployment with a committed config-schema fixture:
derive the env-name universe from the fixture's `defaults` object
(RIO_<UPPER>; nested sections RIO_<SECTION>__<FIELD> — the same
convention rio_common::config's env layering uses), collect the env
NAMES the rendered Deployment carries, and fail when a schema field is
neither rendered nor allowlisted. The allowlist
(nix/tests/helm/env-parity-allowlist.json) is the documented-rationale
home for deliberately unwired knobs — every entry carries its why.

Usage: helm-env-parity.py <render.yaml> <allowlist.json> \
         <component>=<fixture.json> ...
"""

import json
import re
import sys


def schema_envs(fixture_path: str) -> set[str]:
    defaults = json.load(open(fixture_path))["defaults"]
    envs: set[str] = set()
    for key, val in defaults.items():
        if isinstance(val, dict):
            for sub in val:
                envs.add(f"RIO_{key.upper()}__{sub.upper()}")
        else:
            envs.add(f"RIO_{key.upper()}")
    return envs


def rendered_envs(render: str) -> dict[str, set[str]]:
    out: dict[str, set[str]] = {}
    for blob in render.split("\n---"):
        if "kind: Deployment" not in blob:
            continue
        m = re.search(r"^  name: (rio-[a-z-]+)$", blob, re.M)
        if not m:
            continue
        out[m.group(1)] = set(re.findall(r"- name: (RIO_[A-Z0-9_]+)", blob))
    return out


def main() -> int:
    render_path, allowlist_path, *pairs = sys.argv[1:]
    render = open(render_path).read()
    allowlist = json.load(open(allowlist_path))
    per_component = rendered_envs(render)
    failures: list[str] = []
    for pair in pairs:
        component, fixture = pair.split("=", 1)
        rendered = per_component.get(component)
        if rendered is None:
            failures.append(
                f"{component}: no Deployment in the render — premise broken, "
                "parity assertions vacuous"
            )
            continue
        allowed = set(allowlist.get(component, {}))
        stale_allow = sorted(allowed & rendered)
        if stale_allow:
            failures.append(
                f"{component}: allowlisted envs are now RENDERED — delete the "
                f"stale allowlist entries: {', '.join(stale_allow)}"
            )
        missing = sorted(schema_envs(fixture) - rendered - allowed)
        if missing:
            failures.append(
                f"{component}: schema fields neither rendered nor allowlisted "
                f"(wire them in the chart or add an allowlist entry WITH a "
                f"why): {', '.join(missing)}"
            )
    if failures:
        print("FAIL: helm-env-schema-parity", file=sys.stderr)
        for f in failures:
            print(f"  {f}", file=sys.stderr)
        return 1
    print(f"OK: helm-env-schema-parity ({len(pairs)} components)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
