#!/usr/bin/env bash
#
# Regenerate the NAR corpus seeds. Text-format seeds (ATerm, narinfo,
# derived-paths, wire bytes) are hand-maintained directly in corpus/
# since they're trivial to inspect/edit. NAR seeds are the exception:
# the binary format is opaque, so we generate them from nix-store --dump.
#
# Requires nix in PATH (dev shell has it).

set -euo pipefail
cd "$(dirname "$0")"

mkdir -p corpus/nar_parsing

nar_tmp=$(mktemp -d)
trap 'rm -rf "$nar_tmp"' EXIT

mkdir -p "$nar_tmp/empty"
nix-store --dump "$nar_tmp/empty" > corpus/nar_parsing/empty-dir.nar

echo "hello world" > "$nar_tmp/hello.txt"
nix-store --dump "$nar_tmp/hello.txt" > corpus/nar_parsing/single-file.nar

mkdir -p "$nar_tmp/nested/a/b"
echo "deep" > "$nar_tmp/nested/a/b/c.txt"
ln -s "a/b/c.txt" "$nar_tmp/nested/link"
nix-store --dump "$nar_tmp/nested" > corpus/nar_parsing/nested-with-symlink.nar

echo "Regenerated $(ls corpus/nar_parsing | wc -l) NAR seeds."
