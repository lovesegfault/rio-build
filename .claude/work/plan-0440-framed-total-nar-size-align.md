# Plan 440: align MAX_FRAMED_TOTAL with MAX_NAR_SIZE

`MAX_FRAMED_TOTAL` at
[`rio-nix/src/protocol/wire/framed.rs:12`](../../rio-nix/src/protocol/wire/framed.rs)
is 1 GiB. `MAX_NAR_SIZE` at
[`rio-common/src/limits.rs:12`](../../rio-common/src/limits.rs) is 4 GiB.
`FramedStreamReader::new` clamps its `max_total` argument to
`MAX_FRAMED_TOTAL` at
[`framed.rs:74`](../../rio-nix/src/protocol/wire/framed.rs):
`max_total.min(MAX_FRAMED_TOTAL)`.

The gateway's `wopAddToStoreNar` handler at
[`rio-gateway/src/handler/opcodes_write.rs:74`](../../rio-gateway/src/handler/opcodes_write.rs)
accepts `nar_size ≤ MAX_NAR_SIZE` (4 GiB), then passes `nar_size` to
`FramedStreamReader::new` at
[`opcodes_write.rs:118`](../../rio-gateway/src/handler/opcodes_write.rs).
For a NAR between 1 GiB and 4 GiB (CUDA toolkits, chromium, ML model
weights), the gateway accepts the size but the framed reader silently clamps
to 1 GiB — the upload fails mid-stream with a confusing "framed total
exceeded" error instead of the intended size-gate message.

The comment at
[`opcodes_write.rs:112-114`](../../rio-gateway/src/handler/opcodes_write.rs)
— "tighter bound than MAX_FRAMED_TOTAL" — is wrong for `nar_size > 1 GiB`:
the clamp makes `MAX_FRAMED_TOTAL` the effective bound, not `nar_size`.

Origin: bughunter sweep, report `bug_030` at
`/tmp/bughunter/prompt-6701aef0_2/reports/`.

## Tasks

### T1 — `fix(nix):` set MAX_FRAMED_TOTAL = MAX_NAR_SIZE

At [`framed.rs:12`](../../rio-nix/src/protocol/wire/framed.rs), replace the
literal:

```rust
use rio_common::limits::MAX_NAR_SIZE;

/// Maximum total size for framed stream reassembly. Must be ≥ MAX_NAR_SIZE
/// so the gateway's wopAddToStoreNar size check (limits.rs:12) is the
/// effective gate, not this clamp.
pub const MAX_FRAMED_TOTAL: u64 = MAX_NAR_SIZE;
```

`rio-nix` already depends on `rio-common` (verify with
`grep rio-common rio-nix/Cargo.toml`; if not present, add the workspace
dep). Update the clamp test at
[`framed.rs:390-394`](../../rio-nix/src/protocol/wire/framed.rs) — it
likely asserts `max_total == 1 GiB` after passing `u64::MAX`; adjust to
`MAX_NAR_SIZE`.

### T2 — `fix(gateway):` correct MAX_FRAMED_TOTAL comment

At
[`opcodes_write.rs:112-114`](../../rio-gateway/src/handler/opcodes_write.rs),
replace:

```rust
// Wrap reader in FramedStreamReader for the NAR bytes. max_total =
// nar_size (client-declared) — tighter bound than MAX_FRAMED_TOTAL;
// a lying client sending more than declared trips the reader's limit.
```

with:

```rust
// Wrap reader in FramedStreamReader for the NAR bytes. max_total =
// nar_size (client-declared). MAX_FRAMED_TOTAL == MAX_NAR_SIZE, so
// the nar_size check above is the effective gate; this clamp is
// defense-in-depth. A lying client sending more than declared trips
// the reader's limit.
```

### T3 — `fix(nix):` const_assert MAX_FRAMED_TOTAL ≥ MAX_NAR_SIZE

Add [`static_assertions`](https://docs.rs/static_assertions) to
`rio-nix/Cargo.toml` `[dependencies]` (workspace dep if already in root
`Cargo.toml`, otherwise `static_assertions = "1"`). At the top of
`framed.rs` after the const definitions:

```rust
static_assertions::const_assert!(MAX_FRAMED_TOTAL >= rio_common::limits::MAX_NAR_SIZE);
```

This guards against future drift — if someone bumps `MAX_NAR_SIZE` without
touching `MAX_FRAMED_TOTAL`, compilation fails. The assertion is trivially
satisfied today (equality) but encodes the invariant.

## Exit criteria

- `/nixbuild .#ci` green
- `const_assert!` compiles (implicit in build success)
- `grep MAX_NAR_SIZE rio-nix/src/protocol/wire/framed.rs` shows the import + const reference
- Clamp test at `framed.rs:~390` asserts against `MAX_NAR_SIZE` not `1 GiB` literal

## Tracey

References existing markers:
- `r[gw.wire.framed-no-padding]` — T1 touches the framed reader this marker covers
- `r[gw.opcode.add-to-store-nar.framing]` — T2 corrects the comment in the handler this marker covers

Adds new marker to component specs:
- `r[gw.wire.framed-max-total]` → `docs/src/components/gateway.md` (see ## Spec additions below) — T1+T3 implement

## Spec additions

Add to `docs/src/components/gateway.md` after line 384 (after
`r[gw.wire.framed-no-padding]`, before `r[gw.wire.narhash-hex]`):

```markdown
r[gw.wire.framed-max-total]
`MAX_FRAMED_TOTAL` MUST equal `MAX_NAR_SIZE`. The gateway's
`wopAddToStoreNar` handler gates on `nar_size ≤ MAX_NAR_SIZE` before
constructing the `FramedStreamReader`; if `MAX_FRAMED_TOTAL <
MAX_NAR_SIZE`, the reader's internal clamp silently shrinks the
effective limit, causing NARs between the two bounds to fail
mid-stream with a confusing framed-total error instead of the upfront
size-gate message. A `const_assert!` enforces the inequality at
compile time.
```

## Files

```json files
[
  {"path": "rio-nix/src/protocol/wire/framed.rs", "action": "MODIFY", "note": "T1: MAX_FRAMED_TOTAL = MAX_NAR_SIZE; T3: const_assert; update clamp test"},
  {"path": "rio-nix/Cargo.toml", "action": "MODIFY", "note": "T3: add static_assertions dep (if not already present via workspace)"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "T2: fix MAX_FRAMED_TOTAL comment at :112-114"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T1: r[gw.wire.framed-max-total] marker"}
]
```

```
rio-nix/
├── Cargo.toml                        # T3: static_assertions dep
└── src/protocol/wire/
    └── framed.rs                     # T1+T3: const + assert + test update
rio-gateway/src/handler/
└── opcodes_write.rs                  # T2: comment fix
docs/src/components/
└── gateway.md                        # new marker
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "standalone const fix — no upstream deps"}
```

**Depends on:** none. `MAX_NAR_SIZE` is a stable const in `rio-common`;
raising `MAX_FRAMED_TOTAL` to match is a pure relaxation (nothing that
passed before will fail after).

**Conflicts with:** `rio-gateway/src/handler/opcodes_write.rs` and
`rio-nix/src/protocol/wire/framed.rs` are not in collisions top-30. No
serialization needed. Independent of
[P0439](plan-0439-nar-entry-name-validation.md) (different
files).
