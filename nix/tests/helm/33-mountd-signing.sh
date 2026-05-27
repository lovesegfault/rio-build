# Mountd Mount-admission Ed25519 signing wiring (ADR-022 mount-admission
# credentials, §P0590 Phase 3) — the scheme that replaces the
# never-provisioned symmetric mountdHmac family (helm/31 keeps that
# chart-side render locked until the Phase-5 sweep deletes it).
#
# The asymmetry IS the security property: the scheduler (rio-system)
# holds the ONLY private signing key; the rio-mountd DaemonSet — which
# runs on every builder/fetcher node — holds public trust roots plus its
# own node name and nothing else, so a node compromise yields no
# Mount-admission minting ability anywhere. Locked here, end to end:
#   (1) enabled render: private key → scheduler only; public roots +
#       spec.nodeName fieldRef → DaemonSet only; neither half leaks into
#       any other component or any rio-builders object.
#   (2) guard rails: a half-configured pair fails the render, enabling
#       the superseded mountdHmac together with mountdSigning fails, and
#       the keyless default renders none of it.
#   (3) the production EKS deploy (xtask/src/k8s/eks/deploy.rs) sets
#       exactly the ESO-created Secret names and no longer provisions
#       the legacy mountdHmac value.
#   (4) the ESO ExternalSecrets split: private into rio-system ONLY,
#       public into rio-builders ONLY, with the target names, data keys
#       and Secrets Manager remoteRefs the mounts + bootstrap Job expect.
#   (5) the scheduler/DaemonSet mount exactly those ESO-created Secrets.

priv_path=/etc/rio/mountd-signing/mountd-signing.key
pub_path=/etc/rio/mountd-signing/mountd-signing.pub

# ── (1) chart render with the pair enabled ───────────────────────────
on=$TMPDIR/mountd-signing-on.yaml
helm template rio . \
  --set mountdSigning.privateKeySecretName=rio-mountd-signing-key \
  --set mountdSigning.publicKeySecretName=rio-mountd-signing-pub \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$on"

# Exactly one consumer of each env var across the whole render: the
# scheduler signs, the DaemonSet verifies. A second occurrence means the
# family leaked into a component that has no business holding it; zero
# means a missing include (silently keyless production).
n=$(grep -c RIO_MOUNTD_SIGNING_KEY_PATH "$on")
test "$n" -eq 1 || {
  echo "FAIL: expected exactly 1 RIO_MOUNTD_SIGNING_KEY_PATH (scheduler), got $n" >&2
  exit 1
}
n=$(grep -c RIO_MOUNTD_PUBKEY_PATH "$on")
test "$n" -eq 1 || {
  echo "FAIL: expected exactly 1 RIO_MOUNTD_PUBKEY_PATH (rio-mountd DaemonSet), got $n" >&2
  exit 1
}

# Scheduler Deployment: private-key volume → mount → env, structurally.
sched='select(.kind=="Deployment" and .metadata.name=="rio-scheduler")'
test "$(yq "$sched | .spec.template.spec.volumes[] | select(.name==\"mountd-signing-key\") | .secret.secretName" "$on")" = "rio-mountd-signing-key" || {
  echo "FAIL: rio-scheduler missing the mountd-signing-key Secret volume (rio-mountd-signing-key)" >&2
  exit 1
}
test "$(yq "$sched | .spec.template.spec.containers[0].volumeMounts[] | select(.name==\"mountd-signing-key\") | .mountPath" "$on")" = "/etc/rio/mountd-signing" || {
  echo "FAIL: rio-scheduler missing the mountd-signing-key volumeMount at /etc/rio/mountd-signing" >&2
  exit 1
}
test "$(yq "$sched | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_MOUNTD_SIGNING_KEY_PATH\") | .value" "$on")" = "$priv_path" || {
  echo "FAIL: rio-scheduler RIO_MOUNTD_SIGNING_KEY_PATH != $priv_path" >&2
  exit 1
}
# The scheduler verifies nothing — the public trust roots (and the node
# claim env) must stay off it.
sched_yaml=$(yq "$sched" "$on")
if grep -q 'mountd-signing-pub\|RIO_MOUNTD_PUBKEY_PATH\|RIO_MOUNTD_NODE_NAME' <<<"$sched_yaml"; then
  echo "FAIL: rio-scheduler carries the public trust roots / node-name env — the verifier surface belongs to the DaemonSet only" >&2
  exit 1
