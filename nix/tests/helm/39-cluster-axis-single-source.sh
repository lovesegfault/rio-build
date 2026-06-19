# Cluster-axis single source + controller Recreate (merged_bug_001).
#
# The controller's node-informer mints exposure idempotency uids keyed
# `exposure:{cluster}:{hw}:{window-slot}` against the scheduler's
# `interrupt_samples` table, whose M_047 partial unique index is
# table-GLOBAL in the shared-PG topology (ADR-023 §2.13). Two
# invariants keep that key sound, and both are render-time properties
# of this chart:
#
# 1. SINGLE SOURCE: the controller-TOML `cluster` and the
#    scheduler-TOML `[sla].cluster` MUST render from the one values
#    expression (`scheduler.sla.cluster | default
#    karpenter.clusterName | default ""`). A hand-mirrored literal
#    that drifts splits the axis: the controller keys windows under
#    one cluster while the scheduler's λ refresh filters on another —
#    every exposure row silently invisible to the solver.
# 2. RECREATE: replicas=1 with the default RollingUpdate strategy
#    surge co-runs TWO informers on every rollout. The grid-aligned
#    uid makes that co-run dedup-convergent, but Recreate closes the
#    residual partial-window seam (whichever pod's slice commits first
#    wins a slightly different secs value) and interrupt-watcher
#    duplication. The cost is exactly the "30s reschedule gap during
#    pod restart" the chart already documents as acceptable.
# 3. FAIL-CLOSED IDENTITY (bug_022): value distinctness across
#    deployments cannot be typed in-process — the chart refuses to
#    render an empty cluster id when the external-secrets PG path
#    (the shared-capable topology declaration) is enabled, via the
#    rio.clusterIdentity helper both TOML lines include.

. "$(dirname "$0")/_lib.sh"

out=$TMPDIR/cluster-axis.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard: the controller Deployment renders at chart defaults.
dep=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-controller") | .metadata.name' "$out")
test "$dep" = "rio-controller" || {
  echo "FAIL: rio-controller Deployment did not render at chart defaults — assertions vacuous" >&2
  exit 1
}

# (a) strategy MUST be Recreate.
strategy=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-controller") | .spec.strategy.type' "$out")
test "$strategy" = "Recreate" || {
  echo "FAIL: controller strategy is $strategy (RollingUpdate default) — surge co-runs two informers" >&2
  exit 1
}

# (b) TOML-to-TOML cluster parity under all three value paths. The
# extractor reads the RENDERED TOMLs (the same bytes the binaries
# load), not the values tree — a template typo cannot pass. `|| true`
# inside the pipelines: grep's no-match exit must reach the DEDICATED
# failure message below, not die silently in a `set -e` command
# substitution (the stdenv-pipefail trap). `toml_body` from _lib.sh.
toml_cluster() { # <configmap-name> <toml-key-file> <render-file>
  toml_body "$1" "$2" "$3" \
    | { grep -E '^cluster = ' || true; } | head -1 | sed -E 's/^cluster = "(.*)"$/\1/'
}

assert_pair() { # <render-file> <expected> <label>
  # Absence of the controller key is the dedicated failure (the
  # merged_bug_001 deploy half: the axis never reaches the binary).
  ctrl_line=$(toml_body rio-controller-config controller.toml "$1" | { grep -cE '^cluster = ' || true; })
  test "$ctrl_line" = "1" || {
    echo "FAIL: cluster absent from the rendered rio.controllerToml ($3)" >&2
    exit 1
  }
  ctrl=$(toml_cluster rio-controller-config controller.toml "$1")
  sched=$(toml_cluster rio-scheduler-config scheduler.toml "$1")
  test "$ctrl" = "$2" || {
    echo "FAIL: controller-TOML cluster '$ctrl' != expected '$2' ($3)" >&2
    exit 1
  }
  test "$ctrl" = "$sched" || {
    echo "FAIL: controller-TOML cluster '$ctrl' != scheduler-TOML cluster '$sched' ($3) — the axis MUST single-source" >&2
    exit 1
  }
}

# Explicit scheduler.sla.cluster wins on BOTH sides.
exp=$TMPDIR/cluster-axis-explicit.yaml
helm template rio . --set global.image.tag=test --set scheduler.sla.cluster=prod-east >"$exp"
assert_pair "$exp" "prod-east" "explicit scheduler.sla.cluster"

# karpenter.clusterName fallback feeds BOTH sides.
fb=$TMPDIR/cluster-axis-fallback.yaml
helm template rio . --set global.image.tag=test --set karpenter.clusterName=rio-eks-ci >"$fb"
assert_pair "$fb" "rio-eks-ci" "karpenter.clusterName fallback"

