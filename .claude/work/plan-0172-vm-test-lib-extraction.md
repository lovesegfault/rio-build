# Plan 0172: VM test lib extraction — pki/protoset/derivations + standalone fixture

## Design

Pure extractions; `phase2{a,b,c}` + `phase3b` now import from `nix/tests/lib/`. Groundwork for the topology×scenario rearchitecture — these pieces get reused by `scenarios/security.nix`, `scenarios/lifecycle.nix`, and the standalone fixture.

**`lib/pki.nix`:** phase3b's openssl CA + server/client/gateway certs + HMAC key. Parameterized by `serverSans` (was hardcoded `DNS:control,DNS:localhost`) so k3s scenarios can add Service DNS names.

**`lib/protoset.nix`:** protoc `FileDescriptorSet` for `grpcurl`.

**`lib/derivations/`:** `git mv` of `phase2{a,b,c}-derivation.nix` → `fanout`/`chain`/`sizeclass`. Content unchanged — these are in-VM `nix-build` targets taking `{ busybox }`, not host-eval modules. Kept as separate files (not `pkgs.writeText`) so in-VM errors show a filename.

**`lib/derivations/cold-bootstrap.nix`:** new. The smoke-test `builtin:fetchurl` busybox FOD + raw consumer pattern — the only shape that works on an empty store. For the upcoming protocol-cold scenario.

**`lib/derivations.nix`:** index + `mkTrivial` `writeText` factory for per-scenario distinct leaves (avoids DAG-dedup across sections).

**`lib/assertions.py` (`147cd34`):** shared Python helpers for `testScript`s — metric scrape helpers, wait predicates.

**`fixtures/standalone.nix` (`f802ebd`):** NixOS-modules deployment fixture. Decouples topology (control+worker systemd units on one VM) from scenario (what to test).

## Files

```json files
[
  {"path": "nix/tests/lib/pki.nix", "action": "NEW", "note": "openssl CA + certs + HMAC; serverSans param"},
  {"path": "nix/tests/lib/protoset.nix", "action": "NEW", "note": "protoc FileDescriptorSet for grpcurl"},
  {"path": "nix/tests/lib/derivations.nix", "action": "NEW", "note": "index + mkTrivial factory"},
  {"path": "nix/tests/lib/derivations/fanout.nix", "action": "NEW", "note": "git mv phase2a-derivation.nix"},
  {"path": "nix/tests/lib/derivations/chain.nix", "action": "NEW", "note": "git mv phase2b-derivation.nix"},
  {"path": "nix/tests/lib/derivations/sizeclass.nix", "action": "NEW", "note": "git mv phase2c-derivation.nix"},
  {"path": "nix/tests/lib/derivations/cold-bootstrap.nix", "action": "NEW", "note": "builtin:fetchurl busybox FOD pattern"},
  {"path": "nix/tests/lib/assertions.py", "action": "NEW", "note": "shared Python helpers for testScripts"},
  {"path": "nix/tests/fixtures/standalone.nix", "action": "NEW", "note": "NixOS-modules deployment fixture"},
  {"path": "nix/tests/phase2a.nix", "action": "MODIFY", "note": "import lib/ instead of inline"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "import lib/"},
  {"path": "nix/tests/phase2c.nix", "action": "MODIFY", "note": "import lib/"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "import lib/; net -101 lines"}
]
```

## Tracey

No tracey markers — pure extraction, no behavior change.

## Entry

- Depends on P0148: phase 3b complete

## Exit

Merged as `a75a33d`, `147cd34`, `f802ebd` (3 commits). All four affected phase tests eval-verified unchanged.