fi

# rio-mountd DaemonSet: trust-root volume → mount → env, plus the
# downward-API node name the node-scoped claim check needs. Losing the
# fieldRef while signing is enabled would make every node-scoped token
# unverifiable (daemon skips the node check) — that is the render guard
# the node-scoping addendum calls for.
ds='select(.kind=="DaemonSet" and .metadata.name=="rio-mountd")'
test "$(yq "$ds | .spec.template.spec.volumes[] | select(.name==\"mountd-signing-pub\") | .secret.secretName" "$on")" = "rio-mountd-signing-pub" || {
  echo "FAIL: rio-mountd missing the mountd-signing-pub Secret volume (rio-mountd-signing-pub)" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.containers[0].volumeMounts[] | select(.name==\"mountd-signing-pub\") | .mountPath" "$on")" = "/etc/rio/mountd-signing" || {
  echo "FAIL: rio-mountd missing the mountd-signing-pub volumeMount at /etc/rio/mountd-signing" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_MOUNTD_PUBKEY_PATH\") | .value" "$on")" = "$pub_path" || {
  echo "FAIL: rio-mountd RIO_MOUNTD_PUBKEY_PATH != $pub_path" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_MOUNTD_NODE_NAME\") | .valueFrom.fieldRef.fieldPath" "$on")" = "spec.nodeName" || {
  echo "FAIL: rio-mountd RIO_MOUNTD_NODE_NAME is not a spec.nodeName downward-API fieldRef — node-scoped tokens cannot be checked without it" >&2
  exit 1
}

# The private signing key must never render on ANYTHING in the
# rio-builders namespace (DaemonSet, Pools, network policies, ...) —
# that namespace is the builder/fetcher node surface and keeping
# minting material out of it is the point of the Ed25519 cutover.
builders_yaml=$(yq 'select(.metadata.namespace=="rio-builders")' "$on")
if grep -q 'rio-mountd-signing-key\|RIO_MOUNTD_SIGNING_KEY_PATH\|mountd-signing\.key' <<<"$builders_yaml"; then
  echo "FAIL: the private mountd signing key renders on a rio-builders object — minting material must never reach builder/fetcher nodes" >&2
  exit 1
fi

# Neither half belongs to any other component.
for dep in rio-store rio-gateway rio-controller; do
  dep_yaml=$(yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")" "$on")
  if grep -q 'mountd-signing\|RIO_MOUNTD_SIGNING_KEY_PATH\|RIO_MOUNTD_PUBKEY_PATH' <<<"$dep_yaml"; then
    echo "FAIL: $dep carries mountd-signing material — only the scheduler signs and only the DaemonSet verifies" >&2
    exit 1
  fi
done

# ── (2) guard rails ───────────────────────────────────────────────────
# Keyless default renders none of it (helm/25 pins the DaemonSet half).
off=$TMPDIR/mountd-signing-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$off"
! grep -q 'mountd-signing\|RIO_MOUNTD_SIGNING_KEY_PATH\|RIO_MOUNTD_PUBKEY_PATH\|RIO_MOUNTD_NODE_NAME' "$off" || {
  echo "FAIL: mountd-signing material rendered with mountdSigning unset (default)" >&2
  grep -n 'mountd-signing\|RIO_MOUNTD_SIGNING_KEY_PATH\|RIO_MOUNTD_PUBKEY_PATH\|RIO_MOUNTD_NODE_NAME' "$off" >&2
  exit 1
}

