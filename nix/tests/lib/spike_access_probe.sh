#!/usr/bin/env bash
# spike_access_probe.sh — capture page-cache fills for one file under one consumer.
# Output: TSV of (ofs, len_bytes) for every folio added to page cache for that inode.
# Usage: sudo ./spike_access_probe.sh <target-file> <out.tsv> -- <consumer-cmd...>
set -euo pipefail

TARGET=$1; OUT=$2; shift 2
[[ "$1" == "--" ]] && shift

INO_DEC=$(stat -c '%i' "$TARGET")
INO_HEX=$(printf '%x' "$INO_DEC")
SZ=$(stat -c '%s' "$TARGET")
T=/sys/kernel/debug/tracing

echo 0 > "$T/tracing_on"
echo > "$T/trace"
echo "i_ino == $INO_DEC" > "$T/events/filemap/mm_filemap_add_to_page_cache/filter"
echo 1 > "$T/events/filemap/mm_filemap_add_to_page_cache/enable"
echo 3 > /proc/sys/vm/drop_caches
echo 1 > "$T/tracing_on"

# Run consumer; ignore its exit (linkers may fail on missing symbols — we only care about reads)
"$@" >/dev/null 2>&1 || true

echo 0 > "$T/tracing_on"
echo 0 > "$T/events/filemap/mm_filemap_add_to_page_cache/enable"
echo 0 > "$T/events/filemap/mm_filemap_add_to_page_cache/filter"

# Parse: lines look like: ... ino 329d594 pfn=0x... ofs=123456 order=2
# Filter to our ino (filter should already do it, but belt+braces); emit ofs<TAB>len
grep -F " ino $INO_HEX " "$T/trace" \
  | sed -E 's/.* ofs=([0-9]+) order=([0-9]+).*/\1\t\2/' \
  | awk -v PG=4096 '{print $1"\t"(PG * 2^$2)}' \
  > "$OUT"
echo > "$T/trace"

echo "# target=$TARGET size=$SZ ino=$INO_HEX events=$(wc -l < "$OUT")" >&2
