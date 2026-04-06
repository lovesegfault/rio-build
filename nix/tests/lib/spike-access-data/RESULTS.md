# P0575 access-pattern probe — results

Host: ext4 /nix/store, page size 4096, kernel ftrace `mm_filemap_add_to_page_cache`.
Method: copy target to fresh inode on ext4 (avoid live mappings pinning page-cache),
`echo 3 > drop_caches`, run consumer, capture `(ofs, 4096<<order)` per folio add,
coalesce to ranges. Harness: `spike_access_probe.sh` + `spike_access_analyze.py`.

| target | size | consumer | bytes touched | % | ranges | pattern | mech |
|---|---|---|---|---|---|---|---|
| libLLVM.so.21.1 | 188 217 688 | `gcc hello.o $so -o a.out` (link-time) | 5 242 880 | **2.79%** | 4 | bimodal head+tail | mmap |
| libLLVM.so.21.1 | 188 217 688 | `LD_PRELOAD=$so true` (ld.so load only) | 42 450 944 | 22.55% | 177 | scattered | mmap |
| libLLVM.so.21.1 | 188 217 688 | `opt --version` (real tool startup) | 61 673 472 | **32.77%** | 266 | scattered | mmap |
| libLLVM.so.21.1 | 188 217 688 | `opt -O2 trivial.ll` (real compile) | 68 194 304 | 36.23% | 277 | scattered | mmap |
| libicudata.so.76.1 | 31 859 656 | `LD_PRELOAD=$so true` | 90 112 | **0.28%** | 2 | bimodal head+tail | mmap |
| libv8.a | 152 810 402 | `ld -r ref_head.o $a` (1 sym, head member) | 10 223 616 | 6.69% | 1 | single-contiguous front | read |
| libv8.a | 152 810 402 | `ld -r ref_tail.o $a` (1 sym, transitive) | 142 131 200 | **93.01%** | 14 | near-full | read |
| libv8.a | 152 810 402 | `ar t $a` (list members) | 22 249 472 | 14.56% | 922 | scattered (per-member hdr) | read |
| libclangTidyBugproneModule.a | 110 406 710 | `ar t $a` | 14 831 616 | 13.43% | 94 | scattered | read |

`ld.so` does **not** use `MAP_POPULATE` or `posix_fadvise(WILLNEED)` — strace shows
plain `MAP_PRIVATE|MAP_DENYWRITE` for all PT_LOAD segments. Lazy demand-fetch is
not defeated.

## Conclusion

- **Whole-file fetch wastes 64–99.7% for the dominant build patterns** (link-time
  `.so`: 97% wasted; tool-startup `.so`: 64–67% wasted; data `.so`: 99.7% wasted).
  Only deep-transitive `.a` links approach full utilization (7% wasted).
- **Streaming-open (P0575) saves the wasted fraction** — i.e. on a 188 MB libLLVM
  cold open, streaming saves ~120–183 MB depending on consumer.
- **Pattern split:**
  - Link-time + data `.so` → **bimodal** (ELF/sym header at front, section
    headers at tail). A `head(N) + tail(M)` prefetch heuristic would cover these
    almost perfectly.
  - Runtime tool startup → **scattered** (relocation processing + global ctors
    fault pages all over `.text`/`.data.rel.ro`). 177–277 disjoint ranges,
    max gap ~22 MB. Sequential readahead helps *within* each range but cannot
    predict the next range. Pure demand-paging is the only correct strategy.
  - `.a` archives → bimodal (index at front + selected members) when few members
    pulled; near-full when transitive closure is large.
- **Readahead:** kernel default 128 KB readahead is adequate within ranges (the
  scattered runs show ranges typically span dozens of contiguous folios). It
  **does not** help across multi-MB gaps. P0575 should not attempt predictive
  prefetch beyond what the kernel already does — serve `read(off,len)` on demand
  and let kernel readahead extend the request.
