# JWT mount assertions (r[sec.jwt.pubkey-mount]).
#
# jwt.enabled=true MUST render the ConfigMap mount in scheduler+store
# and the Secret mount in gateway. Without the mount, RIO_JWT__KEY_PATH
# stays unset → the interceptor is inert → silent fail-open (every JWT
# passes unverified). The ConfigMap/Secret OBJECTS exist; the MOUNT was
# missing. --set-string for the base64 values — trailing '=' padding is
# fine (everything after the first '=' is the value); --set would try
# to parse it as YAML.

on=$TMPDIR/jwt-on.yaml
helm template rio . \
  --set jwt.enabled=true \
  --set-string jwt.publicKey=dGVzdA== \
  --set-string jwt.signingSeed=dGVzdA== \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$on"

# 3 = scheduler + store + gateway. Each gets exactly one
# RIO_JWT__KEY_PATH env var. >3 would mean a template got included
# twice (leaky nindent loop); <3 = missing include.
n=$(grep -c RIO_JWT__KEY_PATH "$on")
test "$n" -eq 3 || {
  echo "FAIL: expected 3 RIO_JWT__KEY_PATH (sched+store+gw), got $n" >&2
  exit 1
}

# yq: structural asserts. grep would match the ConfigMap resource's own
# `name: rio-jwt-pubkey` — yq drills into the Deployment's pod spec so
# we're asserting the MOUNT, not the object existing.
for dep in rio-scheduler rio-store; do
  # volumes: entry — configMap ref to rio-jwt-pubkey.
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
      | .spec.template.spec.volumes[]
      | select(.name==\"jwt-pubkey\")
      | .configMap.name" "$on" |
    grep -x rio-jwt-pubkey >/dev/null || {
    echo "FAIL: $dep missing jwt-pubkey configMap volume" >&2
    exit 1
  }
  # volumeMounts: entry — path + readOnly.
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
      | .spec.template.spec.containers[0].volumeMounts[]
      | select(.name==\"jwt-pubkey\")
      | .mountPath" "$on" |
    grep -x /etc/rio/jwt >/dev/null || {
    echo "FAIL: $dep missing jwt-pubkey volumeMount at /etc/rio/jwt" >&2
    exit 1
  }
  # env: RIO_JWT__KEY_PATH points at the file the Rust side reads.
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
      | .spec.template.spec.containers[0].env[]
      | select(.name==\"RIO_JWT__KEY_PATH\")
      | .value" "$on" |
    grep -x /etc/rio/jwt/ed25519_pubkey >/dev/null || {
    echo "FAIL: $dep RIO_JWT__KEY_PATH != /etc/rio/jwt/ed25519_pubkey" >&2
    exit 1
  }
done

# Gateway: Secret mount (signing side).
yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
    | .spec.template.spec.volumes[]
    | select(.name=="jwt-signing")
    | .secret.secretName' "$on" |
  grep -x rio-jwt-signing >/dev/null || {
  echo "FAIL: gateway missing jwt-signing Secret volume" >&2
  exit 1
}
yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
    | .spec.template.spec.containers[0].env[]
    | select(.name=="RIO_JWT__KEY_PATH")
    | .value' "$on" |
  grep -x /etc/rio/jwt/ed25519_seed >/dev/null || {
  echo "FAIL: gateway RIO_JWT__KEY_PATH != /etc/rio/jwt/ed25519_seed" >&2
  exit 1
}

# Negative: jwt.enabled=false renders NO mount. The or-gate in
# scheduler/store/gateway elides the volumeMounts/volumes keys when
# nothing is enabled, and the self-guarded templates render nothing.
# "! grep" exits 0 on no-match, 1 on match → && flips to fail-fast.
off=$TMPDIR/jwt-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set tls.enabled=false \
  --set postgresql.enabled=false \
  >"$off"
! grep -q 'RIO_JWT__KEY_PATH\|jwt-pubkey\|jwt-signing' "$off" || {
  echo "FAIL: jwt mount rendered with jwt.enabled=false (default)" >&2
  grep -n 'RIO_JWT__KEY_PATH\|jwt-pubkey\|jwt-signing' "$off" >&2
  exit 1
}
