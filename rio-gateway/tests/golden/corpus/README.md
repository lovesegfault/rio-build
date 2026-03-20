# CA wire-capture corpus

Golden wire bytes for `wopRegisterDrvOutput` (42) and `wopQueryRealisation` (43)
Realisation payloads. Captured for P0247 spike; future conformance tests compare
rio-gateway output against these.

## Provenance

Sourced from Nix's own round-trip golden fixtures at
`src/libstore-tests/data/worker-protocol/` in the locked flake input
`github:NixOS/Nix` rev `26842787496f2293c676fb36db38dacfd63497e0`
(narHash `sha256-qzVtneydMSjNZXzNbxQG9VvJc490keS9RNlbUCfiQas=`).

These are Nix's authoritative `CommonProto::Serialise<Realisation>` and
`Serialise<DrvOutput>` test fixtures — the exact bytes Nix itself validates
against. More authoritative than a one-shot strace capture: Nix's test suite
pins these to prevent wire drift.

Regenerate:

```bash
NIX_SRC=$(nix eval --raw .#inputs.nix.sourceInfo.outPath)
cp $NIX_SRC/src/libstore-tests/data/worker-protocol/realisation.bin \
   ca-register-2deep.bin
cp $NIX_SRC/src/libstore-tests/data/worker-protocol/realisation-with-deps.bin \
   ca-register-with-deps-historical.bin
cp $NIX_SRC/src/libstore-tests/data/worker-protocol/drv-output.bin \
   ca-query-2deep.bin
```

## Files

### `ca-register-2deep.bin` (560 bytes)

Two concatenated Realisation wire frames. Each frame: `[u64 LE json_len][json bytes][pad-to-8]`.
Both have `"dependentRealisations":{}` — the **only shape current Nix ever emits**
(see ADR-018).

| offset | bytes | decoded |
|---|---|---|
| 0x000 | `b0 00 00 00 00 00 00 00` | len = 176 |
| 0x008..0x0b7 | JSON | `{"dependentRealisations":{},"id":"sha256:15e3...d527!baz","outPath":"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo","signatures":[]}` |
| 0x0b8 | `6f 01 00 00 00 00 00 00` | len = 367 |
| 0x0c0..0x22e | JSON | same + 2 signatures `asdf:AAA...==`, `qwer:AAA...==` |
| 0x22f | `00` | pad (367 % 8 = 7, 1 byte) |

### `ca-register-with-deps-historical.bin` (496 bytes)

One Realisation wire frame with a **historical** non-empty `dependentRealisations`.
Nix keeps this fixture only for back-compat read testing — current Nix never
writes this shape (`realisation.cc` always emits `{}`). Useful for rio-gateway
defensive parsing tests.

| offset | bytes | decoded |
|---|---|---|
| 0x000 | `e4 01 00 00 00 00 00 00` | len = 484 |
| 0x008..0x1eb | JSON | `{"dependentRealisations":{"sha256:6f86...3f5!quux":"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo"},"id":"sha256:15e3...d527!baz","outPath":"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo","signatures":["asdf:AAA...==","qwer:AAA...=="]}` |
| 0x1ec..0x1ef | `00 00 00 00` | pad (484 % 8 = 4, 4 bytes) |

Historical shape (see ADR-018 for full context):

- **Flat** `{DrvOutput_string: StorePath_basename}` map
- Key format: same as `id` — `"sha256:<64-hex>!<output_name>"`
- Value format: same as `outPath` — store path basename (no `/nix/store/` prefix)

### `ca-query-2deep.bin` (176 bytes)

Two concatenated DrvOutput wire frames (the `wopQueryRealisation` request format).
Each frame: `[u64 LE str_len][str bytes][pad-to-8]`.

| offset | bytes | decoded |
|---|---|---|
| 0x000 | `4b 00 00 00 00 00 00 00` | len = 75 |
| 0x008..0x052 | `sha256:15e3c560...d527!baz` | DrvOutput id |
| 0x053..0x057 | `00 00 00 00 00` | pad (75 % 8 = 3, 5 bytes) |
| 0x058 | `4c 00 00 00 00 00 00 00` | len = 76 |
| 0x060..0x0ab | `sha256:6f869f9e...3f5!quux` | DrvOutput id |
| 0x0ac..0x0af | `00 00 00 00` | pad (76 % 8 = 4, 4 bytes) |
