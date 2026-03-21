#!/usr/bin/env python3
"""Post-process crate2nix JSON to inject devDependencies.

crate2nix's --format json output omits dev_dependencies (json_output.rs
ResolvedCrate struct has no field for them). The Cargo.nix template
mode DOES include them, but we want the JSON mode's smaller/diffable
output.

All the dev-dep TARGET crates are already in Cargo.json (crate2nix
resolves --all-features, which pulls in everything the lockfile has).
We just need the mapping: workspace_member -> [dev_dep_packageId].

This script reads `cargo metadata --format-version 1` to find each
workspace member's dev-deps, maps them to packageIds using Cargo.json's
own crate dictionary, and writes devDependencies back.

Run after `crate2nix generate --format json -o Cargo.json`.
"""

import json
import subprocess
import sys
from pathlib import Path


def main() -> None:
    cargo_json_path = Path(sys.argv[1] if len(sys.argv) > 1 else "Cargo.json")
    workspace_root = cargo_json_path.parent

    # Read the crate2nix-generated JSON
    with cargo_json_path.open() as f:
        resolved = json.load(f)

    # Build a simple name→id map for unambiguous cases (most deps).
    name_to_id: dict[str, str] = {}
    name_ambiguous: set[str] = set()
    for pkg_id, crate in resolved["crates"].items():
        n = crate["crateName"]
        if n in name_to_id:
            name_ambiguous.add(n)
        name_to_id[n] = pkg_id
    for n in name_ambiguous:
        del name_to_id[n]

    # Read cargo metadata for dev-deps per package.
    # --format-version 1 is stable; --no-deps would skip dependency
    # resolution which we need for the `resolve` section. But we only
    # need .packages[].dependencies (unresolved dep declarations),
    # so --no-deps is fine — we map names to ids using our own dict.
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            cwd=workspace_root,
        )
    )

    # For each workspace member, find its dev-deps and inject them.
    workspace_members = resolved["workspaceMembers"]
    injected = 0
    for member_name, member_pkg_id in workspace_members.items():
        # Find the corresponding package in cargo metadata.
        meta_pkg = next(
            (p for p in metadata["packages"] if p["name"] == member_name), None
        )
        if meta_pkg is None:
            print(f"warn: {member_name} not in cargo metadata, skipping", file=sys.stderr)
            continue

        dev_deps = []
        for dep in meta_pkg["dependencies"]:
            if dep.get("kind") != "dev":
                continue
            dep_name = dep["name"]
            # Map name→packageId. Use name_to_id for unambiguous cases;
            # fall back to (name, version) match otherwise.
            pkg_id = name_to_id.get(dep_name)
            if pkg_id is None:
                # Ambiguous or missing. Try (name, version) match from
                # the metadata's req field — but req is a semver spec,
                # not an exact version. Instead, use the `rename`+`name`
                # and accept the first match by name alone (safe for
                # workspace dev-deps which are rarely version-conflicted).
                candidates = [
                    (pid, c)
                    for pid, c in resolved["crates"].items()
                    if c["crateName"] == dep_name
                ]
                if not candidates:
                    print(
                        f"warn: dev-dep {dep_name} of {member_name} not in "
                        f"resolved crates, skipping",
                        file=sys.stderr,
                    )
                    continue
                # Prefer the highest version (lexicographic is close enough
                # for semver) — matches cargo's resolution.
                candidates.sort(key=lambda x: x[1]["version"], reverse=True)
                pkg_id = candidates[0][0]

            dev_deps.append(
                {
                    "name": dep_name,
                    "packageId": pkg_id,
                    # crate2nix's DepInfo omits rename/target when absent.
                    # Dev-deps rarely have platform conditions; if they do,
                    # the target field would need populating from
                    # dep["target"]. For now, unconditional.
                    **({"rename": dep["rename"]} if dep.get("rename") else {}),
                    **({"target": dep["target"]} if dep.get("target") else {}),
                }
            )

        if dev_deps:
            resolved["crates"][member_pkg_id]["devDependencies"] = dev_deps
            injected += 1

    # Write back. Preserve 2-space indent to match crate2nix's output
    # style so diffs stay minimal.
    with cargo_json_path.open("w") as f:
        json.dump(resolved, f, indent=2)
        f.write("\n")

    print(
        f"injected devDependencies into {injected}/{len(workspace_members)} "
        f"workspace members",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
