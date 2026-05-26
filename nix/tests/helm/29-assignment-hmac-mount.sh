# Assignment-token HMAC mount assertions.
#
# assignmentHmac.secretName set MUST render the Secret volume + mount +
# RIO_HMAC_KEY_PATH env on scheduler AND store — the scheduler signs
# WorkAssignment tokens, the store verifies them on PutPath and on
# castore DirectoryService/BlobService reads, where the token is the
# builder's only tenant credential (no anonymous fallback). A deployment
# missing either half fails every builder castore read UNAUTHENTICATED
# ("DirectoryService requires a tenant"). Unset (the default) MUST
# render none of it — dev mode stays keyless.

on=$TMPDIR/assignment-hmac-on.yaml
helm template rio . \
  --set assignmentHmac.secretName=rio-hmac \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$on"

# 2 = scheduler + store, exactly one RIO_HMAC_KEY_PATH each. >2 would
# mean the family leaked into a component whose config has no
# hmac_key_path (gateway/controller); <2 = missing include.
n=$(grep -c RIO_HMAC_KEY_PATH "$on")
test "$n" -eq 2 || {
  echo "FAIL: expected 2 RIO_HMAC_KEY_PATH (sched+store), got $n" >&2
  exit 1
}

# yq: structural asserts against the Deployments' pod specs (volume →
# mount → env), not just the strings appearing somewhere in the render.
for dep in rio-scheduler rio-store; do
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
      | .spec.template.spec.volumes[]
      | select(.name==\"assignment-hmac\")
      | .secret.secretName" "$on" |
    grep -x rio-hmac >/dev/null || {
    echo "FAIL: $dep missing assignment-hmac Secret volume (rio-hmac)" >&2
    exit 1
  }
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
      | .spec.template.spec.containers[0].volumeMounts[]
      | select(.name==\"assignment-hmac\")
      | .mountPath" "$on" |
    grep -x /etc/rio/assignment-hmac >/dev/null || {
    echo "FAIL: $dep missing assignment-hmac volumeMount at /etc/rio/assignment-hmac" >&2
    exit 1
  }
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
      | .spec.template.spec.containers[0].env[]
      | select(.name==\"RIO_HMAC_KEY_PATH\")
      | .value" "$on" |
    grep -x /etc/rio/assignment-hmac/hmac.key >/dev/null || {
    echo "FAIL: $dep RIO_HMAC_KEY_PATH != /etc/rio/assignment-hmac/hmac.key" >&2
    exit 1
  }
done

# The vmtest profile carries the pair: the k3s fixture creates the
# rio-hmac Secret in both namespaces and every k3s build scenario
# exercises builder castore reads through it.
vmtest=$TMPDIR/assignment-hmac-vmtest.yaml
helm template rio . -f values/vmtest-full.yaml >"$vmtest"
n=$(grep -c RIO_HMAC_KEY_PATH "$vmtest")
test "$n" -eq 2 || {
  echo "FAIL: vmtest-full.yaml: expected 2 RIO_HMAC_KEY_PATH (sched+store), got $n" >&2
  exit 1
}

# Negative: default (secretName unset) renders NO assignment-hmac
# volume/mount/env — dev mode stays keyless. The serviceHmac family
# (service-hmac / RIO_SERVICE_HMAC_KEY_PATH) is always-on and does not
# match either pattern below.
off=$TMPDIR/assignment-hmac-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$off"
! grep -q 'RIO_HMAC_KEY_PATH\|assignment-hmac' "$off" || {
  echo "FAIL: assignment-hmac rendered with assignmentHmac.secretName unset (default)" >&2
  grep -n 'RIO_HMAC_KEY_PATH\|assignment-hmac' "$off" >&2
  exit 1
}
