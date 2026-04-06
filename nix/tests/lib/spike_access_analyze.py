#!/usr/bin/env python3
"""Analyze page-cache-fill TSV (ofs<TAB>len) against a file size.

Reports: bytes touched, % of file, range count after coalescing,
front/tail/mid distribution, max gap, pattern verdict.
"""
import sys

def main() -> None:
    tsv, size = sys.argv[1], int(sys.argv[2])
    evs = []
    with open(tsv) as f:
        for line in f:
            o, l = line.split()
            evs.append((int(o), int(l)))
    if not evs:
        print("no events")
        return
    # coalesce into ranges
    evs.sort()
    ranges = []
    cur_s, cur_e = evs[0][0], evs[0][0] + evs[0][1]
    for o, l in evs[1:]:
        if o <= cur_e:
            cur_e = max(cur_e, o + l)
        else:
            ranges.append((cur_s, cur_e))
            cur_s, cur_e = o, o + l
    ranges.append((cur_s, cur_e))
    touched = sum(e - s for s, e in ranges)
    # distribution: front = first 5%, tail = last 5%
    f5, t5 = size * 5 // 100, size * 95 // 100
    front = sum(min(e, f5) - s for s, e in ranges if s < f5)
    tail = sum(e - max(s, t5) for s, e in ranges if e > t5)
    mid = touched - front - tail
    gaps = [ranges[i + 1][0] - ranges[i][1] for i in range(len(ranges) - 1)]
    maxgap = max(gaps) if gaps else 0
    # pattern verdict
    if len(ranges) == 1:
        pat = "single-contiguous"
    elif front + tail > touched * 0.8:
        pat = "bimodal(head+tail)"
    elif len(ranges) <= 4 and maxgap > size * 0.2:
        pat = "few-islands"
    elif maxgap < 1 << 20:
        pat = "near-sequential"
    else:
        pat = "scattered"
    print(f"file_size={size}")
    print(f"touched_bytes={touched}  pct={touched / size * 100:.2f}%")
    print(f"ranges={len(ranges)}  max_gap={maxgap}  events={len(evs)}")
    print(f"front5%={front}  mid={mid}  tail5%={tail}")
    print(f"first_range=[{ranges[0][0]},{ranges[0][1]})  last_range=[{ranges[-1][0]},{ranges[-1][1]})")
    print(f"pattern={pat}")

if __name__ == "__main__":
    main()
