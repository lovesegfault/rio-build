---
name: nixbuild
description: Local nix build with structured log lifecycle. Use for .#ci / .#coverage-full runs from agent context to avoid flooding it with build output.
---

## Invocation

```bash
.claude/bin/nixbuild .#ci
.claude/bin/nixbuild .#checks.x86_64-linux.vm-lifecycle   # single check
.claude/bin/nixbuild .#coverage-full --keep-going         # extra args pass through
```

**Quiet by default.** Stderr gets one line (the log path); stdout gets a JSON report at the end. No 3MB build log streamed into context. For interactive debugging where you want live output, use plain `nix build -L` instead.

**Callers jq.** Once a build is attempted, process exit is 0 — `rc` is in the JSON. (Missing target arg exits 1 with usage.)

```bash
report=$(.claude/bin/nixbuild .#ci)
rc=$(jq -r .rc <<<"$report")
[[ $rc -eq 0 ]] || jq -r .log_tail <<<"$report"
```

## Output

```json
{"rc": 0, "log": "/tmp/rio-dev/rio-<branch>-<n>.log", "log_bytes": 123456, "store_path": "/nix/store/...", "drv_failed": [], "log_tail": ""}
```

| Field | Meaning |
|---|---|
| `rc` | nix build exit code; 0 = green |
| `log` | full build log path (always kept) |
| `store_path` | first output path (empty on failure) |
| `drv_failed` | list of `.drv` paths that failed (parsed from log) |
| `log_tail` | last 80 lines on failure, empty on success |

Logs accumulate at `/tmp/rio-dev/rio-<branch>-<n>.log` — `<n>` auto-increments per branch.
