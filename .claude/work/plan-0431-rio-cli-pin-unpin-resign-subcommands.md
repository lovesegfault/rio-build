# Plan 0431: rio-cli pin/unpin/resign subcommands — wire StoreAdminService clients

sprint-1 cleanup finding. `PinPath`/`UnpinPath`/`ResignPaths` at [`rio-store/src/grpc/admin.rs:376`](../../rio-store/src/grpc/admin.rs) are server-implemented and tested but have zero callers — only `TriggerGC` is wired into rio-cli today. Promoted from `TODO(P0431)` at `admin.rs:376`.

Same shape as [P0216](plan-0216-rio-cli-subcommands.md) (the original cli subcommand batch) — three more StoreAdminService RPCs need cli verbs.

## Entry criteria

- [P0216](plan-0216-rio-cli-subcommands.md) merged (rio-cli clap structure + StoreAdminServiceClient plumbing exist)
- `PinPath`/`UnpinPath`/`ResignPaths` server handlers exist at [`admin.rs`](../../rio-store/src/grpc/admin.rs) (they do — `:380`+)

## Tasks

### T1 — `feat(cli):` rio-cli store pin <path>

MODIFY [`rio-cli/src/main.rs`](../../rio-cli/src/main.rs). Add `Pin { path: String }` variant to the store subcommand enum, and a handler that calls `StoreAdminServiceClient::pin_path`. Mirror the existing `TriggerGC` wiring pattern.

### T2 — `feat(cli):` rio-cli store unpin <path>

Same pattern as T1 — `Unpin { path: String }` → `unpin_path`.

### T3 — `feat(cli):` rio-cli store resign [--tenant <uuid>] <path>...

`Resign { tenant: Option<Uuid>, paths: Vec<String> }` → `resign_paths`. Check the proto for whether `ResignPathsRequest` takes a tenant filter; if so, plumb `--tenant` flag.

### T4 — `test(cli):` VM subtest — pin/unpin/resign smoke

MODIFY [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix). Add a subtest that pins a path, runs GC, asserts the path survived; unpins, runs GC again, asserts it's collected. Resign: re-sign a path with a fresh key, assert the new signature appears in narinfo.

### T5 — `chore(store):` remove TODO(P0431) at admin.rs:376

Once T1-T3 land, the RPCs have callers — delete the TODO comment.

## Exit criteria

- `/nixbuild .#ci` green
- `rio-cli store pin --help` / `unpin --help` / `resign --help` all render
- `grep 'TODO(P0431)' rio-store/` → 0 hits
- VM subtest proves pin survives GC, unpin allows collection

## Tracey

References existing `r[store.admin.pin]` / `r[store.admin.resign]` if present in [`store.md`](../../docs/src/components/store.md). T4 adds `# r[verify store.admin.pin]` at the `cli.nix` subtests entry per VM-test marker placement convention.

## Files

```json files
[
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T1-T3: +Pin/Unpin/Resign subcommand variants + handlers"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T4: +pin-gc-survives + unpin-gc-collects + resign subtest"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T4: +r[verify store.admin.pin] marker at subtests entry"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "T5: delete TODO(P0431) at :376"}
]
```

## Dependencies

```json deps
{"deps": [216], "soft_deps": [311], "note": "sprint-1 cleanup finding (discovered_from=rio-store cleanup worker). Hard-dep P0216 (cli clap structure exists). Soft-dep P0311 (T1-T3 there also touch cli.nix — additive subtests, non-overlapping). cli.nix is moderate-traffic; main.rs count check at dispatch."}
```

**Depends on:** [P0216](plan-0216-rio-cli-subcommands.md) — DONE.
**Conflicts with:** [`cli.nix`](../../nix/tests/scenarios/cli.nix) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T1-T3 and T43/T49 all append subtests. All additive, different subtest names.
