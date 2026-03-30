# Plan 990719404: onibus merge dag-flip — git commit --amend exit 128 under subprocess

`onibus merge dag-flip` calls `git("commit", "--amend", "--no-edit", ...)` at [`merge.py:258`](../../.claude/lib/onibus/merge.py). P0504 merger hit **exit 128** here at mc=53; recovered manually. The `git()` wrapper at [`git_ops.py:35`](../../.claude/lib/onibus/git_ops.py) is:

```python
subprocess.run(["git", *args], cwd=..., capture_output=True, text=True, check=True)
```

No `env=` kwarg. By default `subprocess.run` inherits `os.environ`, so straight env inheritance isn't the failure — but `capture_output=True` means stdin/stdout/stderr are pipes, **not a TTY**. If `commit.gpgsign=true` is in git config, `--amend` re-signs, and GPG (or the SSH signing helper) may need TTY (`GPG_TTY`) or an interactive prompt.

The user fixed a signing issue in the same session — likely the same root cause. The manual recovery worked because the interactive shell HAS a TTY and all signing env.

Exit 128 is git's generic "fatal error" code; the actual message was captured to `CalledProcessError.stderr` but not surfaced (the wrapper's `check=True` raises with stderr in the exception, but `dag_flip`'s caller may not have logged it).

## Tasks

### T1 — `fix(tooling):` diagnose + fix — surface stderr, then choose env-passthrough vs signing-bypass

**Diagnosis first** (at dispatch, before the fix): reproduce by running `onibus merge dag-flip` from the merger's actual spawn context. Check:

```bash
git config --get commit.gpgsign        # likely "true" if user fixed signing
git config --get gpg.format            # "ssh" or default gpg
echo $SSH_AUTH_SOCK $GPG_TTY           # present in interactive, absent in subagent spawn?
```

**Fix path A (preferred if signing IS the cause):** the `--amend` at `:258` is amending a commit that was JUST created by the merger itself (the dag.jsonl status flip goes into the implementation commit). That commit is already signed. `--amend --no-edit` with no content change is a **no-op amend** — the tree is identical, the message is identical. It only exists to bump the committer timestamp after `count_bump`. **If the amend is genuinely a no-op** (verify: `git diff HEAD^` post-amend is empty), replace it with `git commit --amend --no-edit --reuse-message=HEAD` ... no, that still re-signs.

Actually the cleanest fix: **don't amend at all if the tree didn't change**. Check `git diff --cached --quiet` before `:258`; if clean, skip the amend. The `amend_sha` returned at `:259` then becomes `git rev-parse --short HEAD` without the amend.

**Fix path B (if amend IS necessary):** MODIFY [`git_ops.py`](../../.claude/lib/onibus/git_ops.py) — add an optional `interactive: bool = False` param to `git()`. When `interactive=True`, drop `capture_output=True` (let stdin/stdout pass through), which gives signing helpers a real stdin. `merge.py:258` passes `interactive=True`.

**Fix path C (stopgap):** set `GIT_COMMITTER_DATE` explicitly and use `--amend --no-edit -c commit.gpgsign=false` ONLY IF the original commit is already signed (check `git log -1 --format=%G?` returns `G`). This preserves the signature by not re-creating it. **Do NOT take this path without explicit confirmation — bypassing signing is policy-relevant.**

### T2 — `fix(tooling):` surface subprocess stderr on CalledProcessError

Regardless of T1's chosen path, `git()` swallows stderr on success and raises `CalledProcessError` on failure — but the caller at `merge.py:258` doesn't catch-and-log. Next failure is equally opaque.

MODIFY [`git_ops.py:33`](../../.claude/lib/onibus/git_ops.py). Wrap `subprocess.run` in try/except, re-raise with stderr in the message:

```python
def git(*args: str, cwd: Path | None = None) -> str:
    """Run git, return stdout stripped. Non-zero → CalledProcessError
    with stderr surfaced in the message (not just the exception repr)."""
    try:
        out = subprocess.run(
            ["git", *args], cwd=cwd or REPO_ROOT,
            capture_output=True, text=True, check=True,
        )
    except subprocess.CalledProcessError as e:
        # Surface stderr — CalledProcessError.__str__ includes returncode
        # and cmd but NOT stderr. P0504 merger hit exit 128 here and the
        # actual git error was invisible.
        raise subprocess.CalledProcessError(
            e.returncode, e.cmd,
            output=e.output,
            stderr=f"git {' '.join(args)} failed:\n{e.stderr}",
        ) from None
    return out.stdout.strip()
```

Alternatively, log `e.stderr` at ERROR before re-raising. Either way: the NEXT exit-128 tells you WHY.

## Exit criteria

- Reproduce the exit-128 in the merger's spawn context (or prove it no longer reproduces after the session's signing fix)
- T2: `git()` failures surface stderr — verified by forcing a failure (`git("commit", "--amend")` with no staged changes in a fresh repo) and checking the exception message contains git's actual error
- T1 path chosen based on diagnosis — if path-A (skip-no-op-amend), `merge.py:258` guarded by `git diff --cached --quiet`; if path-B, `git()` has `interactive` param
- `onibus merge dag-flip` completes without manual intervention on the next real merge

## Tracey

No markers — tooling code is not spec-covered. `.claude/lib/onibus/*.py` is outside tracey's scan paths.

## Files

```json files
[
  {"path": ".claude/lib/onibus/git_ops.py", "action": "MODIFY", "note": "T2: surface stderr in CalledProcessError at :35; T1-path-B optional interactive param"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1-path-A: guard :258 amend with git diff --cached --quiet skip; OR T1-path-B: pass interactive=True"}
]
```

```
.claude/lib/onibus/
├── git_ops.py        # T2: stderr surface; T1-B: interactive param
└── merge.py          # T1-A: no-op-amend skip OR T1-B: interactive=True
```

## Dependencies

```json deps
{"deps": [504], "soft_deps": [], "note": "discovered_from=504 (P0504 merger hit the failure). P0504 DONE — no code dep, just the discovery origin."}
```

**Depends on:** [P0504](plan-0504-scheduler-resource-fit-filter.md) — discovery origin only (DONE).

**Conflicts with:** `merge.py` count unknown (not in top-20). `git_ops.py:33` is a 10-line wrapper touched by zero active plans. [P0304](plan-0304-trivial-batch-p0222-harness.md) T491/T498 touch `merge.py` at different sites (`:556` docstring, amend-timeout — T498 is in the SAME vicinity as `:258`, check at dispatch).

## Risks

- **Possibly already fixed:** the user fixed signing in the same session. If that fix was `git config --global commit.gpgsign false` or configuring a non-interactive signer, the exit-128 may not reproduce. **T2 is still valuable regardless** — the next surprise exit-128 (from a DIFFERENT cause) needs stderr to diagnose.
- **Path-A assumption:** "no-op amend" needs verification. `merge.py:258` runs AFTER `count_bump()` at `:254`, which may or may not stage changes. If `count_bump` stages a mc-log file, the amend is NOT a no-op and path-A's skip-guard never fires. Trace `count_bump` at dispatch.
- **Don't unilaterally bypass hooks/signing:** the followup description suggested `PRE_COMMIT_ALLOW_NO_CONFIG` as a "wrong fix" — correct, it bypasses hooks. Same for `-c commit.gpgsign=false`. T1-path-C is included for completeness but needs explicit approval. T1-path-A (skip if no-op) and path-B (give it a TTY) are both policy-clean.