# Default-empty: both render "" (the single-cluster default, matching
# the scheduler column's DEFAULT ''). SCOPE (bug_022): this leg
# certifies single-cluster RENDER behavior only — defaults stay
# installable — never cross-deployment safety; the shared-PG topology
# is gated by the external-secrets legs below.
assert_pair "$out" "" "default-empty"

# bug_022 (ctrl.informer.cluster-identity-boundary, render half): the
# external-secrets PG path is the chart's ONLY render-visible
# declaration of a shared-capable PG. With it enabled and NO cluster
# id, the rio.clusterIdentity helper MUST fail the render — two
# deployments sharing one PG with cluster="" mint identical exposure
# uids and the M_047 dedup silently absorbs one deployment's λ
# evidence.
es_args="--set externalSecrets.enabled=true --set externalSecrets.auroraSecretArn=arn:x --set externalSecrets.auroraEndpoint=db.example"

# (e) PLANTED RED: a gate that cannot fail its planted fixture does
# not gate (the fragment-35 self-test pattern). MUST exit nonzero AND
# name the refusal.
gate_err=$TMPDIR/cluster-axis-gate.err
# shellcheck disable=SC2086
if helm template rio . --set global.image.tag=test $es_args >/dev/null 2>"$gate_err"; then
  echo "FAIL: external-secrets PG path rendered with an empty cluster id — the gate is fail-open" >&2
  exit 1
fi
grep -q "cluster identity required" "$gate_err" || {
  echo "FAIL: gate refused but without the 'cluster identity required' message:" >&2
  cat "$gate_err" >&2
  exit 1
}

# (f) explicit scheduler.sla.cluster satisfies the gate; parity holds.
esx=$TMPDIR/cluster-axis-es-explicit.yaml
# shellcheck disable=SC2086
helm template rio . --set global.image.tag=test $es_args --set scheduler.sla.cluster=prod-east >"$esx"
assert_pair "$esx" "prod-east" "external-secrets + explicit cluster"

# (g) karpenter.clusterName fallback satisfies the gate; parity holds.
esk=$TMPDIR/cluster-axis-es-karpenter.yaml
# shellcheck disable=SC2086
helm template rio . --set global.image.tag=test $es_args --set karpenter.clusterName=rio-eks-ci >"$esk"
assert_pair "$esk" "rio-eks-ci" "external-secrets + karpenter fallback"

# (i) merged_bug_067: the gate predicate and the emission are
# NORMALIZED at the rio.clusterIdentity mint — driven over the
# cross-boundary golden fixture ($clusterIdentityFixture: the same
# bytes the Rust constructor golden test consumes, so the two
# languages' trim ∘ classify predicates cannot drift). For every
# helm-settable case: single_cluster_default=true ⟹ the
# external-secrets render REFUSES (the refusal set is "every
# helm-settable value the runtime classifies single-cluster-default",
# not the empty-string point — the round-6 weak-witness kill);
# =false ⟹ the render carries the NORMALIZED value in BOTH TOMLs
# (the trim-collision axis closed at the mint: uid axis and λ-filter
# axis provably consume one alphabet).
test -n "${clusterIdentityFixture:-}" || {
  echo "FAIL: clusterIdentityFixture env input missing — leg (i) cannot run" >&2
  exit 1
}
norm_out=$TMPDIR/cluster-axis-norm.yaml
norm_err=$TMPDIR/cluster-axis-norm.err
while IFS= read -r case_json; do
  raw=$(jq -r '.raw' <<<"$case_json")
  normalized=$(jq -r '.normalized' <<<"$case_json")
  is_default=$(jq -r '.single_cluster_default' <<<"$case_json")
  if [ "$is_default" = "true" ]; then
    # shellcheck disable=SC2086
    if helm template rio . --set global.image.tag=test $es_args \
      --set-string "scheduler.sla.cluster=$raw" >/dev/null 2>"$norm_err"; then
      echo "FAIL: external-secrets render accepted raw '$raw' — the runtime classifies it single-cluster-default; the gate predicate is raw" >&2
      exit 1
    fi
    grep -q "cluster identity required" "$norm_err" || {
      echo "FAIL: gate refused raw '$raw' but without the 'cluster identity required' message:" >&2
      cat "$norm_err" >&2
      exit 1
    }
  else
    # shellcheck disable=SC2086
    helm template rio . --set global.image.tag=test $es_args \
      --set-string "scheduler.sla.cluster=$raw" >"$norm_out"
    assert_pair "$norm_out" "$normalized" "normalization: raw '$raw'"
  fi
done < <(jq -c '.cases[] | select(.helm_settable)' "$clusterIdentityFixture")

echo "OK: controller strategy Recreate; cluster single-sourced (explicit/fallback/default) TOML-to-TOML; external-secrets empty-id render gate fires (planted red) and passes with either id source; normalization law pinned to the golden fixture (leg i)"
