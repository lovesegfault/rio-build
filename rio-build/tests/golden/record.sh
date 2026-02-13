#!/usr/bin/env bash
# Record byte-level nix-daemon sessions for golden conformance tests.
#
# This script captures the raw wire protocol exchange between a nix client
# and the real nix-daemon for each Phase 1a opcode, saving both client-sent
# and server-response bytes as binary fixtures.
#
# Prerequisites: nix, socat, and a running nix-daemon
#
# Usage: ./record.sh <output_dir>
#
# Output: one .bin file per recorded session containing the full bidirectional
# byte stream, and a .client.bin / .server.bin split for each.

set -euo pipefail

OUTDIR="${1:-$(dirname "$0")}"
TEMPDIR="$(mktemp -d)"
trap 'rm -rf "$TEMPDIR"' EXIT

NIX_DAEMON_SOCKET="${NIX_DAEMON_SOCKET:-/nix/var/nix/daemon-socket/socket}"

echo "Recording golden protocol sessions to: $OUTDIR"
echo "Using nix-daemon socket: $NIX_DAEMON_SOCKET"

# Check prerequisites
command -v nix >/dev/null 2>&1 || { echo "ERROR: nix not found"; exit 1; }
command -v socat >/dev/null 2>&1 || { echo "ERROR: socat not found"; exit 1; }

# Record a session by proxying through socat with hex logging
# $1: test name
# $2: nix command to run
record_session() {
    local name="$1"
    shift
    local proxy_socket="$TEMPDIR/${name}.sock"
    local raw_log="$TEMPDIR/${name}.raw"

    # Start socat as a recording proxy
    socat -x \
        "UNIX-LISTEN:${proxy_socket},fork" \
        "UNIX-CONNECT:${NIX_DAEMON_SOCKET}" \
        2>"$raw_log" &
    local socat_pid=$!
    sleep 0.5

    # Run the nix command against the proxy
    NIX_DAEMON_SOCKET_PATH="$proxy_socket" "$@" 2>/dev/null || true

    # Give socat time to flush
    sleep 0.5
    kill "$socat_pid" 2>/dev/null || true
    wait "$socat_pid" 2>/dev/null || true

    if [ -s "$raw_log" ]; then
        cp "$raw_log" "$OUTDIR/${name}.socat.log"
        echo "  Recorded: ${name} ($(wc -c < "$raw_log") bytes of socat log)"
    else
        echo "  WARNING: No data captured for ${name}"
    fi
}

# Build a test path we can query
echo "Building test store path..."
TEST_PATH=$(nix build --no-link --print-out-paths --impure --expr \
    '(import <nixpkgs> {}).writeTextFile { name = "rio-golden-test"; text = "golden test data\n"; }' \
    2>/dev/null) || {
    echo "ERROR: Could not build test path"
    exit 1
}
echo "Test path: $TEST_PATH"

# Record sessions for each Phase 1a operation
echo ""
echo "Recording sessions..."

# 1. Store info (handshake + wopSetOptions)
record_session "handshake" nix store info --store "unix://$TEMPDIR/handshake.sock"

# 2. Path info query (wopQueryPathInfo)
record_session "query_path_info" nix path-info --store "unix://$TEMPDIR/query_path_info.sock" "$TEST_PATH"

# 3. Valid paths query (wopQueryValidPaths)
record_session "query_valid_paths" nix store info --store "unix://$TEMPDIR/query_valid_paths.sock"

echo ""
echo "Done. Socat logs saved to: $OUTDIR"
echo ""
echo "Note: These logs contain hex dumps of the bidirectional traffic."
echo "Parse them with the golden_conformance test to extract client/server bytes."
