# RIO_*_KEY_PATH ↔ ExternalSecret data-key parity (bug_205).
#
# A Secret mounted WHOLE exposes its data keys as the mounted
# filenames — so every `RIO_*_KEY_PATH=<dir>/<basename>` env must name
# a basename that its backing ExternalSecret actually syncs. Pre-fix
# the chart said `RIO_HMAC_KEY_PATH=/etc/rio/assign-hmac/hmac.key`
# while the rio-hmac ExternalSecret synced `secretKey: key` — the
# mounted file was `/etc/rio/assign-hmac/key`, `hmac.key` did not
# exist, and rio_auth::hmac::load_key treats ENOENT as fatal: every
# jwt-enabled EKS deployment crashlooped BOTH the scheduler and the
# store at boot. The VM fixture (k3s-full.nix) writes data key
# `hmac.key`, masking the mismatch; the sibling rio-service-hmac entry
# even documents the rule being violated ("secretKey is the k8s Secret
# data key → the mounted filename").
#
# The parity is asserted over the RENDER as a structural join:
#   env RIO_*KEY_PATH → container volumeMount(mountPath == dirname)
#     → pod volume(secret) → ExternalSecret(target.name == secretName)
#     → basename ∈ data[].secretKey
# Secrets without an ExternalSecret (bootstrap-Job- or fixture-created)
# are out of scope here; a zero-pair join is a FAILURE (vacuity guard).
#
# Self-test (no fail-open enforcement): the checker is first proven
# RED against a private chart copy with the bug_205 mismatch
# re-planted, then run for real.
#
# (documentary — .sh is not tracey-scanned.)

root=$PWD
T=$TMPDIR/keypath-parity
rm -rf "$T"; mkdir -p "$T"

render() {
  # jwt on (mounts the assign-hmac family), external-secrets on (the
  # ESO entries under test), tofu-fed values stubbed.
  helm template rio "$1" \
    --set global.image.tag=test \
    --set jwt.enabled=true \
    --set externalSecrets.enabled=true \
    --set externalSecrets.auroraSecretArn=arn:aws:secretsmanager:stub \
    --set externalSecrets.auroraEndpoint=db.stub.local
}

# The join, as a function: prints one line per violation; prints
# PAIRS=<n> on stderr for the vacuity guard.
parity_violations() {
  yq -N -o=json '.' "$1" | jq -rs '
    ([ .[] | select(.kind=="ExternalSecret")
         | {(.spec.target.name): [.spec.data[].secretKey]} ] | add // {}) as $es
    | [ .[] | select(.kind=="Deployment" or .kind=="StatefulSet"
                     or .kind=="DaemonSet" or .kind=="Job")
        | .metadata.name as $w
        | .spec.template.spec as $p
        | ($p.volumes // []) as $vols
        | (($p.containers // []) + ($p.initContainers // []))[]
        | .name as $c
        | (.volumeMounts // []) as $vm
        | (.env // [])[]
        | select((.name // "") | test("^RIO_.*KEY_PATH"))
        | select(.value != null)
        | (.value | split("/")) as $parts
        | ($parts[:-1] | join("/")) as $dir
        | $parts[-1] as $base
        | ($vm[] | select(.mountPath == $dir)) as $mount
        | ($vols[] | select(.name == $mount.name)) as $vol
        | select($vol.secret != null)
        | $vol.secret.secretName as $secret
        | select($es[$secret] != null)
        # NB: an `empty` inside an object constructor erases the WHOLE
        # object — pair counting and violation filtering must be two
        # separate steps or matched pairs vanish from the census.
        | {ok: (($es[$secret] | index($base)) != null),
           msg: "\($w)/\($c): \(.name)=\(.value) expects file \($base) but ExternalSecret(\($secret)) syncs \($es[$secret])"}
      ]
    | ("PAIRS=\(length)" | stderr) as $dbg
    | [ .[] | select(.ok | not) | .msg ] | .[]
  ' 2>"$T/pairs"
}

# ── Planted RED: re-introduce the bug_205 mismatch in a private copy ──
priv=$TMPDIR/chart-205-red
rm -rf "$priv"
cp -r . "$priv"
sed -i 's/^\(\s*\)- secretKey: hmac.key$/\1- secretKey: key/' "$priv/templates/external-secrets.yaml"
grep -qE '^\s*- secretKey: key$' "$priv/templates/external-secrets.yaml" || {
  echo "FAIL: planted-red sed did not take — ExternalSecret shape changed; update this fixture" >&2
  exit 1
}
render "$priv" > "$T/red.yaml"
red=$(parity_violations "$T/red.yaml")
test -n "$red" || {
  echo "FAIL: planted KEY_PATH↔secretKey mismatch was NOT caught — the parity join is fail-open" >&2
  exit 1
}

# ── Real chart ──
render "$root" > "$T/real.yaml"
bad=$(parity_violations "$T/real.yaml")
pairs=$(grep -oE 'PAIRS=[0-9]+' "$T/pairs" | cut -d= -f2)
test -n "$pairs" -a "${pairs:-0}" -gt 0 || {
  echo "FAIL: parity join matched ZERO env↔ExternalSecret pairs — the gate is vacuous" >&2
  echo "      (jwt/externalSecrets gating or the join shape drifted; fix the fragment)" >&2
  exit 1
}
test -z "$bad" || {
  echo "FAIL: RIO_*_KEY_PATH envs point at files their ExternalSecret does not sync:" >&2
  echo "$bad" >&2
  echo "      secretKey is the k8s Secret data key → the mounted filename (bug_205)" >&2
  exit 1
}
