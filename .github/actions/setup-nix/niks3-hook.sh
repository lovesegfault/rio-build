#!/usr/bin/env bash
# Nix post-build-hook: queue output paths for async upload to niks3.
#
# Nix invokes this synchronously after every locally-built derivation
# completes (NOT on substituted paths). It blocks further builds while
# running, and if it exits non-zero, Nix stops scheduling any more
# builds for the rest of the session. So it has exactly one job: be
# instant and never fail.
#
# The daemon runs this as root with a stripped environment — only
# $DRV_PATH and $OUT_PATHS survive. We can't rely on $NIKS3_URL,
# $RUNNER_TEMP, OIDC env, or anything set by the workflow. So the
# queue file path is baked in at install time: setup-nix/action.yml
# sed-replaces __NIKS3_QUEUE__ with the resolved $RUNNER_TEMP path
# before this script is ever invoked.
#
# $OUT_PATHS is space-separated store paths (one or more outputs of
# a single derivation). We write the whole thing as one line: the
# uploader daemon word-splits it back into args for a single `niks3
# push` call, which is more efficient than one RPC per output.
#
# The `>>` append is atomic on Linux for writes under PIPE_BUF (4096
# bytes), and bash's builtin echo emits one write() call. Store paths
# are ~60 bytes each; a derivation with even 20 outputs is ~1.2k.
# So concurrent hook invocations (max-jobs=auto) won't interleave.
#
# `exec` replaces the shell so there's no subshell/fork overhead, and
# `|| true` is belt-and-braces against ENOSPC or a filesystem hiccup:
# a dropped queue entry is a cache miss next time, but a hook failure
# wedges the entire build session.

exec echo "$OUT_PATHS" >> __NIKS3_QUEUE__ || true
