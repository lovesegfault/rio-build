# Plan 983457304: Fix rio-tracey-validate daemon startup flake in nix sandbox

`rio-tracey-validate` (CI check at [`flake.nix:637-654`](../../flake.nix))
failed 2× this session (P0460 run 1, P0483 run 2) with:

```
Error getting status: Cancelled
Daemon failed to start within 5s (No such file or directory)
```

Infrastructure flake — tracey daemon socket startup races in the nix
sandbox under parallel-build load. The check does `tracey query validate`
which spawns a daemon, waits for socket readiness, then queries. Under
load, the 5s socket-wait times out.

Fix options:
- **Option A (increase timeout):** bump tracey's daemon-startup timeout
  from 5s to 15-30s. May need upstream tracey patch or an env var if
  tracey exposes one (`TRACEY_DAEMON_TIMEOUT`?). Check `tracey --help`.
- **Option B (retry wrapper):** wrap `tracey query validate` in a bash
  retry loop in the `runCommand` body. Pure flake.nix change.
- **Option C (no-daemon mode):** if tracey supports a `--no-daemon` or
  one-shot mode that scans without the persistent daemon, use that.
  Daemon is for caching across sessions — CI runs are one-shot anyway.

**Option B is preferred** as the minimal fix — retry-once covers the race
without touching tracey internals. Option C if tracey supports it (cleaner
semantically). Option A only if tracey exposes the timeout knob.

## Tasks

### T1 — `fix(ci):` tracey-validate — retry-once around daemon startup

MODIFY [`flake.nix:646-654`](../../flake.nix). Wrap the `tracey query
validate` invocation in a retry:

```nix
''
  cp -r $src $TMPDIR/work
  chmod -R +w $TMPDIR/work
  cd $TMPDIR/work
  rm -rf .tracey/
  export HOME=$TMPDIR
  set -o pipefail
  # Retry once: daemon-socket startup races under sandbox load.
  # First attempt may hit "Daemon failed to start within 5s".
  tracey query validate 2>&1 | tee $out || {
    echo "retry: first tracey attempt failed, retrying once" >&2
    rm -rf .tracey/  # clear partial daemon state
    sleep 2
    tracey query validate 2>&1 | tee $out
  }
'';
```

Check at dispatch whether tracey supports `TRACEY_DAEMON_TIMEOUT` env or
`--no-daemon` — if so, prefer that over retry (document why in the
comment).

### T2 — `fix(flake-registry):` add known-flake entry for rio-tracey-validate

This plan's implementation adds the entry via `onibus flake add` in the
worktree and commits it alongside T1. See `## Known-flake entry` below.

## Exit criteria

- `/nixbuild .#checks.x86_64-linux.tracey-validate` green (retry covers the race)
- `grep 'retry.*tracey\|TRACEY_DAEMON_TIMEOUT\|--no-daemon' flake.nix` → ≥1 hit in the tracey-validate derivation
- `grep 'rio-tracey-validate' .claude/known-flakes.jsonl` → ≥1 hit with `fix_owner` = this plan's final number

## Tracey

No domain markers — CI infrastructure fix, not spec behavior.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: retry-once (or timeout/no-daemon) at tracey-validate :646-654"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T2: add rio-tracey-validate entry"}
]
```

```
flake.nix                   # T1: retry wrapper at :646-654
.claude/known-flakes.jsonl  # T2: flake registry entry
```

## Known-flake entry

```json
{"test":"rio-tracey-validate","symptom":"Error getting status: Cancelled after Daemon failed to start within 5s","root_cause":"tracey daemon socket startup race in nix sandbox under parallel-build load","fix_owner":"P983457304","fix_description":"retry-once wrapper around tracey query validate (or --no-daemon mode if supported)","retry":"Once"}
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [304, 311], "note": "No hard deps — tracey-validate check has existed since phase-2. Soft-dep P0304 (T251 also adds a helm-lint known-flake entry — same known-flakes.jsonl, additive). Soft-dep P0311 (multiple flake.nix helm-lint asserts — different section :660+, non-overlapping with :637-654). discovered_from=coverage-sink."}
```

**Depends on:** none — infrastructure flake independent of feature work.
**Conflicts with:** [`flake.nix`](../../flake.nix) is hot (helm-lint asserts at `:660+`, coverage at `:1200+`) — T1 edits `:646-654` which is the tracey-validate derivation ONLY. Non-overlapping with other flake.nix plans.
