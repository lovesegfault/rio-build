# S7 generated sweeps (bughunt-5)

Committed [GEN-SET] output per the round-4 banner (retained under the
round-5 banner): "every surface" is a generated set, never a memory.
Regenerate each block with its command; a drifted block means a new
surface appeared and must be classified.

## gc failure-frame prefix-literal census (merged_bug_052)

The contract: the gc phase-3 failure-frame prefixes are single-sourced
at `rio_common::classify` (`GC_CHUNK_COLLECT_SUSPENDED_PREFIX` /
`GC_CHUNK_COLLECT_FAILED_PREFIX`, colon included). The store renders
THROUGH the constants (`render_phase3`), the CLI exit posture matches
THROUGH the shared predicate (`gc_render_is_chunk_collect_failure`),
and both suites assert through the same symbols. A prefix literal
anywhere else is a contract fork.

Command:

    rg -n 'chunk collect SUSPENDED|chunk collect FAILED' --no-heading | sort

Pre-close output (2026-06-09, at wave base 65ea57afa — the 12-site
census of merged_bug_052 plus the two weak colon-free asserts):

| site | classification |
|---|---|
| `rio-cli/src/gc.rs:110` | matcher literal (SUSPENDED) — hand-mirrored copy 1 |
| `rio-cli/src/gc.rs:111` | matcher literal (FAILED) — hand-mirrored copy 2 |
| `rio-cli/src/gc.rs:253` | CLI test fixture literal (copy 3) |
| `rio-cli/src/gc.rs:254` | CLI test fixture literal (copy 4) |
| `rio-cli/src/gc.rs:265` | CLI test assertion literal (copy 5) |
| `rio-cli/src/gc.rs:266` | CLI test assertion literal (copy 6) |
| `rio-cli/src/gc.rs:272` | CLI test assertion literal (copy 7) |
| `rio-cli/src/gc.rs:273` | CLI test assertion literal (copy 8) |
| `rio-store/src/gc/mod.rs:499` | producer-side comment restating the contract (SUSPENDED) |
| `rio-store/src/gc/mod.rs:500` | producer-side comment restating the contract (FAILED) |
| `rio-store/src/gc/mod.rs:536` | producer `format!` literal (SUSPENDED) |
| `rio-store/src/gc/mod.rs:547` | producer `format!` literal (FAILED) |
| `rio-store/src/gc/mod.rs:842` | weak store assert — colon-FREE prefix (the reword hole) |
| `rio-store/src/gc/mod.rs:892` | weak store assert — colon-FREE prefix (the reword hole) |

Post-close output (2026-06-09; the contract's executable home plus its
byte-exact pin — the ONLY sanctioned literal sites):

| site | classification |
|---|---|
| `rio-common/src/classify.rs:197` | `GC_CHUNK_COLLECT_SUSPENDED_PREFIX` const — THE source |
| `rio-common/src/classify.rs:199` | `GC_CHUNK_COLLECT_FAILED_PREFIX` const — THE source |
| `rio-common/src/classify.rs:275` | `gc_failure_prefix_alphabet_pinned` byte-exact pin (SUSPENDED) |
| `rio-common/src/classify.rs:276` | `gc_failure_prefix_alphabet_pinned` byte-exact pin (FAILED) |

(This file is excluded from the census command's hit set by
construction: the command is run from the repo root and this file
documents it; a regeneration that finds hits beyond classify.rs and
this sweeps file is a contract fork and must be re-classified.)
