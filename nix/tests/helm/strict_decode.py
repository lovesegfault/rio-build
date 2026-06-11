#!/usr/bin/env python3
"""Strict-decode tier for rendered helm manifests (merged_bug_004 close).

A value query on a duplicate-tolerant parser is an INCOMPLETE VIEW of
the document (R26 structural form): yq's traversal resolves a
duplicated mapping key to whichever block its walk order favors, so a
template that renders the same key twice (the store nodeSelector
shape: a close added its block below the stale block it obsoleted)
stays green under every value assert while strict consumers
(kubectl strict validation, ArgoCD, kubeconform) reject the manifest
and first-wins parsers silently keep the OBSOLETE value.

This tool is the missing validity tier. Two modes:

  strict <file>...
      Parse every YAML document in every file with a loader that
      REJECTS duplicate mapping keys (the yaml.v3-strict equivalent).
      Any duplicate or parse error is a structural red regardless of
      which value would have won.

  keys <file>...
      Emit the rendered key population: one line per
      `<kind>/<name>\t<key path>` (list indices collapsed to `[]`,
      paths sorted unique). The committed baseline
      (rendered-key-population.txt) makes adjacency drift — a key
      appearing in or vanishing from a rendered document — a
      reviewable diff instead of a silent render change.
      Regenerate via: nix/tests/helm/regen-key-population.sh

The helm-lint driver (nix/misc-checks.nix) wraps `helm` so EVERY
fragment's successful `helm template` output passes through `strict`;
the key-population baseline is diffed against the canonical profile
pair there. Exclusions: none — every rendered document decodes
strictly or the gate is red.
"""

import sys

import yaml


class DuplicateKeyError(Exception):
    pass


class StrictLoader(yaml.SafeLoader):
    """SafeLoader that rejects duplicate keys in any mapping."""


def _strict_construct_mapping(loader, node, deep=False):
    seen = set()
    for key_node, _ in node.value:
        key = loader.construct_object(key_node, deep=True)
        try:
            hashable = key if not isinstance(key, (list, dict)) else repr(key)
        except TypeError:
            hashable = repr(key)
        if hashable in seen:
            raise DuplicateKeyError(
                f"duplicate mapping key {key!r} at line {key_node.start_mark.line + 1}"
            )
        seen.add(hashable)
    return yaml.SafeLoader.construct_mapping(loader, node, deep)


StrictLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, _strict_construct_mapping
)


def iter_documents(path):
    with open(path, encoding="utf-8") as f:
        yield from yaml.load_all(f, Loader=StrictLoader)


def cmd_strict(paths):
    failed = False
    for path in paths:
        try:
            for _ in iter_documents(path):
                pass
        except (DuplicateKeyError, yaml.YAMLError) as e:
            print(f"STRICT-DECODE FAIL: {path}: {e}", file=sys.stderr)
            failed = True
    if failed:
        print(
            "strict-decode: a rendered manifest fails strict YAML decode — "
            "duplicate keys defeat value-query witnesses (merged_bug_004); "
            "fix the template, never the assert",
            file=sys.stderr,
        )
        return 1
    return 0


def _key_paths(node, prefix, out):
    if isinstance(node, dict):
        for key, val in node.items():
            path = f"{prefix}.{key}" if prefix else str(key)
            out.add(path)
            _key_paths(val, path, out)
    elif isinstance(node, list):
        for item in node:
            _key_paths(item, f"{prefix}[]", out)


def cmd_keys(paths):
    lines = set()
    for path in paths:
        try:
            docs = list(iter_documents(path))
        except (DuplicateKeyError, yaml.YAMLError) as e:
            print(f"STRICT-DECODE FAIL: {path}: {e}", file=sys.stderr)
            return 1
        for doc in docs:
            if doc is None:
                continue
            kind = doc.get("kind", "?") if isinstance(doc, dict) else "?"
            name = "?"
            if isinstance(doc, dict):
                meta = doc.get("metadata") or {}
                if isinstance(meta, dict):
                    name = meta.get("name", "?")
            paths_out = set()
            _key_paths(doc, "", paths_out)
            for key_path in paths_out:
                lines.add(f"{kind}/{name}\t{key_path}")
    sys.stdout.write("\n".join(sorted(lines)) + "\n")
    return 0


def main():
    if len(sys.argv) < 3 or sys.argv[1] not in ("strict", "keys"):
        print(__doc__, file=sys.stderr)
        return 2
    mode, paths = sys.argv[1], sys.argv[2:]
    return cmd_strict(paths) if mode == "strict" else cmd_keys(paths)


if __name__ == "__main__":
    sys.exit(main())