# Pair-or-nothing: a lone private key mints tokens no daemon accepts; a
# lone trust-root file opens the 0666 socket with nothing ever minted.
if helm template rio . \
  --set mountdSigning.privateKeySecretName=rio-mountd-signing-key \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >/dev/null 2>&1; then
  echo "FAIL: mountdSigning.privateKeySecretName without publicKeySecretName should fail the render" >&2
  exit 1
fi
if helm template rio . \
  --set mountdSigning.publicKeySecretName=rio-mountd-signing-pub \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >/dev/null 2>&1; then
  echo "FAIL: mountdSigning.publicKeySecretName without privateKeySecretName should fail the render" >&2
  exit 1
fi

# Mutual exclusivity with the superseded symmetric family: rendering
# both would put a Mount-admission signing key back on every builder
# node — the forcing function against a permanent dual state.
if helm template rio . \
  --set mountdSigning.privateKeySecretName=rio-mountd-signing-key \
  --set mountdSigning.publicKeySecretName=rio-mountd-signing-pub \
  --set mountdHmac.secretName=rio-mountd-hmac \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >/dev/null 2>&1; then
  echo "FAIL: mountdSigning.* together with mountdHmac.secretName should fail the render (the symmetric scheme is superseded and must never be co-enabled)" >&2
  exit 1
fi

# ── (3) the EKS deploy sets exactly the ESO names, and not mountdHmac ─
deploy_src=.eks-deploy.rs
test -f "$deploy_src" || {
  echo "FAIL: $deploy_src not staged — keep the cp in nix/misc-checks.nix helm-lint in sync with this fragment" >&2
  exit 1
}
# rustfmt splits the .set() call across lines (the argument list is past
# fn_call_width), so flatten the file before extracting the values.
# Capture-then-here-string everywhere (never `producer | grep -q`): a
# grep -q early exit can SIGPIPE the producer under pipefail and turn a
# MATCH into a silently skipped FAIL (the r39 pass-gap shape).
deploy_flat=$(tr -s ' \n' ' ' <"$deploy_src")
extract_set() { # helm value key (regex) → the literal it is set to
  m=$(grep -o "\"$1\", *\"[^\"]*\"" <<<"$deploy_flat" | head -n1 || true)
  sed 's/.*, *"\([^"]*\)"/\1/' <<<"$m"
}
priv_name=$(extract_set 'mountdSigning\.privateKeySecretName')
pub_name=$(extract_set 'mountdSigning\.publicKeySecretName')
test -n "$priv_name" && test -n "$pub_name" || {
  echo 'FAIL: the xtask EKS deploy no longer sets the mountdSigning.* pair —' >&2
  echo '      production mountd would stay gid-only and hostUsers:false executor' >&2
  echo '      pods could never Mount. Restore both .set() calls in' >&2
  echo '      xtask/src/k8s/eks/deploy.rs (or update this fragment if the' >&2
  echo '      formatting changed).' >&2
  exit 1
}
# The superseded symmetric secret must NOT be provisioned (ADR-022
# §P0590: the chart family survives until Phase 5, but no deploy may
# enable it).
if grep -q '"mountdHmac\.secretName"' <<<"$deploy_flat"; then
  echo "FAIL: the xtask EKS deploy still sets mountdHmac.secretName — the symmetric mountd token is superseded by mountdSigning and must never be provisioned" >&2
  exit 1
fi
# The Secrets only exist because ESO syncs them.
grep -q '"externalSecrets\.enabled", *"true"' "$deploy_src" || {
  echo "FAIL: the xtask EKS deploy no longer sets externalSecrets.enabled=true —" >&2
  echo "      the $priv_name/$pub_name Secrets it points mountdSigning at are ESO-synced" >&2
  exit 1
}

