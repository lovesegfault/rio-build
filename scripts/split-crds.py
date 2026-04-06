#!/usr/bin/env python3
"""Split multi-doc crdgen YAML into one file per metadata.name.

Shared by `cargo xtask regen crds` and the crds-drift flake check.
Both callers diff-check byte-identity, so a single serialization
path (PyYAML `dump(sort_keys=False)`) is the only thing that makes
xtask output pass the drift gate.

usage: split-crds.py <multi-doc-yaml> <out-dir>
"""
import sys, yaml, pathlib

src, out = sys.argv[1], pathlib.Path(sys.argv[2])
with open(src) as f:
    for doc in yaml.safe_load_all(f):
        if doc is None:
            continue
        name = doc["metadata"]["name"]
        (out / f"{name}.yaml").write_text(yaml.dump(doc, sort_keys=False))
