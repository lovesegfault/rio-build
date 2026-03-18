# Plan 0126: Tracey adoption — spec-coverage tooling + 292 retroactive markers

## Design

**This is when tracey was adopted.** Four commits formalized the existing grep-enforced "keep docs/code in sync" discipline into tooling that CI, LSPs, and MCP can query directly. 123 spec rules defined in `docs/src/components/*.md`, 295 `r[impl]`/`r[verify]` annotations added retroactively across 40+ source files.

`f3957f7` packaged tracey (crane build from v1.3.0 source; dashboard `build.rs` stubbed — needs node+pnpm, breaks sandbox; `include_str!` assets stubbed; `--no-default-features` drops tantivy). Single spec "rio", prefix `r` (inferred), covers all 8 component docs. `.nix` files excluded — tracey doesn't parse them; VM-verified rules use `.sh` shims. Gateway annotated first (43 requirements, 100% impl+verify coverage): `r[gw.opcode.*]` (per-opcode wire contracts), `r[gw.wire.*]` (all-ints-u64, collection-max, narhash-hex), `r[gw.handshake.*]`, `r[gw.stderr.*]` (error-before-return rule from CLAUDE.md), `r[gw.conn.*]`, `r[gw.compat.*]`, `r[gw.dag.*]`, `r[gw.hook.*]`. Golden conformance tests carry the strongest `r[verify]` (byte-for-byte against real nix-daemon). 122 `r[impl]`/`r[verify]` annotations in this commit.

`bb0949c` annotated scheduler spec + state machine (24 requirements, 100% coverage, 51 annotations). `r[sched.actor.*]`, `r[sched.merge.*]`, `r[sched.state.*]` (1:1 with `validate_transition` match arms), `r[sched.worker.*]`, `r[sched.estimate.*]`, `r[sched.classify.*]`, `r[sched.lease.*]` (referencing P0114's K8s Lease, NOT the PG advisory lock the spec previously described — P0127 fixes the spec). `impl` on module headers; `verify` on `actor/tests/*` headers.

`813609f` completed the sweep: store/worker/controller/obs/sec/proto specs (126 total requirements, 124 with impl, 124 verified, 119 annotations added). The 2 uncovered (`store.gc.two-phase`, `store.gc.pending-deletes`) correctly surface GC as deferred to phase 3b+ — this is the signal tracey is designed for.

`787a5b4` added the CI check: `runCommand` that copies `cleanSource` to writable tmpdir, runs `tracey query validate`, greps for `"0 total error(s)"`. Uses `pkgs.lib.cleanSource` NOT `craneLib.cleanCargoSource` (tracey needs `docs/**/*.md` + `.config/tracey/config.styx` which crane's filter drops). `HOME=$TMPDIR` for daemon state dir writability. grep workaround: tracey v1.3.0 always exits 0 even on validation errors (upstream bug); grep makes the check actually fail. CLAUDE.md grew the "Spec traceability (tracey)" section.

**Every plan P0097-P0125 carries "markers added retroactively in f3957f7..813609f" in its `## Tracey` section** — the annotations describe code that already existed.

## Files

```json files
[
  {"path": "nix/tracey.nix", "action": "NEW", "note": "crane build from v1.3.0; dashboard build.rs stubbed; --no-default-features"},
  {"path": ".config/tracey/config.styx", "action": "NEW", "note": "single spec 'rio', prefix r, 8 component docs, .nix excluded"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "43 r[gw.*] spec markers"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "24 r[sched.*] spec markers"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "r[store.*] markers; 2 GC rules left uncovered (deferred)"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "r[worker.*] markers"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "r[ctrl.*] markers"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "r[proto.*] markers"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "r[obs.*] markers"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "r[sec.*] markers"},
  {"path": "rio-gateway/src/opcodes_read.rs", "action": "MODIFY", "note": "r[impl gw.opcode.*] on each handler"},
  {"path": "rio-gateway/src/opcodes_write.rs", "action": "MODIFY", "note": "r[impl gw.opcode.*]"},
  {"path": "rio-gateway/src/wire.rs", "action": "MODIFY", "note": "r[impl gw.wire.*]"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "MODIFY", "note": "r[verify gw.*] \u2014 strongest (byte-for-byte)"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "r[impl sched.actor.*] module header"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "r[impl sched.merge.*]"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "r[impl sched.dispatch.*]"},
  {"path": "rio-scheduler/src/lease.rs", "action": "MODIFY", "note": "r[impl sched.lease.*]"},
  {"path": "rio-scheduler/src/actor/tests/mod.rs", "action": "MODIFY", "note": "r[verify sched.*] on test module headers"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "r[impl worker.cgroup.*]"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "r[impl worker.fuse.*]"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "r[impl ctrl.*]"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "r[impl store.put.*]"},
  {"path": "flake.nix", "action": "MODIFY", "note": "tracey-validate in ciBaseDrvs"}
]
```

## Tracey

**This plan CREATED tracey.** 123 spec markers defined (`r[domain.*]` standalone paragraphs in `docs/src/`). 295 `r[impl]`/`r[verify]` annotations added across source files. `tracey query status` at merge: 126/126 rules, 124 impl, 124 verify (2 GC deferrals correctly uncovered).

## Entry

- Depends on P0125: all impl work done before annotating. Annotating in parallel would have created merge conflicts in 40+ files.

## Exit

Merged as `f3957f7..787a5b4` (4 commits). `.#ci` green at merge including new `tracey-validate` check. `tracey query validate` → `"0 total error(s)"`. `tracey query uncovered` → 2 (GC deferrals). In `.#ci`, `.#ci-local-fast`, `.#ci-fast`, `nix flake check`.
