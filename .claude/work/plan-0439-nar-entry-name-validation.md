# Plan 439: NAR entry-name validation — path traversal guard

**SECURITY — ship first.** `parse_directory` at
[`rio-nix/src/nar.rs:282-334`](../../rio-nix/src/nar.rs) accepts directory
entry names with no content validation beyond UTF-8 + sorted-order. A crafted
NAR with entry name `..`, `/etc/passwd`, or a name containing `/` or NUL
passes parsing; `extract_to_path` at
[`nar.rs:616`](../../rio-nix/src/nar.rs) then does
`path.join(&entry.name)` — `Path::join` with an absolute component or `..`
traverses outside the extraction root.

Reachable from the worker FUSE fetch path:
[`rio-worker/src/fuse/fetch.rs:378`](../../rio-worker/src/fuse/fetch.rs)
calls `extract_to_path` on store-fetched NARs into a tmp sibling of the FUSE
cache dir. A malicious store (or a store compromised via another vector)
serving a crafted NAR could write arbitrary files on worker nodes.

The Nix C++ reference implementation
([`archive.cc`](https://github.com/NixOS/nix/blob/master/src/libutil/archive.cc))
rejects empty names, `.`, `..`, and names containing `/` or NUL immediately
after the name read. We replicate that exactly.

Origin: bughunter sweep, report `bug_032` at
`/tmp/bughunter/prompt-6701aef0_2/reports/`.

## Tasks

### T1 — `fix(nix):` reject dangerous NAR entry names at parse time

Add `NarError::InvalidEntryName { name: String }` variant to the error enum
at [`nar.rs:49`](../../rio-nix/src/nar.rs). After the UTF-8 conversion at
[`nar.rs:307`](../../rio-nix/src/nar.rs) and before the sorted-order check,
reject:

```rust
// r[impl worker.nar.entry-name-safety]
if name.is_empty()
    || name == "."
    || name == ".."
    || name.contains('/')
    || name.contains('\0')
{
    return Err(NarError::InvalidEntryName { name });
}
```

Matches Nix C++ `archive.cc` `parseDump` name-validation. The check is
ordered before `prev_name = Some(name.clone())` so a rejected name doesn't
pollute the sort-order state.

### T2 — `test(nix):` crafted-NAR unit tests + extract_to_path round-trip

Add unit tests in `rio-nix/src/nar.rs` `#[cfg(test)]`:

- `test_parse_rejects_dotdot_entry` — hand-crafted NAR bytes with entry
  name `..`, expect `Err(NarError::InvalidEntryName { .. })`
- `test_parse_rejects_slash_entry` — entry name `etc/passwd`
- `test_parse_rejects_absolute_entry` — entry name `/etc/passwd`
- `test_parse_rejects_nul_entry` — entry name `foo\0bar`
- `test_parse_rejects_empty_entry` — entry name `""`
- `test_parse_rejects_dot_entry` — entry name `.`
- `test_extract_safe_names_round_trip` — build a directory NAR with
  safe names (`foo`, `bar-baz`, `a.b.c`), `extract_to_path` into a
  `tempfile::tempdir()`, verify the tree matches

Extend the existing `nar_parsing` fuzz target at
[`rio-nix/fuzz/fuzz_targets/nar_parsing.rs`](../../rio-nix/fuzz/fuzz_targets/nar_parsing.rs)
to round-trip `extract_to_path` into a tempdir when parse succeeds — any
crash or write outside the tempdir is a fuzz finding. Add seed inputs with
dangerous names to `rio-nix/fuzz/corpus/nar_parsing/seed-dotdot`,
`seed-slash`, `seed-nul`.

### T3 — `docs(worker):` add r[worker.nar.entry-name-safety] marker

Add the spec marker to
[`docs/src/components/worker.md`](../../docs/src/components/worker.md)
under the FUSE Cache section (after `r[worker.fuse.cache-lru]`). See
`## Spec additions` below. Annotate the T1 validation block with
`// r[impl worker.nar.entry-name-safety]` and the T2 rejection tests with
`// r[verify worker.nar.entry-name-safety]`.

## Exit criteria

- `/nixbuild .#ci` green
- `cargo nextest run -p rio-nix nar` shows 6 new rejection tests passing
- `tracey query rule worker.nar.entry-name-safety` shows impl + verify sites
- `grep -c 'InvalidEntryName' rio-nix/src/nar.rs` ≥ 2 (variant + return site)
- Fuzz corpus has `seed-dotdot`, `seed-slash`, `seed-nul` entries

## Tracey

Adds new marker to component specs:
- `r[worker.nar.entry-name-safety]` → `docs/src/components/worker.md` (see ## Spec additions below) — T1 implements, T2 verifies

## Spec additions

Add to `docs/src/components/worker.md` after line 62 (after the
`r[worker.fuse.cache-lru]` block, before `### FUSE Implementation`):

```markdown
r[worker.nar.entry-name-safety]
NAR directory entry names MUST be rejected at parse time if empty,
equal to `.` or `..`, or containing `/` or NUL. This matches the Nix
C++ reference (`archive.cc` `parseDump`). The rejection happens in
`rio_nix::nar::parse_directory` before any filesystem call —
`extract_to_path` never sees a dangerous name. Rationale:
`Path::join("..")` traverses upward; `Path::join("/abs")` discards the
base. A crafted NAR from a compromised store could otherwise write
arbitrary files on worker nodes via the FUSE fetch path.
```

## Files

```json files
[
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "T1: InvalidEntryName variant + rejection check; T2: 7 unit tests"},
  {"path": "rio-nix/fuzz/fuzz_targets/nar_parsing.rs", "action": "MODIFY", "note": "T2: extract_to_path round-trip on parse success"},
  {"path": "rio-nix/fuzz/corpus/nar_parsing/seed-dotdot", "action": "NEW", "note": "T2: fuzz seed with .. entry name"},
  {"path": "rio-nix/fuzz/corpus/nar_parsing/seed-slash", "action": "NEW", "note": "T2: fuzz seed with slash entry name"},
  {"path": "rio-nix/fuzz/corpus/nar_parsing/seed-nul", "action": "NEW", "note": "T2: fuzz seed with NUL entry name"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "T3: r[worker.nar.entry-name-safety] marker"}
]
```

```
rio-nix/
├── src/
│   └── nar.rs                        # T1+T2: variant, check, 7 tests
└── fuzz/
    ├── fuzz_targets/
    │   └── nar_parsing.rs            # T2: extract_to_path round-trip
    └── corpus/nar_parsing/
        ├── seed-dotdot               # T2 (NEW)
        ├── seed-slash                # T2 (NEW)
        └── seed-nul                  # T2 (NEW)
docs/src/components/
└── worker.md                         # T3: new marker
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "standalone security fix — no deps, ship first"}
```

**Depends on:** none. The NAR parser is leaf code; `extract_to_path` has no
upstream callers that would be affected by stricter validation (legitimate
NARs never contain these names — Nix C++ rejects them on write too).

**Conflicts with:** `rio-nix/src/nar.rs` and
`docs/src/components/worker.md` are not in the collisions top-30. No
serialization needed.
