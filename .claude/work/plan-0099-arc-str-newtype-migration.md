# Plan 0099: Arc<str> newtype migration + scoped interning

## Design

`DrvHash` and `WorkerId` had been `String`-backed newtypes since phase 2a, cloned liberally across the actor, DAG, and assignment code. Profiling showed the scheduler spent measurable time in `memcpy` from `DrvHash::clone()` during DAG merge — each derivation hash is a 53-character string, and a 1000-node DAG merge cloned each hash a dozen times.

Five commits migrated both newtypes to `Arc<str>` backing via a new `arc_string_newtype!` macro in `rio-common`, then added scoped interning at the DAG boundary. The macro (`88336ca`) generates `struct X(Arc<str>)` with the same trait surface as the existing `string_newtype!`: `Deref<Target=str>`, `Borrow<str>`, `Display`, `From<String>`/`From<&str>`, `PartialEq<str>`/`&str`/`String`, `AsRef<str>`. Clone is an atomic refcount bump — no alloc, no memcpy. `Hash`/`Eq`/`Ord` are manually implemented on `str` **contents**, not `Arc` pointer identity: two `DrvHash::from("same")` built independently are `==` and hash-equal, so `HashMap<DrvHash, _>::get(&str)` via `Borrow<str>` continues to work. No `Borrow<String>` impl (`Arc<str>` contains no String to borrow) — only `Borrow<str>`, which is what every call site actually needed.

The newtypes moved from `rio-scheduler` to `rio-common` (`c7425dc`) since both worker and gateway need `WorkerId`. `807affb` added `StorePath::basename()` and migrated the ~20 `strip_prefix("/nix/store/")` sites to it — consistent basename extraction instead of ad-hoc string slicing.

`ef24a9d` added `Dag::canonical()`, a scoped interner: during `merge_dag`, all `DrvHash` instances pointing to the same string content get deduplicated to the same `Arc<str>` instance. A 1000-node DAG with 5000 edges goes from 6000 `Arc<str>` allocations to 1000. A `verify_invariant` debug assertion checks post-merge that `Arc::ptr_eq` holds for equal hashes. The scoping is per-DAG — no global interner, no cross-DAG leak risk.

## Files

```json files
[
  {"path": "rio-common/src/newtype.rs", "action": "MODIFY", "note": "arc_string_newtype! macro; DrvHash + WorkerId moved here from rio-scheduler"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "Dag::canonical() scoped interner; verify_invariant debug assertion"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "93-line interning test: Arc::ptr_eq holds post-merge for equal hashes"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "StorePath::basename(); ~20 strip_prefix sites migrated"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "DrvHash clone sites now refcount-bump not memcpy"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "WorkerId clone sites migrated"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "DrvHash imports from rio-common"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). No spec requirement covers newtype backing strategy — this is a performance optimization, not a behavior change.

## Entry

- Depends on P0097: the newtype migration touched `rio-scheduler/src/state/mod.rs` which was being split in P0102; doing the migration first meant the split commits could move already-migrated code.

## Exit

Merged as `807affb..ef24a9d` (5 commits). `.#ci` green at merge. All existing tests pass unchanged (the `Borrow<str>` impl ensures `HashMap<DrvHash, _>::get(&str)` continues to work). 93-line interning test added. `verify_invariant` is a debug assertion — zero release-build cost.
