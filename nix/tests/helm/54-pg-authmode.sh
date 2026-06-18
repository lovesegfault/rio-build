# rio.pgEnv render-time guards (_helpers.tpl). Both misconfigurations
# guarded here used to render "successfully" and only fail in the
# cluster:
# - a typo'd authMode fell through to the password branch — pods came
#   up healthy on the rotating master password while the operator
#   believed IAM was on;
# - iam without externalSecrets rendered a URL whose sslrootcert=
#   points at the rdsCa bundle that mount family never mounts — every
#   PG connect failed at runtime, not at deploy.

eks_args=(
  --set global.image.tag=test
  --set externalSecrets.enabled=true
  --set externalSecrets.auroraEndpoint=db.example.invalid
  --set "externalSecrets.auroraSecretArn=arn:aws:secretsmanager:eu:1:secret:x"
  --set scheduler.sla.cluster=pg-authmode-stub
)

# Bad enum value must fail the render, naming the knob.
if out=$(helm template rio . "${eks_args[@]}" --set postgres.authMode=IAM 2>&1); then
  echo "FAIL: postgres.authMode=IAM (bad enum) rendered successfully" >&2
  exit 1
fi
printf '%s\n' "$out" | grep -q 'postgres.authMode must be' || {
  echo "FAIL: bad-enum render failed for the wrong reason:" >&2
  printf '%s\n' "$out" | tail -5 >&2
  exit 1
}

# iam without externalSecrets must fail (rdsCa mount family is gated
# on externalSecrets.enabled; the URL would point at an unmounted file).
if out=$(helm template rio . --set global.image.tag=test \
  --set postgres.authMode=iam 2>&1); then
  echo "FAIL: authMode=iam without externalSecrets.enabled rendered successfully" >&2
  exit 1
fi
printf '%s\n' "$out" | grep -q 'requires externalSecrets.enabled' || {
  echo "FAIL: iam-without-ESO render failed for the wrong reason:" >&2
  printf '%s\n' "$out" | tail -5 >&2
  exit 1
}

# Positive control: a correct iam render carries the hardcoded rio_app
# user (there is no postgres.iamUser knob — see values.yaml).
# Herestrings, not `printf | grep -q`: -q exits on first match, the
# printf side takes SIGPIPE, and the harness' pipefail turns that into
# a false failure on multi-thousand-line renders.
render=$(helm template rio . "${eks_args[@]}" --set postgres.authMode=iam)
grep -qF 'postgres://rio_app@db.example.invalid:5432/rio' <<<"$render" || {
  echo "FAIL: iam render does not carry the hardcoded rio_app URL" >&2
  exit 1
}

# In iam mode the STORE-namespace rio-postgres ExternalSecret copy is
# skipped (no consumer); the SYSTEM-namespace copy must survive (the
# migrate runner and xtask qa read it).
ns=$(printf '%s\n' "$render" \
  | yq 'select(.kind=="ExternalSecret" and .metadata.name=="rio-postgres") | .metadata.namespace')
if [ "$ns" != rio-system ]; then
  echo "FAIL: iam render must keep exactly the rio-system rio-postgres ExternalSecret (got: '$ns')" >&2
  exit 1
fi
