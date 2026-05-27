# P0563 castore-FUSE + chunk-cache-tier observability rendering.
#
# Two new dashboards (dashboards/{castore-fuse,chunk-cache-tier}.json)
# ship through the dashboards-configmap.yaml `.Files.Glob` loop, and a
# new PrometheusRule group (rio.castore.rules) carries the castore
# integrity/EIO/identity pages plus the mountd/prefetch/eager-index
# warns. The shapes that must hold:
#   - monitoring.enabled=true renders one ConfigMap per dashboard, with
#     the grafana_dashboard sidecar label, and the embedded JSON is
#     still parseable after the nindent round-trip (a stray template
#     character or indentation slip turns the dashboard into a string
#     Grafana silently drops).
#   - the rio.castore.rules group renders with exactly the expected
#     alert set, and the severity split (content-integrity pages are
#     critical, degradation signals are warning) survives edits.
#   - nothing castore-obs-related leaks into a render with monitoring
#     disabled (the gate is the template-level `if`, same as every
#     other monitoring object).

mon=$TMPDIR/castore-obs.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true >"$mon"

# ── dashboard ConfigMaps render and embed valid JSON ──────────────────
for d in castore-fuse chunk-cache-tier; do
  cm="select(.kind==\"ConfigMap\" and .metadata.name==\"rio-dashboard-$d\")"

  test "$(yq "$cm | .metadata.labels.grafana_dashboard" "$mon")" = "1" || {
    echo "FAIL: rio-dashboard-$d ConfigMap missing or lacks grafana_dashboard=\"1\" label" >&2
    exit 1
  }

  # Extract the embedded dashboard and re-parse it. Capture-then-pipe
  # into jq via a file (not yq | jq) so a jq parse abort can't SIGPIPE
  # the producer under pipefail.
  payload=$TMPDIR/dash-$d.json
  yq "$cm | .data[\"$d.json\"]" "$mon" >"$payload"
  jq -e . "$payload" >/dev/null || {
    echo "FAIL: rio-dashboard-$d data is not valid JSON after the configmap nindent round-trip" >&2
    exit 1
  }
  test "$(jq -r '.uid' "$payload")" = "rio-$d" || {
    echo "FAIL: dashboards/$d.json uid is not rio-$d" >&2
    exit 1
  }
  test "$(jq '.panels | length' "$payload")" -ge 8 || {
    echo "FAIL: rio-dashboard-$d has <8 panels — content went missing in the render" >&2
    exit 1
  }
done

# ── castore alert group: exact alert set ──────────────────────────────
got_alerts=$(yq -N \
  '.spec.groups[] | select(.name=="rio.castore.rules") | .rules[].alert' "$mon" | sort)
want_alerts='RioBuilderCastoreEio
RioBuilderCastoreIntegrityFail
RioBuilderDagPrefetchSlow
RioMountdPromoteRejectMismatch
RioStoreCastoreScopeDenied
RioStoreCastoreScopeUnresolvable
RioStoreFileDigestMismatch
RioStoreNarIndexEagerErrors'
test "$got_alerts" = "$want_alerts" || {
  echo "FAIL: rio.castore.rules alert set changed." >&2
  echo "  got:  $(echo "$got_alerts" | tr '\n' ' ')" >&2
  echo "  want: $(echo "$want_alerts" | tr '\n' ' ')" >&2
  echo "Update this fragment AND 10-dashboard-labels.sh / docs/gen/alerts.json together." >&2
  exit 1
}

# ── severity split: identity/integrity pages stay critical ───────────
sev_of() {
  yq -N ".spec.groups[] | select(.name==\"rio.castore.rules\")
         | .rules[] | select(.alert==\"$1\") | .labels.severity" "$mon"
}
for a in RioBuilderCastoreIntegrityFail RioBuilderCastoreEio RioStoreFileDigestMismatch; do
  test "$(sev_of "$a")" = "critical" || {
    echo "FAIL: $a must page (severity: critical) — content integrity / build-failing infra" >&2
    exit 1
  }
done
for a in RioMountdPromoteRejectMismatch RioBuilderDagPrefetchSlow RioStoreNarIndexEagerErrors \
  RioStoreCastoreScopeDenied RioStoreCastoreScopeUnresolvable; do
  test "$(sev_of "$a")" = "warning" || {
    echo "FAIL: $a must be severity: warning (degradation, not a page)" >&2
    exit 1
  }
done

# The mismatch warn must key on reason="mismatch" — dropping the
# selector silently widens it to protocol-misuse rejects (not-regular,
# too-large, race-timeout) and turns routine contention into pages on
# the next severity bump.
prm_block=$(grep -A4 'alert: RioMountdPromoteRejectMismatch' "$mon" || true)
grep -q 'reason="mismatch"' <<<"$prm_block" || {
  echo "FAIL: RioMountdPromoteRejectMismatch expr does not select reason=\"mismatch\"" >&2
  exit 1
}

# Same shape for the eager-index warn: outcome="error" only —
# outcome="skipped" is a routine concurrency-cap deferral.
nie_block=$(grep -A4 'alert: RioStoreNarIndexEagerErrors' "$mon" || true)
grep -q 'outcome="error"' <<<"$nie_block" || {
  echo "FAIL: RioStoreNarIndexEagerErrors expr does not select outcome=\"error\"" >&2
  exit 1
}

# P0591 closure-scope alerts. The deny alert must cover BOTH counters:
# denied (enforce, the default) AND would_deny (the log rollback mode)
# — dropping the would_deny arm makes the rollback mode blind, and the
# would-deny backlog is the gate for flipping back to enforce.
csd_block=$(grep -A8 'alert: RioStoreCastoreScopeDenied' "$mon" || true)
grep -q 'rio_store_castore_scope_denied_total' <<<"$csd_block" || {
  echo "FAIL: RioStoreCastoreScopeDenied expr does not watch rio_store_castore_scope_denied_total" >&2
  exit 1
}
grep -q 'rio_store_castore_scope_would_deny_total' <<<"$csd_block" || {
  echo "FAIL: RioStoreCastoreScopeDenied expr no longer covers rio_store_castore_scope_would_deny_total — the log rollback mode would have no deny signal" >&2
  exit 1
}

# The unresolvable warn must key on resolution="denied" — the served
# (log-mode) and derived (fallback-hit) resolutions are healthy paths,
# and widening to them turns routine new-replica churn into alerts.
csu_block=$(grep -A4 'alert: RioStoreCastoreScopeUnresolvable' "$mon" || true)
grep -q 'resolution="denied"' <<<"$csu_block" || {
  echo "FAIL: RioStoreCastoreScopeUnresolvable expr does not select resolution=\"denied\"" >&2
  exit 1
}

# ── monitoring disabled: nothing renders ──────────────────────────────
off=$TMPDIR/castore-obs-off.yaml
helm template rio . --set global.image.tag=test >"$off"
for needle in rio-dashboard-castore-fuse rio-dashboard-chunk-cache-tier rio.castore.rules; do
  if grep -q "$needle" "$off"; then
    echo "FAIL: $needle renders with monitoring.enabled=false" >&2
    exit 1
  fi
done

echo "OK"
