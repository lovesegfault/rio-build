#!/usr/bin/env python3
"""Build a composefs dump file + FUSE manifest from a closure TSV.

TSV columns: <kind:F|D|L> <size> <store-relative-path>

Output:
  <dump>      — composefs-dump(5) text manifest for `mkcomposefs --from-file`
  <manifest>  — `<64-hex-digest> <size>` per regular file (FUSE input)
  <samples>   — JSON: {shallow, deep, large, *_size, n_files, total_size}
"""

import hashlib
import json
import sys

# composefs-dump(5): escape space, newline, CR, tab, backslash. '=' only in
# xattr fields (not used here). Dash is the empty-field sentinel.
ESC = {ord(c): f"\\x{ord(c):02x}" for c in " \n\r\t\\"}


def esc(s: str) -> str:
    return s.translate(ESC)


def digest_for(path: str, size: int) -> str:
    return hashlib.sha256(f"{path}|{size}".encode()).hexdigest()


def main() -> None:
    tsv, dump_out, manifest_out, samples_out = sys.argv[1:5]

    files: list[tuple[str, int]] = []
    dirs: set[str] = set()
    links: list[str] = []
    with open(tsv) as f:
        for line in f:
            kind, size_s, path = line.rstrip("\n").split("\t", 2)
            if kind == "D":
                dirs.add(path)
            elif kind == "L":
                links.append(path)
            elif kind == "F":
                files.append((path, int(size_s)))

    # composefs-dump(5) requires every parent dir to precede its children.
    for p in [p for p, _ in files] + links:
        parts = p.split("/")
        for i in range(1, len(parts)):
            dirs.add("/".join(parts[:i]))

    with open(dump_out, "w") as df, open(manifest_out, "w") as mf:
        df.write("/ 0 40555 2 0 0 0 0.0 - - -\n")
        for d in sorted(dirs):
            df.write(f"/{esc(d)} 0 40555 2 0 0 0 0.0 - - -\n")
        # Regular files: PAYLOAD = <2hex>/<digest> → mkcomposefs writes
        # trusted.overlay.redirect=/<payload> + metacopy automatically.
        # DIGEST='-' (no fs-verity for the spike).
        for p, sz in files:
            d = digest_for(p, sz)
            df.write(f"/{esc(p)} {sz} 100644 1 0 0 0 0.0 {d[:2]}/{d} - -\n")
            mf.write(f"{d} {sz}\n")
        for p in links:
            df.write(f"/{esc(p)} 0 120777 1 0 0 0 0.0 spike-dangling - -\n")

    by_depth = sorted(files, key=lambda x: x[0].count("/"))
    by_size = sorted(files, key=lambda x: x[1])
    shallow = next((e for e in by_depth if "/bin/" in e[0]), by_depth[0])
    deep = by_depth[-1]
    large = next(
        (e for e in reversed(by_size) if e[0].endswith(".so") or ".so." in e[0]),
        by_size[-1],
    )
    samples = {
        "shallow": shallow[0],
        "shallow_size": shallow[1],
        "deep": deep[0],
        "deep_size": deep[1],
        "large": large[0],
        "large_size": large[1],
        "n_files": len(files),
        "total_size": sum(s for _, s in files),
    }
    with open(samples_out, "w") as f:
        json.dump(samples, f)

    print(
        f"dump: {len(dirs)} dirs, {len(files)} files, {len(links)} symlinks; "
        f"total {samples['total_size']} bytes",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
