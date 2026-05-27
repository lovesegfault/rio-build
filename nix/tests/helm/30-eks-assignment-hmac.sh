# Production-EKS assignment-HMAC wiring (P0560 production enablement).
#
# 29-assignment-hmac-mount.sh locks what the chart renders GIVEN
# assignmentHmac.secretName; this fragment locks that the production
# EKS deploy actually sets it, and that the name it sets is the one the
# chart's ESO ExternalSecret creates. The chart default is keyless
# (fail-closed dev posture, do not change it), so no chart-only render
# would notice the EKS deploy dropping the value — production would
# silently run with unsigned assignment tokens and no store-side
# verification.
#
# The EKS deploy passes every production value via --set from
# `cargo xtask k8s -p eks up --deploy`, so the helm-lint driver stages
# xtask/src/k8s/eks/deploy.rs at .eks-deploy.rs (nix/misc-checks.nix).

deploy_src=.eks-deploy.rs
test -f "$deploy_src" || {
  echo "FAIL: $deploy_src not staged — keep the cp in nix/misc-checks.nix helm-lint in sync with this fragment" >&2
  exit 1
}

# ── (1) deploy.rs sets assignmentHmac.secretName ─────────────────────
# Extract the value it passes. No match = the EKS deploy no longer
# wires the assignment-token key at all.
hmac_set=$(grep -o '"assignmentHmac\.secretName", *"[^"]*"' "$deploy_src" | head -n1 || true)
test -n "$hmac_set" || {
  echo 'FAIL: the xtask EKS deploy no longer sets assignmentHmac.secretName —' >&2
  echo '      production scheduler/store would run keyless (unsigned assignment' >&2
  echo '      tokens, no verification). Restore the .set() in' >&2
  echo '      xtask/src/k8s/eks/deploy.rs (or update this fragment if the call' >&2
  echo '      was reformatted onto multiple lines).' >&2
  exit 1
}
name=$(printf '%s' "$hmac_set" | sed 's/.*, *"\([^"]*\)"/\1/')
test -n "$name" || {
  echo "FAIL: could not extract the secretName value from: $hmac_set" >&2
  exit 1
}

# ── (2) the Secret only exists because ESO syncs it ──────────────────
# deploy.rs must keep externalSecrets enabled, or the mount points at a
# Secret nothing creates and the scheduler/store pods never start.
grep -q '"externalSecrets\.enabled", *"true"' "$deploy_src" || {
  echo "FAIL: the xtask EKS deploy no longer sets externalSecrets.enabled=true —" >&2
  echo "      the $name Secret it points assignmentHmac at is ESO-synced" >&2
  exit 1
}

# ── (3) the chart's ESO object creates exactly that Secret ───────────
# Render what the EKS deploy renders (ESO on + the secretName from
# deploy.rs): the rio-hmac ExternalSecret must land in BOTH consumer
# namespaces, target the same Secret name, and use the hmac.key data
# key the mount path expects.
eks=$TMPDIR/eks-assignment-hmac.yaml
helm template rio . \
  --set externalSecrets.enabled=true \
  --set "assignmentHmac.secretName=$name" \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$eks"

es='select(.kind=="ExternalSecret" and .metadata.name=="rio-hmac")'

ns_list=$(yq -N "$es | .metadata.namespace" "$eks" | sort | paste -sd' ' -)
test "$ns_list" = "rio-store rio-system" || {
  echo "FAIL: rio-hmac ExternalSecret namespaces '$ns_list', want 'rio-store rio-system'" >&2
  exit 1
}

target=$(yq -N "$es | .spec.target.name" "$eks" | sort -u)
test "$target" = "$name" || {
  echo "FAIL: ESO target Secret name '$target' != deploy.rs assignmentHmac.secretName '$name'" >&2
  exit 1
}

key=$(yq -N "$es | .spec.data[0].secretKey" "$eks" | sort -u)
test "$key" = "hmac.key" || {
  echo "FAIL: rio-hmac ExternalSecret data key '$key' != hmac.key (RIO_HMAC_KEY_PATH expects /etc/rio/assignment-hmac/hmac.key)" >&2
  exit 1
}

# ── (4) the scheduler+store Deployments mount THAT Secret ────────────
# Closes the loop deploy.rs --set → chart volume → ESO target. The
# per-Deployment mount/env shape is 29's job; here only the name link.
for dep in rio-scheduler rio-store; do
  vol=$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
        | .spec.template.spec.volumes[]
        | select(.name==\"assignment-hmac\")
        | .secret.secretName" "$eks")
  test "$vol" = "$name" || {
    echo "FAIL: $dep assignment-hmac volume secretName '$vol' != '$name'" >&2
    exit 1
  }
done