# ── (4) the chart's ESO objects create exactly those Secrets ─────────
# Render what the EKS deploy renders (ESO on + the names from
# deploy.rs). The namespace split is the load-bearing part: the private
# ExternalSecret lands in rio-system ONLY, the public one in
# rio-builders ONLY.
eks=$TMPDIR/eks-mountd-signing.yaml
helm template rio . \
  --set externalSecrets.enabled=true \
  --set "mountdSigning.privateKeySecretName=$priv_name" \
  --set "mountdSigning.publicKeySecretName=$pub_name" \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$eks"

es_priv='select(.kind=="ExternalSecret" and .metadata.name=="rio-mountd-signing-key")'
es_pub='select(.kind=="ExternalSecret" and .metadata.name=="rio-mountd-signing-pub")'

ns_priv=$(yq -N "$es_priv | .metadata.namespace" "$eks" | sort | paste -sd' ' -)
test "$ns_priv" = "rio-system" || {
  echo "FAIL: rio-mountd-signing-key ExternalSecret namespaces '$ns_priv', want exactly 'rio-system' (the private key never syncs anywhere else)" >&2
  exit 1
}
ns_pub=$(yq -N "$es_pub | .metadata.namespace" "$eks" | sort | paste -sd' ' -)
test "$ns_pub" = "rio-builders" || {
  echo "FAIL: rio-mountd-signing-pub ExternalSecret namespaces '$ns_pub', want exactly 'rio-builders'" >&2
  exit 1
}

test "$(yq -N "$es_priv | .spec.target.name" "$eks")" = "$priv_name" || {
  echo "FAIL: private ESO target Secret name != deploy.rs mountdSigning.privateKeySecretName '$priv_name'" >&2
  exit 1
}
test "$(yq -N "$es_pub | .spec.target.name" "$eks")" = "$pub_name" || {
  echo "FAIL: public ESO target Secret name != deploy.rs mountdSigning.publicKeySecretName '$pub_name'" >&2
  exit 1
}

test "$(yq -N "$es_priv | .spec.data[0].secretKey" "$eks")" = "mountd-signing.key" || {
  echo "FAIL: rio-mountd-signing-key ExternalSecret data key != mountd-signing.key (RIO_MOUNTD_SIGNING_KEY_PATH expects $priv_path)" >&2
  exit 1
}
test "$(yq -N "$es_pub | .spec.data[0].secretKey" "$eks")" = "mountd-signing.pub" || {
  echo "FAIL: rio-mountd-signing-pub ExternalSecret data key != mountd-signing.pub (RIO_MOUNTD_PUBKEY_PATH expects $pub_path)" >&2
  exit 1
}

test "$(yq -N "$es_priv | .spec.data[0].remoteRef.key" "$eks")" = "rio/mountd-signing-key" || {
  echo "FAIL: rio-mountd-signing-key ExternalSecret remoteRef != rio/mountd-signing-key (the bootstrap Job creates that Secrets Manager entry)" >&2
  exit 1
}
test "$(yq -N "$es_pub | .spec.data[0].remoteRef.key" "$eks")" = "rio/mountd-signing-pub" || {
  echo "FAIL: rio-mountd-signing-pub ExternalSecret remoteRef != rio/mountd-signing-pub (the bootstrap Job creates that Secrets Manager entry)" >&2
  exit 1
}

# ── (5) the consumers mount THOSE Secrets ────────────────────────────
vol=$(yq -N "$sched | .spec.template.spec.volumes[] | select(.name==\"mountd-signing-key\") | .secret.secretName" "$eks")
test "$vol" = "$priv_name" || {
  echo "FAIL: rio-scheduler mountd-signing-key volume secretName '$vol' != '$priv_name'" >&2
  exit 1
}
vol=$(yq -N "$ds | .spec.template.spec.volumes[] | select(.name==\"mountd-signing-pub\") | .secret.secretName" "$eks")
test "$vol" = "$pub_name" || {
  echo "FAIL: rio-mountd mountd-signing-pub volume secretName '$vol' != '$pub_name'" >&2
  exit 1
}

echo "OK"
