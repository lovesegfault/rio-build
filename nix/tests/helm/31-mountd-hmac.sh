# Mountd Mount-admission HMAC wiring (ADR-022 §P0559).
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
# Like helm/30 for the assignment key, this fragment also locks the
# production-EKS deploy: the chart default is keyless, so only
# xtask/src/k8s/eks/deploy.rs setting mountdHmac.secretName makes
# hostUsers:false executor pods admissible at all — and the name it
# sets must be the Secret the chart's ESO ExternalSecret actually
# creates, in exactly the two consumer namespaces.

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

# ── (3) the EKS deploy sets it, and to the ESO-created name ──────────
deploy_src=.eks-deploy.rs
test -f "$deploy_src" || {
  echo "FAIL: $deploy_src not staged — keep the cp in nix/misc-checks.nix helm-lint in sync with this fragment" >&2
  exit 1
}
mountd_set=$(grep -o '"mountdHmac\.secretName", *"[^"]*"' "$deploy_src" | head -n1 || true)
test -n "$mountd_set" || {
  echo 'FAIL: the xtask EKS deploy no longer sets mountdHmac.secretName —' >&2
  echo '      production mountd would stay gid-only and hostUsers:false executor' >&2
  echo '      pods could never Mount. Restore the .set() in' >&2
  echo '      xtask/src/k8s/eks/deploy.rs (or update this fragment if the call' >&2
  echo '      was reformatted onto multiple lines).' >&2
  exit 1
}
name=$(printf '%s' "$mountd_set" | sed 's/.*, *"\([^"]*\)"/\1/')
test -n "$name" || {
  echo "FAIL: could not extract the secretName value from: $mountd_set" >&2
  exit 1
}

# ── (4) the chart's ESO object creates exactly that Secret ───────────
# Render what the EKS deploy renders (ESO on + the secretName from
# deploy.rs): the rio-mountd-hmac ExternalSecret must land in BOTH
# consumer namespaces (rio-system for the scheduler, rio-builders for
# the DaemonSet — and NOT rio-store), target the same Secret name, and
# use the mountd-hmac.key data key the mount path expects.
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
  echo "FAIL: ESO target Secret name '$target' != deploy.rs mountdHmac.secretName '$name'" >&2
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

# ── (5) those Deployments/DaemonSet mount THAT Secret ────────────────
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
