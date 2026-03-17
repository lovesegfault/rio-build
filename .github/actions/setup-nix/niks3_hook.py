#!/usr/bin/python3
"""niks3 post-build-hook + async uploader daemon + drain.

One script, three personalities. Stdlib only — no pip deps, so it runs
on a bare ARC runner image without any setup.

  hook    Nix calls this (via a 2-line shim) after every local build.
          Appends one JSON line to the queue and exits. Must be
          near-instant: per `man nix.conf`, the hook blocks all other
          builds while running, and a non-zero exit stops Nix from
          scheduling any further builds for the whole session.

  daemon  Started by setup-nix. Follows the queue, fetches a fresh
          OIDC token per entry (short TTL, builds are long), shells
          out to `niks3 push`, writes a completion marker. SIGTERM
          finishes the current upload before exiting.

  drain   Called by the niks3-push action (if: always()). Waits for
          completions to catch up with the queue, then SIGTERMs the
          daemon. This is what makes per-derivation caching survive
          a failed build: cargoArtifacts uploaded → clippy dies →
          next run substitutes cargoArtifacts from S3.

Queue format is JSON-lines. Each line is one derivation's outputs:

  {"out_paths": ["/nix/store/..."], "drv_path": "/nix/store/....drv",
   "ts": 1710000000.0}

The hook holds an exclusive flock during the write. Nix runs builds
in parallel (max-jobs=auto), so multiple hooks can fire at once;
flock serializes the append so we never get torn JSON.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path


# --- hook ------------------------------------------------------------------

def run_hook(queue: Path) -> int:
    """Append $OUT_PATHS to the queue. Fast, never fails.

    Nix sets exactly two env vars for us: OUT_PATHS (space-separated
    store paths) and DRV_PATH. Everything else — RUNNER_TEMP, NIKS3_URL,
    the OIDC request token — is stripped. That's why the queue path
    arrives as a CLI arg baked into the shim, not from the env.

    A non-zero exit here wedges the build session (no further builds
    scheduled), so we swallow every exception. A lost queue entry is
    a cache miss next time; a wedged build is a wasted CI hour.
    """
    try:
        out_paths = os.environ.get("OUT_PATHS", "").split()
        if not out_paths:
            return 0
        entry = {
            "out_paths": out_paths,
            "drv_path": os.environ.get("DRV_PATH", ""),
            "ts": time.time(),
        }
        line = json.dumps(entry, separators=(",", ":")) + "\n"
        # "a" → O_APPEND, so the write position is always EOF even
        # if another hook appended between our open and our write.
        # flock on top of that serializes the whole critical section.
        with open(queue, "a") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            try:
                f.write(line)
                f.flush()
            finally:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    except Exception as e:  # noqa: BLE001
        # Best effort to surface why, but exit 0 regardless.
        try:
            sys.stderr.write(f"niks3-hook: {e}\n")
        except Exception:  # noqa: BLE001
            pass
    return 0


# --- daemon ----------------------------------------------------------------

# Module-level flag so the signal handler (which can't take the daemon
# state as an arg) can flip it. The daemon loop checks it at every
# iteration boundary — never mid-upload.
_stop = False


def _sigterm(_signum: int, _frame: object) -> None:
    global _stop
    _stop = True


def _follow(path: Path) -> "iter[str]":
    """Yield appended lines forever. Poor man's tail -F.

    Stdlib only — no inotify. 200ms poll on EOF is fine: the daemon
    exists to keep uploads off the hook's critical path, not to be
    low-latency. Starts from line 1 so anything queued before the
    daemon came up (e.g. the niks3 build itself, on a cold cache)
    gets processed.
    """
    with open(path, "r") as f:
        while not _stop:
            line = f.readline()
            if line:
                yield line.rstrip("\n")
            else:
                time.sleep(0.2)


def _fetch_oidc(audience: str) -> str | None:
    """Fresh GitHub OIDC token. Stdlib urllib, no requests dep.

    GitHub OIDC tokens expire in ~5-10 minutes. VM test builds can
    run for 30+. Fetching per-push costs one in-cluster HTTP round
    trip — cheap — and keeps us always-valid.
    """
    token = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
    base = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_URL")
    if not token or not base:
        return None
    url = f"{base}&audience={audience}"
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return json.loads(r.read().decode())["value"]
    except (urllib.error.URLError, KeyError, json.JSONDecodeError) as e:
        sys.stderr.write(f"[niks3] OIDC fetch failed: {e}\n")
        return None


def _mark_done(done: Path, entry: dict) -> None:
    """Record completion. flock for symmetry with the hook.

    Drain compares line counts, so we write exactly one line per
    queue line — even when the push failed. Otherwise drain hangs
    on transient network blips.
    """
    line = json.dumps(entry, separators=(",", ":")) + "\n"
    with open(done, "a") as f:
        fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            f.write(line)
            f.flush()
        finally:
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)


def run_daemon(queue: Path, done: Path, niks3_bin: str, server_url: str) -> int:
    """Tail the queue, push each entry, log completion.

    SIGTERM sets _stop; we finish the in-flight upload and exit
    cleanly at the next loop boundary. SIGINT treated the same
    (someone ran the daemon by hand and hit ^C).

    Push failures are logged, not fatal. Cache population is
    best-effort; the worst outcome is a cache miss next run.
    """
    signal.signal(signal.SIGTERM, _sigterm)
    signal.signal(signal.SIGINT, _sigterm)

    sys.stderr.write(f"[niks3] daemon started, following {queue}\n")

    for line in _follow(queue):
        if not line:
            continue
        try:
            entry = json.loads(line)
            out_paths = entry.get("out_paths", [])
        except json.JSONDecodeError as e:
            sys.stderr.write(f"[niks3] skipping bad queue line: {e}\n")
            _mark_done(done, {"error": "bad_json", "raw": line[:200]})
            continue
        if not out_paths:
            _mark_done(done, {**entry, "pushed": False, "reason": "empty"})
            continue

        oidc = _fetch_oidc(server_url)
        if oidc is None:
            _mark_done(done, {**entry, "pushed": False, "reason": "no_oidc"})
            continue

        cmd = [niks3_bin, "push", "--server-url", server_url, "--auth-token", oidc, *out_paths]
        try:
            subprocess.run(cmd, check=True)
            _mark_done(done, {**entry, "pushed": True})
            sys.stderr.write(f"[niks3] pushed {len(out_paths)} path(s) from {entry.get('drv_path', '?')}\n")
        except subprocess.CalledProcessError as e:
            sys.stderr.write(f"[niks3] push failed (exit {e.returncode}): {out_paths}\n")
            _mark_done(done, {**entry, "pushed": False, "reason": f"exit_{e.returncode}"})
        except FileNotFoundError:
            # niks3 binary vanished — unrecoverable for this run.
            sys.stderr.write(f"[niks3] {niks3_bin} not found, bailing\n")
            _mark_done(done, {**entry, "pushed": False, "reason": "no_binary"})
            return 1

        if _stop:
            break

    sys.stderr.write("[niks3] daemon exiting\n")
    return 0


# --- drain -----------------------------------------------------------------

def _linecount(path: Path) -> int:
    try:
        with open(path, "rb") as f:
            return sum(1 for _ in f)
    except FileNotFoundError:
        return 0


def run_drain(queue: Path, done: Path, pid_file: Path, timeout: int) -> int:
    """Wait for done-count ≥ queued-count, then SIGTERM the daemon.

    Refreshes both counts each iteration — a straggler build might
    finish after the main step exits but before drain runs.

    Returns 0 even on timeout: cache push is not a CI gate. We emit
    a ::warning:: so it's visible in the GHA summary.
    """
    if not pid_file.exists():
        queued = _linecount(queue)
        print(f"[niks3] no daemon pid file — nothing to drain ({queued} queued, 0 uploaded)")
        return 0

    try:
        pid = int(pid_file.read_text().strip())
    except (ValueError, OSError) as e:
        print(f"[niks3] bad pid file ({e}) — skipping drain")
        return 0

    deadline = time.monotonic() + timeout
    queued = done_n = 0
    while time.monotonic() < deadline:
        queued = _linecount(queue)
        done_n = _linecount(done)
        if done_n >= queued:
            print(f"[niks3] drained: {done_n}/{queued}")
            break
        print(f"[niks3] waiting: {done_n}/{queued} uploaded…")
        time.sleep(2)
    else:
        print(f"::warning::niks3 drain timed out after {timeout}s ({done_n}/{queued})")

    # Try the process group first (daemon started under setsid, so
    # -pid hits its whole group including any niks3 child). Fall
    # back to the single pid if that fails.
    for target in (-pid, pid):
        try:
            os.kill(target, signal.SIGTERM)
            break
        except (ProcessLookupError, PermissionError):
            continue

    try:
        pid_file.unlink()
    except FileNotFoundError:
        pass

    # Summarize what actually got pushed vs skipped.
    pushed = skipped = 0
    try:
        with open(done) as f:
            for line in f:
                try:
                    if json.loads(line).get("pushed"):
                        pushed += 1
                    else:
                        skipped += 1
                except json.JSONDecodeError:
                    skipped += 1
    except FileNotFoundError:
        pass
    print(f"[niks3] summary: {pushed} pushed, {skipped} skipped/failed, {queued} total queued")

    return 0


# --- entry -----------------------------------------------------------------

def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--mode", required=True, choices=("hook", "daemon", "drain"))
    p.add_argument("--queue", type=Path, required=True)
    p.add_argument("--done", type=Path, help="daemon/drain only")
    p.add_argument("--niks3-bin", help="daemon only")
    p.add_argument("--server-url", help="daemon only")
    p.add_argument("--pid-file", type=Path, help="drain only")
    p.add_argument("--timeout", type=int, default=300, help="drain only")
    a = p.parse_args()

    if a.mode == "hook":
        return run_hook(a.queue)
    if a.mode == "daemon":
        if not (a.done and a.niks3_bin and a.server_url):
            p.error("--mode=daemon requires --done, --niks3-bin, --server-url")
        return run_daemon(a.queue, a.done, a.niks3_bin, a.server_url)
    if a.mode == "drain":
        if not (a.done and a.pid_file):
            p.error("--mode=drain requires --done, --pid-file")
        return run_drain(a.queue, a.done, a.pid_file, a.timeout)
    return 2


if __name__ == "__main__":
    sys.exit(main())
