# Plan 973647701: CONSOLIDATION — READ_ONLY_ROOT_MOUNTS const table

Consolidator finding (mc-cadence): the `read_only_root_fs` emptyDir mounts in [`common/sts.rs:404+`](../../rio-controller/src/reconcilers/common/sts.rs) are whack-a-mole — 5 write paths found so far, each adds a `Volume` + `VolumeMount` pair in two separate `if p.read_only_root_fs` blocks. Each new rootfs write in `rio-builder` means hunting both blocks and adding two struct literals that differ only by name. ~40-line extraction to a const table iterated at both sites.

[P0466](plan-0466-fetcherpool-readonly-root-emptydir.md) adds more mounts to this block (CI-blocking gap fill). This plan refactors the now-complete set AFTER P0466 lands.

## Entry criteria

- [P0466](plan-0466-fetcherpool-readonly-root-emptydir.md) merged (emptyDir set is complete; refactor target is stable)

## Tasks

### T1 — `refactor(controller):` extract READ_ONLY_ROOT_MOUNTS const table

MODIFY [`rio-controller/src/reconcilers/common/sts.rs`](../../rio-controller/src/reconcilers/common/sts.rs).

Current shape (post-P0466, approximate):

```rust
if p.read_only_root_fs {
    v.push(Volume { name: "tmp".into(), empty_dir: Some(EmptyDirVolumeSource { medium: Some("Memory".into()), size_limit: Some(Quantity("64Mi".into())) }), ..Default::default() });
    v.push(Volume { name: "fuse-store".into(), empty_dir: Some(EmptyDirVolumeSource::default()), ..Default::default() });
    v.push(Volume { name: "nix-var".into(), /* ... */ });
    // ... N more
}
// ... 80 lines later, in the volume_mounts block:
if p.read_only_root_fs {
    m.push(VolumeMount { name: "tmp".into(), mount_path: "/tmp".into(), ..Default::default() });
    m.push(VolumeMount { name: "fuse-store".into(), mount_path: "/nix/store".into(), ..Default::default() });
    // ... N more, MUST match the Volume block by name
}
```

Extract to:

```rust
/// emptyDir mounts that make readOnlyRootFilesystem:true workable for
/// rio-builder. Each entry here corresponds to a startup write path in
/// rio-builder/src/main.rs (see the audit comment there). Adding a
/// startup write? Add a row here.
const READ_ONLY_ROOT_MOUNTS: &[(&str, &str, Option<&str>, Option<&str>)] = &[
    // (name, mount_path, medium, size_limit)
    ("tmp", "/tmp", Some("Memory"), Some("64Mi")),
    ("fuse-store", "/nix/store", None, None),
    ("nix-var", "/nix/var", Some("Memory"), Some("128Mi")),
    // ... complete set from P0466
];
```

Then both `if p.read_only_root_fs` blocks iterate the table:

```rust
if p.read_only_root_fs {
    for (name, _, medium, size_limit) in READ_ONLY_ROOT_MOUNTS {
        v.push(Volume {
            name: (*name).into(),
            empty_dir: Some(EmptyDirVolumeSource {
                medium: medium.map(Into::into),
                size_limit: size_limit.map(|s| Quantity(s.into())),
            }),
            ..Default::default()
        });
    }
}
```

Tuple-of-4 is fine; a named struct is overkill for a 5-8 row const table. If the mount specs diverge further (subPath, readOnly flags), promote to a struct then.

## Exit criteria

- `/nixbuild .#ci` → green (vm-fetcher-split stays green — refactor is behavior-preserving)
- `grep -c 'READ_ONLY_ROOT_MOUNTS' rio-controller/src/reconcilers/common/sts.rs` → ≥3 (const decl + 2 iteration sites)
- `grep -c 'v.push(Volume.*empty_dir' rio-controller/src/reconcilers/common/sts.rs` inside the `read_only_root_fs` blocks → 0 (all table-driven)
- `diff <(kubectl get sts -n rio-fetchers -o yaml) /tmp/pre-refactor-sts.yaml` → only cosmetic/ordering diffs, no semantic change to rendered StatefulSet

## Tracey

No markers — pure refactor, no behavior change. The `r[fetcher.sandbox.strict-seccomp]` and `r[ctrl.fetcherpool.reconcile]` impl annotations stay where they are (on the reconciler, not the const table).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/common/sts.rs", "action": "MODIFY", "note": "T1: extract READ_ONLY_ROOT_MOUNTS const table; iterate at Volume+VolumeMount blocks"}
]
```

```
rio-controller/src/reconcilers/common/
└── sts.rs                     # T1: const table extraction
```

## Dependencies

```json deps
{"deps": [466], "soft_deps": [], "note": "Consolidator finding — whack-a-mole Volume+VolumeMount pairs. Hard-dep P0466: the emptyDir set must be COMPLETE before refactoring it (P0466 adds mounts to unblock vm-fetcher-split CI). Refactoring an incomplete set means P0466's additions land in the OLD shape, then this refactor has to redo them. Sequence: P0466 → this. discovered_from=consolidator."}
```

**Depends on:** [P0466](plan-0466-fetcherpool-readonly-root-emptydir.md) — emptyDir set complete.
**Conflicts with:** [`common/sts.rs`](../../rio-controller/src/reconcilers/common/sts.rs) — P0466 T2 adds mounts to `:404+`; this plan refactors that same block. Serialize strictly: P0466 → P973647701.
