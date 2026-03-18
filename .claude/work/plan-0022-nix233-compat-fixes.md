# Plan 0022: Nix 2.33 compat — stderr loop hang, SSH EOF abort, hex fingerprints

## Context

P0021 isolated the golden daemon but didn't pin its version — it spawned whatever `nix-daemon` was on `PATH`. When the dev environment's nixpkgs bumped to Nix 2.33, three tests broke, each for a different reason. One was an upstream Nix regression (`QueryPathInfo` sending `STDERR_ERROR` instead of `STDERR_LAST + u64(0)` for not-found paths, NixOS/nix#15134). One was a latent rio bug that Nix 2.33's changed channel-close behavior exposed. One was a store-path hash encoding bug that had been producing wrong paths all along but only visibly when cross-validated against 2.33's stricter assertions.

Three independent failures, one version bump. Version pinning (`nix = nixVersions.nix_2_33`, which was at 2.33.3 post-fix) is the structural mitigation; the three fixes are each individually load-bearing.

## Commits

- `bda7607` — fix(rio-build): pin nix to 2.33.3 and fix read_stderr_loop hang
- `d4b04d4` — fix(rio-build): don't abort SSH tasks on channel EOF
- `cab5133` — fix(rio-nix): use hex encoding in store path fingerprints, not nixbase32

Three commits, three distinct bugs with no shared cause.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "pin nix = nixVersions.nix_2_33 (avoids 2.32-2.33.2 QueryPathInfo regression NixOS/nix#15134)"},
  {"path": "rio-build/tests/golden/daemon.rs", "action": "MODIFY", "note": "read_stderr_loop treats STDERR_ERROR as terminal (was continue-after-parse)"},
  {"path": "rio-build/src/gateway/server.rs", "action": "MODIFY", "note": "channel_eof drops client_tx only; does NOT remove ChannelSession (whose Drop aborts tasks)"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "make_text + make_fixed_output fingerprints use hex(hash), not nixbase32(hash)"}
]
```

## Design

**STDERR_ERROR hang (`bda7607`).** Golden tests' `read_stderr_loop` parsed `STDERR_ERROR` correctly but then continued the loop — expecting another STDERR frame to follow. `STDERR_ERROR` is terminal; the daemon sends it then waits for the next opcode. The loop waited for a frame the daemon was never going to send. Hang until test timeout. The bug predates 2.33; it surfaced because 2.32–2.33.2 had a regression (`QueryPathInfo` not-found → `STDERR_ERROR` instead of `STDERR_LAST + u64(0)`) that exercised this code path in tests that previously only saw `STDERR_LAST`. The upstream regression was fixed in 2.33.3 — but rio's loop bug was real regardless.

**SSH channel EOF abort (`d4b04d4`).** Latent since P0001. `channel_eof` removed the `ChannelSession` from the session map. `ChannelSession`'s `Drop` aborted the protocol handler and response-pump tasks. Aborted tasks never send SSH EOF/close back to the client. The client's SSH subprocess hangs forever waiting for close, holding the stdout/stderr pipes open. Nix ≤2.32 never triggered this — it kept the channel open for the entire build. Nix 2.33 closes the channel immediately after uploading derivations and opens a fresh one for the build. The first channel's EOF aborted the second channel's tasks. Fix: `channel_eof` drops only `client_tx` (signaling end-of-input to the protocol task), letting the tasks complete naturally and send proper SSH close.

**Hex fingerprints (`cab5133`).** P0015's `make_text` and `make_fixed_output` computed CA store paths from a fingerprint string containing the content hash. The hash was nixbase32-encoded. Nix uses hex (`HashFormat::Base16`) in fingerprints. Different encoding → different fingerprint → different SHA-256 → different store path. Every CA store path rio computed was wrong. The mismatch surfaced as `Assertion path2 == path failed` inside nix when the client's expected path didn't match the server's computed path. This is the CLAUDE.md "Wire encoding conventions" lesson's twin: the same hash bytes appear in different encodings in different contexts, and guessing wrong is silent until cross-validated.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.ssh.eof`, `nix.storepath.ca.fingerprint`.

## Outcome

Pinned daemon version for reproducible goldens. STDERR_ERROR terminates the stderr loop. SSH channels close gracefully under Nix 2.33's channel-per-operation behavior. CA store paths match Nix's computation. Three unrelated bugs, one version bump to find them all — the strongest argument for regular dependency bumps as a bug-discovery tool.
