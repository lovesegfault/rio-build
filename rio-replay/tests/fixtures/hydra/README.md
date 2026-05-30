# Recorded hydra.nixos.org fixtures

Responses recorded from hydra.nixos.org on 2026-05-26 for evaluation
1824219 (jobset `nixos/unstable`), captured during the design spike that
preceded the rio-replay implementation. The unit tests in
`rio-replay/src/hydra.rs` (and the eval-recipe tests built on top of
them) parse these files and assert against the recorded values.

| File | Endpoint |
| --- | --- |
| `eval-1824219.json.zst` | `GET /eval/1824219` (zstd-compressed, see below) |
| `jobset-nixos-unstable.json` | `GET /jobset/nixos/unstable` |
| `job-<job>.json` (11 files) | `GET /eval/1824219/job/<job>` |

## Byte-exact — do not edit

Every recorded response is committed byte-for-byte as received: no
reformatting, no re-indenting, no trailing-newline "fixes". The repo's
`end-of-file-fixer` pre-commit hook is excluded for this directory (see
`flake.nix`) precisely so it cannot append newlines to the recorded
bytes. A "cleaned up" fixture no longer documents what Hydra actually
serves, and the tests assert exact recorded values.

The one exception to "as received" is `eval-1824219.json.zst`: the raw
eval response is ~1.6 MB (a 161,643-entry `builds` array), which the
`check-added-large-files` pre-commit hook (500 KB cap) would reject, so
it is committed zstd-compressed. Decompressing it yields the byte-exact
recorded response; the tests decompress at runtime. Never commit the
decompressed `eval-1824219.json`.

This README is documentation, not a recorded response.

## Re-recording (only if ever needed)

hydra.nixos.org is a shared, load-sensitive service — stay polite:

- Issue the requests sequentially (no parallel fetches), at least one
  second apart. The full set is 13 requests; stay under ~15 total.
- Send `Accept: application/json` and a descriptive User-Agent that
  includes contact information, e.g.
  `rio-replay-fixture-rerecord/0.1 (+https://github.com/lovesegfault/rio-build; contact: <email>)`.
- URLs: `https://hydra.nixos.org/eval/1824219`,
  `https://hydra.nixos.org/jobset/nixos/unstable`, and
  `https://hydra.nixos.org/eval/1824219/job/<job>` for each
  `job-<job>.json` file present here.
- Save each body unmodified; re-compress the eval response with
  `zstd -19` and commit only the `.zst`.
- Re-check every recorded value asserted in tests (build ids, drvPaths,
  release names, output paths, the `builds.len()` count) against the
  fresh responses, and note the re-recording in the commit message.
