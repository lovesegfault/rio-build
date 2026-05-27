# Mountd Mount-admission HMAC wiring (ADR-022 §P0559) — chart side only.
#
# mountdHmac.secretName set MUST render the Secret volume + mount +
# RIO_MOUNTD_HMAC_KEY_PATH env on the scheduler Deployment (it mints
# WorkAssignment.mountd_token) AND the rio-mountd DaemonSet (it
# verifies Mount{} tokens) — and on nothing else: the store never sees
# this token family, and leaking the key into extra components widens
# the blast radius of the one key that lives on every builder node.
# Unset (the default) MUST render none of it (helm/25 also pins the
# DaemonSet half of that posture).
#
# The symmetric scheme is SUPERSEDED by the Ed25519 mountdSigning
# family (ADR-022 mount-admission credentials, §P0590) and was never
# provisioned in any cluster: the production EKS deploy no longer sets
# mountdHmac.secretName (helm/33 asserts that, plus the mountdSigning
# wiring that replaced it). Until the §P0590 Phase-5 sweep deletes the
# family, this fragment keeps the chart-side render contract locked so
# the residual code path cannot silently rot or leak into other
# components.

# ── (1) chart render with the family enabled ─────────────────────────
on=$TMPDIR/mountd-hmac-on.yaml
helm template rio . \
  --set mountdHmac.secretName=rio-mountd-hmac \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$on"

# 2 = scheduler Deployment + mountd DaemonSet, exactly one
# RIO_MOUNTD_HMAC_KEY_PATH each. >2 would mean the family leaked into a
# component that has no business holding the node-resident key; <2 = a
# missing include (the scheduler silently mints nothing, or mountd
# silently never enters token mode).
n=$(grep -c RIO_MOUNTD_HMAC_KEY_PATH "$on")
test "$n" -eq 2 || {
  echo "FAIL: expected 2 RIO_MOUNTD_HMAC_KEY_PATH (scheduler+mountd), got $n" >&2
  exit 1
}

# Structural asserts against both consumers' pod specs (volume → mount
# → env), not just the strings appearing somewhere in the render.
check_pod() { # kind name
  local kind=$1 name=$2
  yq "select(.kind==\"$kind\" and .metadata.name==\"$name\")
      | .spec.template.spec.volumes[]
      | select(.name==\"mountd-hmac\")
      | .secret.secretName" "$on" |
    grep -x rio-mountd-hmac >/dev/null || {
    echo "FAIL: $name missing mountd-hmac Secret volume (rio-mountd-hmac)" >&2
    exit 1
  }
  yq "select(.kind==\"$kind\" and .metadata.name==\"$name\")
      | .spec.template.spec.containers[0].volumeMounts[]
      | select(.name==\"mountd-hmac\")
      | .mountPath" "$on" |
    grep -x /etc/rio/mountd-hmac >/dev/null || {
    echo "FAIL: $name missing mountd-hmac volumeMount at /etc/rio/mountd-hmac" >&2
    exit 1
  }
  yq "select(.kind==\"$kind\" and .metadata.name==\"$name\")
      | .spec.template.spec.containers[0].env[]
      | select(.name==\"RIO_MOUNTD_HMAC_KEY_PATH\")
      | .value" "$on" |
    grep -x /etc/rio/mountd-hmac/mountd-hmac.key >/dev/null || {
    echo "FAIL: $name RIO_MOUNTD_HMAC_KEY_PATH != /etc/rio/mountd-hmac/mountd-hmac.key" >&2
    exit 1
  }
}
check_pod Deployment rio-scheduler
check_pod DaemonSet rio-mountd

# The store must NOT carry the mountd key (separate token family).
store_yaml=$(yq 'select(.kind=="Deployment" and .metadata.name=="rio-store")' "$on")
if grep -q 'mountd-hmac\|RIO_MOUNTD_HMAC_KEY_PATH' <<<"$store_yaml"; then
  echo "FAIL: rio-store carries the mountd-hmac key — the store never verifies mountd tokens" >&2
  exit 1
fi

# ── (2) negative: default render carries none of it ──────────────────
off=$TMPDIR/mountd-hmac-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$off"
! grep -q 'RIO_MOUNTD_HMAC_KEY_PATH\|mountd-hmac' "$off" || {
  echo "FAIL: mountd-hmac rendered with mountdHmac.secretName unset (default)" >&2
  grep -n 'RIO_MOUNTD_HMAC_KEY_PATH\|mountd-hmac' "$off" >&2
  exit 1
}

# ── (3) the chart's ESO object stays consistent with the mounts ──────
# Render with ESO on + the chart's own canonical Secret name (the EKS
# deploy no longer sets this family — helm/33 owns the deploy-side
# assertions): the rio-mountd-hmac ExternalSecret must land in BOTH
# consumer namespaces (rio-system for the scheduler, rio-builders for
# the DaemonSet — and NOT rio-store), target that same Secret name, and
# use the mountd-hmac.key data key the mount path expects.
name=rio-mountd-hmac
eks=$TMPDIR/eks-mountd-hmac.yaml
helm template rio . \
  --set externalSecrets.enabled=true \
  --set "mountdHmac.secretName=$name" \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$eks"

es='select(.kind=="ExternalSecret" and .metadata.name=="rio-mountd-hmac")'

ns_list=$(yq -N "$es | .metadata.namespace" "$eks" | sort | paste -sd' ' -)
test "$ns_list" = "rio-builders rio-system" || {
  echo "FAIL: rio-mountd-hmac ExternalSecret namespaces '$ns_list', want 'rio-builders rio-system'" >&2
  exit 1
}

target=$(yq -N "$es | .spec.target.name" "$eks" | sort -u)
test "$target" = "$name" || {
  echo "FAIL: ESO target Secret name '$target' != the chart's canonical mountdHmac Secret name '$name'" >&2
  exit 1
}

key=$(yq -N "$es | .spec.data[0].secretKey" "$eks" | sort -u)
test "$key" = "mountd-hmac.key" || {
  echo "FAIL: rio-mountd-hmac ExternalSecret data key '$key' != mountd-hmac.key (RIO_MOUNTD_HMAC_KEY_PATH expects /etc/rio/mountd-hmac/mountd-hmac.key)" >&2
  exit 1
}

remote=$(yq -N "$es | .spec.data[0].remoteRef.key" "$eks" | sort -u)
test "$remote" = "rio/mountd-hmac" || {
  echo "FAIL: rio-mountd-hmac ExternalSecret remoteRef '$remote' != rio/mountd-hmac (the bootstrap Job creates that Secrets Manager entry)" >&2
  exit 1
}

# ── (4) those Deployments/DaemonSet mount THAT Secret ────────────────
for spec in "Deployment rio-scheduler" "DaemonSet rio-mountd"; do
  set -- $spec
  vol=$(yq -N "select(.kind==\"$1\" and .metadata.name==\"$2\")
        | .spec.template.spec.volumes[]
        | select(.name==\"mountd-hmac\")
        | .secret.secretName" "$eks")
  test "$vol" = "$name" || {
    echo "FAIL: $2 mountd-hmac volume secretName '$vol' != '$name'" >&2
    exit 1
  }
done

echo "OK"
