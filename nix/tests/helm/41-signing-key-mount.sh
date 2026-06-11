# narSign mount family: the I-217 visibility-fallback enabler
# (round-9 dossier A2, premise Q2 resolved 2026-06-11).
#
# The ESO ExternalSecret rio-signing-key has synced the narinfo
# signing seed (AWS SM rio/signing-key, minted by the bootstrap Job)
# into the store namespace since 2026-04-13 — but nothing ever
# MOUNTED the synced Secret: no rio.mounts family, RIO_SIGNING_KEY_PATH
# rendered nowhere, so every deployment ran signer-less and the
# evidence kernel's signature fallback row
# ((owned=false, any_built=false, sig_trusted=true) -> Visible) was
# structurally unreachable. The cold-solve forensics measured the
# cost: freshly-built outputs Hidden from their own tenant in the
# cancel race, ~150K-node re-execution of work whose bytes sat in the
# store.
#
# This fragment pins the family wiring:
#   - externalSecrets.enabled=true => the STORE Deployment (and only
#     the store: narinfo signing is a store concern,
#     rio-store/src/main.rs Signer::load) carries
#     RIO_SIGNING_KEY_PATH=/etc/rio/signing-key/key, a readOnly
#     volumeMount, and the Secret volume (rio-signing-key,
#     defaultMode 0440 = 288: private-key hygiene; group-readable for
#     fsGroup 65532).
#   - chart defaults (externalSecrets off) => nothing renders, which
#     is what keeps the env-parity allowlist's RIO_SIGNING_KEY_PATH
#     "conditional wiring" row honest.
#
# The keypath<->secretKey basename parity (env file `key` vs the
# ExternalSecret's `secretKey: key`) is fragment 37's join — this
# fragment's externalSecrets render feeds it a live signing-key pair.

out=$TMPDIR/signing-on.yaml
helm template rio . \
  --set global.image.tag=test \
  --set externalSecrets.enabled=true \
  --set externalSecrets.auroraSecretArn=arn:aws:secretsmanager:stub \
  --set externalSecrets.auroraEndpoint=db.stub.local \
  --set scheduler.sla.cluster=signing-mount-stub \
  >"$out"

# Store env: RIO_SIGNING_KEY_PATH points at the mounted data key.
v=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")
    | .spec.template.spec.containers[0].env[]
    | select(.name=="RIO_SIGNING_KEY_PATH") | .value' "$out")
test "$v" = "/etc/rio/signing-key/key" || {
  echo "FAIL: store RIO_SIGNING_KEY_PATH = '$v', expected /etc/rio/signing-key/key" >&2
  exit 1
}

# volumeMount: readOnly at the env's dirname.
mnt=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")
    | .spec.template.spec.containers[0].volumeMounts[]
    | select(.name=="signing-key") | .mountPath + "," + (.readOnly|tostring)' "$out")
test "$mnt" = "/etc/rio/signing-key,true" || {
  echo "FAIL: store signing-key volumeMount = '$mnt', expected /etc/rio/signing-key,true" >&2
  exit 1
}

# volume: the ESO-synced Secret, 0440 (288 decimal).
vol=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")
    | .spec.template.spec.volumes[]
    | select(.name=="signing-key") | .secret.secretName + "," + (.secret.defaultMode|tostring)' "$out")
test "$vol" = "rio-signing-key,288" || {
  echo "FAIL: store signing-key volume = '$vol', expected rio-signing-key,288 (0440)" >&2
  exit 1
}

# Store-only: exactly one Deployment carries the env (the signing key
# is the store's narinfo signer; nothing else may read the seed).
n=$(grep -c 'RIO_SIGNING_KEY_PATH' "$out")
test "$n" -eq 1 || {
  echo "FAIL: RIO_SIGNING_KEY_PATH rendered $n times, expected exactly 1 (store-only)" >&2
  exit 1
}

# Negative: chart defaults render no trace of the family — keeps the
# env-parity allowlist row (conditional wiring) honest.
off=$TMPDIR/signing-off.yaml
helm template rio . --set global.image.tag=test >"$off"
! grep -q 'RIO_SIGNING_KEY_PATH\|signing-key' "$off" || {
  echo "FAIL: signing-key family rendered at chart defaults (externalSecrets disabled)" >&2
  grep -n 'RIO_SIGNING_KEY_PATH\|signing-key' "$off" | head >&2
  exit 1
}

echo "OK"
