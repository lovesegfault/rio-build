{{/*
Full image reference for a component.
Usage: {{ include "rio.image" (list . "rio-scheduler") }}
Registry prefix from global.image.registry (empty → bare name for
airgap-loaded dev images). Tag from global.image.tag (NEVER defaulted —
missing tag = ImagePullBackOff, which is the right failure mode).
*/}}
{{- define "rio.image" -}}
{{- $root := index . 0 -}}
{{- $repo := index . 1 -}}
{{- $g := $root.Values.global.image -}}
{{- if $g.registry -}}
{{- printf "%s/%s:%s" $g.registry $repo $g.tag | quote -}}
{{- else -}}
{{- printf "%s:%s" $repo $g.tag | quote -}}
{{- end -}}
{{- end -}}

{{/*
Selector labels — STABLE, immutable once a Deployment/StatefulSet exists.
Changing these is a breaking change (kubectl apply rejects selector updates).
Only the name label. Never add part-of here (that was the old kustomize
commonLabels footgun).
*/}}
{{- define "rio.selectorLabels" -}}
app.kubernetes.io/name: {{ . }}
{{- end -}}

{{/*
Full label set — goes on metadata.labels ONLY, never spec.selector.
*/}}
{{- define "rio.labels" -}}
app.kubernetes.io/name: {{ .name }}
app.kubernetes.io/component: {{ .component }}
app.kubernetes.io/part-of: rio-build
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ .root.Chart.Name }}-{{ .root.Chart.Version }}
{{- end -}}

{{/*
Guarded mount families — one parameterized template for the four
env+mount+volume triples that every control-plane pod conditionally
carries (tls / jwtVerify / jwtSign / cov). Replaces the previous 12
near-identical `rio.{tls,jwtVerify,jwtSign,cov}{Env,VolumeMount,Volume}`
defines: same self-guard pattern (renders nothing when the family's
.Values.*.enabled flag is false), same output, single edit point.

Usage:
  {{- include "rio.mounts" (dict "root" . "form" "env"    "want" (list "cov" "jwtSign")) | nindent 12 }}
  {{- include "rio.mounts" (dict "root" . "form" "mount"  "want" (list "cov" "jwtVerify")) | nindent 12 }}
  {{- include "rio.mounts" (dict "root" . "form" "volume" "want" (list "cov")) | nindent 8 }}

`want` is the ordered list of families to emit; each entry self-guards
on its `.on` flag so callers include unconditionally and drop the
hand-rolled `{{ if or .Values.coverage.enabled ... }}`
wrapper around `volumes:` / `volumeMounts:` (a null list-key is valid
PodSpec — k8s treats it as empty).

Families:
  jwtVerify  .Values.jwt.enabled. SCHEDULER + STORE. ConfigMap
             rio-jwt-pubkey (PUBLIC ed25519 verifying key). Without
             the mount, cfg.jwt.key_path stays None → interceptor is
             inert → silent fail-open. See r[sec.jwt.pubkey-mount].
  jwtSign    .Values.jwt.enabled. GATEWAY ONLY. Secret rio-jwt-signing
             (private ed25519 seed). Same RIO_JWT__KEY_PATH env var as
             jwtVerify — JwtConfig is a shared type; gateway loads it
             as SigningKey seed, scheduler/store as VerifyingKey.
  assignHmac .Values.jwt.enabled. SCHEDULER (signs assignment
             tokens at dispatch) + STORE (verifies them on
             PutPath/AppendLog). Secret rio-hmac →
             /etc/rio/assign-hmac/hmac.key, env RIO_HMAC_KEY_PATH.
             jwt-gated: the boot coherence law (jwt ⇒ service ∧ hmac,
             r[store.authz.key-coherence]) refuses a store that
             advertises tenant auth while builder ingest runs keyless
             — before this family existed NOTHING mounted rio-hmac,
             so every jwt-enabled deployment was exactly that
             half-configuration (assignment enforcement silently in
             dev-mode). Keyless (non-jwt) deployments stay dev-mode
             by doctrine.
  serviceHmac  always-on. GATEWAY+SCHEDULER+CONTROLLER (signers) +
             STORE+SCHEDULER (verifiers).
             Secret rio-service-hmac → /etc/rio/hmac/service-hmac.key,
             env RIO_SERVICE_HMAC_KEY_PATH. Service-identity HMAC
             replaced mTLS CN-allowlisting when transport encryption
             moved to Cilium WireGuard (D2). Gateway signs
             x-rio-service-token on store PutPath; scheduler signs it
             on dispatch-time FindMissingPaths/QueryPathInfo so the
             store honours x-rio-probe-tenant-id
             (r[sched.dispatch.fod-substitute]); store verifies.
  cov        .Values.coverage.enabled. hostPath /var/lib/rio/cov for
             LLVM profraw atexit flush. POD_NAME in the filename: pods
             share the hostPath and all run PID 1, so %p alone does NOT
             disambiguate a replacement pod on the same node (leader-
             election failover, rollout) — its PID-1 profraw would
             OVERWRITE the predecessor's. Kubelet's dependent-env-var
             expansion substitutes $(POD_NAME) at container start. %p
             still covers in-container CrashLoop restarts, %m covers
             same-PID-different-binary.
*/}}
{{- define "rio.mounts" -}}
{{- $root := .root -}}
{{- $form := .form -}}
{{- $fams := dict
      "jwtVerify" (dict
        "on"   $root.Values.jwt.enabled
        "vol"  "jwt-pubkey" "path" "/etc/rio/jwt" "ro" true
        "src"  (dict "configMap" (dict "name" "rio-jwt-pubkey"))
        "env"  (list
          (dict "name" "RIO_JWT__KEY_PATH" "value" "/etc/rio/jwt/ed25519_pubkey")))
      "jwtSign" (dict
        "on"   $root.Values.jwt.enabled
        "vol"  "jwt-signing" "path" "/etc/rio/jwt" "ro" true
        "src"  (dict "secret" (dict "secretName" "rio-jwt-signing"))
        "env"  (list
          (dict "name" "RIO_JWT__KEY_PATH" "value" "/etc/rio/jwt/ed25519_seed")))
      "cov" (dict
        "on"   $root.Values.coverage.enabled
        "vol"  "cov" "path" "/var/lib/rio/cov" "ro" false
        "src"  (dict "hostPath" (dict "path" "/var/lib/rio/cov" "type" "DirectoryOrCreate"))
        "env"  (list
          (dict "name" "POD_NAME" "valueFrom" (dict "fieldRef" (dict "fieldPath" "metadata.name")))
          (dict "name" "LLVM_PROFILE_FILE" "value" "/var/lib/rio/cov/rio-$(POD_NAME)-%p-%m.profraw")))
      "assignHmac" (dict
        "on"   $root.Values.jwt.enabled
        "vol"  "assign-hmac" "path" "/etc/rio/assign-hmac" "ro" true
        "src"  (dict "secret" (dict "secretName" "rio-hmac"))
        "env"  (list
          (dict "name" "RIO_HMAC_KEY_PATH" "value" "/etc/rio/assign-hmac/hmac.key")))
      "serviceHmac" (dict
        "on"   true
        "vol"  "service-hmac" "path" "/etc/rio/hmac" "ro" true
        "src"  (dict "secret" (dict "secretName" "rio-service-hmac"))
        "env"  (list
          (dict "name" "RIO_SERVICE_HMAC_KEY_PATH" "value" "/etc/rio/hmac/service-hmac.key")))
-}}
{{- range .want }}
{{- $f := get $fams . }}
{{- if $f.on }}
{{- if eq $form "env" }}
{{- range $f.env }}
- {{ toYaml . | nindent 2 | trim }}
{{- end }}
{{- else if eq $form "mount" }}
- name: {{ $f.vol }}
  mountPath: {{ $f.path }}
{{- if $f.ro }}
  readOnly: true
{{- end }}
{{- else if eq $form "volume" }}
- name: {{ $f.vol }}
{{ toYaml $f.src | indent 2 }}
{{- end }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
RUST_LOG env var. Self-guarded — empty global.logLevel renders nothing
(binary falls back to "info"). Include unconditionally with `| nindent 12`.
*/}}
{{- define "rio.rustLogEnv" -}}
{{- with .Values.global.logLevel }}
- name: RUST_LOG
  value: {{ . | quote }}
{{- end }}
{{- end -}}

{{/*
PSA-restricted securityContext for control-plane pods (scheduler,
gateway, controller, store). Satisfies pod-security.kubernetes.io/
enforce=restricted — runAsNonRoot, drop-ALL, seccomp:RuntimeDefault,
readOnlyRootFilesystem. UID 65532 = distroless nonroot; nix/docker.nix
sets config.User to match so `docker run` without k8s also runs
unprivileged.

Coverage branch: explicit runAsUser/runAsGroup: 0. Coverage mode
mounts a hostPath at /var/lib/rio/cov for LLVM profraw collection.
hostPath is NOT subject to fsGroup (k8s docs: "fsGroup ignored for
hostPath"), so a UID-65532 process can't write to root-owned
/var/lib/rio/cov → profraw atexit flush fails EACCES → zero coverage.
Omitting securityContext entirely does NOT yield root: the runtime
falls back to the IMAGE's config.User (65532, baked in nix/docker.nix
nonrootUser), so the EACCES persists. Explicit runAsUser: 0 overrides
the image. The k3s-full fixture sets namespaces.{system,store}.psa
to privileged under coverage (k3s-full.nix optionalAttrs coverage
block) so the root pod is admitted, and pre-creates /var/lib/rio/cov
0777 via tmpfiles as belt-and-suspenders.

rio.podSecurityContext goes at spec.template.spec (pod-level);
rio.containerSecurityContext at spec.template.spec.containers[]
(container-level). PSA restricted requires BOTH — seccomp/runAsNonRoot
at pod level, allowPrivilegeEscalation/capabilities/readOnlyRoot at
container level.
*/}}
{{- define "rio.podSecurityContext" -}}
{{- if .Values.coverage.enabled }}
securityContext:
  runAsUser: 0
  runAsGroup: 0
{{- else }}
securityContext:
  runAsNonRoot: true
  runAsUser: 65532
  runAsGroup: 65532
  fsGroup: 65532
  seccompProfile:
    type: RuntimeDefault
{{- end }}
{{- end -}}

{{- define "rio.containerSecurityContext" -}}
{{- if not .Values.coverage.enabled }}
securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: [ALL]
{{- end }}
{{- end -}}

{{/*
Render an optional boolean field. Unlike `with`, this renders when the
value is explicitly `false` (Helm's `with` is falsy-skip — setting
`hostUsers: false` in values produces NO key, controller default wins).
Usage:
  {{- include "rio.optBool" (list $ctx "hostUsers" $ctx.hostUsers) | nindent 2 }}
$ctx is the map to hasKey against; $key the field name; $val the value.
*/}}
{{/*
k8s Quantity string ("500Gi" / "2Ti") → bytes. Minimal: only the
integer Ki/Mi/Gi/Ti suffixes the chart uses (`karpenter.dataVolumeSize`).
Fractional ("1.5Ti") and unrecognized ("500G", decimal SI) Quantities
`fail` at `helm template` time — sprig `int64` would silently coerce
them to 0, which zeroes `max_node_disk` in `controller.toml` and makes
`cover::sizing`'s over-cap filter drop every intent (a fleet-wide
provisioning halt with no alert). Bare integers are bytes.
*/}}
{{- define "rio.quantityBytes" -}}
{{- $q := toString . -}}
{{- if not (regexMatch "^[0-9]+(Ki|Mi|Gi|Ti)?$" $q) -}}
{{- fail (printf "rio.quantityBytes: %q is not an integer Quantity with a Ki/Mi/Gi/Ti suffix or no suffix; fix karpenter.dataVolumeSize in values.yaml" .) -}}
{{- else if hasSuffix "Ti" $q -}}
{{- mul (trimSuffix "Ti" $q | int64) 1099511627776 -}}
{{- else if hasSuffix "Gi" $q -}}
{{- mul (trimSuffix "Gi" $q | int64) 1073741824 -}}
{{- else if hasSuffix "Mi" $q -}}
{{- mul (trimSuffix "Mi" $q | int64) 1048576 -}}
{{- else if hasSuffix "Ki" $q -}}
{{- mul (trimSuffix "Ki" $q | int64) 1024 -}}
{{- else -}}
{{- int64 $q -}}
{{- end -}}
{{- end -}}

{{- define "rio.optBool" -}}
{{- $ctx := index . 0 -}}
{{- $key := index . 1 -}}
{{- $val := index . 2 -}}
{{- if hasKey $ctx $key -}}
{{ $key }}: {{ $val }}
{{- end -}}
{{- end -}}

{{/*
IPv6-only Service fields. rio assumes a v6 Service CIDR; the chart
fails at apply on v4-only clusters (SingleStack + [IPv6] is rejected
by the apiserver). v4 ingress/egress is external infra (NLB, NAT64).

Usage (root context, inside a Service spec block):
  spec:
    {{- include "rio.ipFamily" . | nindent 2 }}
    type: ClusterIP
*/}}
{{- define "rio.ipFamily" -}}
ipFamilyPolicy: SingleStack
ipFamilies:
  - IPv6
{{- end -}}

{{/*
ClusterIP + headless Service pair for a gRPC component. scheduler + store
both expose this exact pair: ClusterIP for per-call connects (controller
reconcilers, rio-cli, tests — UNAVAILABLE-then-retry is fine off the hot
path); headless for BalancedChannel DNS resolution (gateway, workers
resolve pod IPs and p2c — a sticky single-channel against the ClusterIP
means scaling replicas doesn't help, I-077).

Usage: {{ include "rio.grpcServicePair" (dict "root" . "name" "rio-store" "ns" .Values.namespaces.store.name "component" "store" "port" 9002) }}
*/}}
{{- define "rio.grpcServicePair" -}}
{{- $root := .root -}}
---
apiVersion: v1
kind: Service
metadata:
  name: {{ .name }}
  namespace: {{ .ns }}
  labels:
    {{- include "rio.labels" (dict "root" $root "name" .name "component" .component) | nindent 4 }}
spec:
  {{- include "rio.ipFamily" $root | nindent 2 }}
  type: ClusterIP
  selector:
    {{- include "rio.selectorLabels" .name | nindent 4 }}
  ports:
    - name: grpc
      port: {{ .port }}
      targetPort: grpc
---
apiVersion: v1
kind: Service
metadata:
  name: {{ .name }}-headless
  namespace: {{ .ns }}
  labels:
    {{- include "rio.labels" (dict "root" $root "name" .name "component" .component) | nindent 4 }}
spec:
  {{- include "rio.ipFamily" $root | nindent 2 }}
  clusterIP: None
  selector:
    {{- include "rio.selectorLabels" .name | nindent 4 }}
  ports:
    - name: grpc
      port: {{ .port }}
      targetPort: grpc
{{- end -}}

{{/*
rio.promTrigger: emit one KEDA prometheus AverageValue trigger whose
metric and threshold knob are unit-checked against
files/metric-units.json (bug_299: a threshold seeded in PATH units once
divided a DERIVATION-count gauge — silently overscaling-blind). Args
(positional list): root, serverAddress, query, metric name, knob name,
threshold value. A missing registry row or a unit mismatch fails the
render (helm-lint + every VM test), so a future mismatched trigger
cannot ship.
*/}}
{{- define "rio.promTrigger" -}}
{{- $root := index . 0 -}}
{{- $address := index . 1 -}}
{{- $query := index . 2 -}}
{{- $metric := index . 3 -}}
{{- $knob := index . 4 -}}
{{- $threshold := index . 5 -}}
{{- $units := $root.Files.Get "files/metric-units.json" | fromJson -}}
{{- $mu := index $units "metrics" $metric -}}
{{- $ku := index $units "knobs" $knob -}}
{{- if not $mu -}}
{{- fail (printf "rio.promTrigger: metric %q has no unit in files/metric-units.json" $metric) -}}
{{- end -}}
{{- if not $ku -}}
{{- fail (printf "rio.promTrigger: knob %q has no unit in files/metric-units.json" $knob) -}}
{{- end -}}
{{- if ne $mu $ku -}}
{{- fail (printf "rio.promTrigger: unit mismatch — metric %s counts %q but knob %s is in %q" $metric $mu $knob $ku) -}}
{{- end -}}
- type: prometheus
  metricType: AverageValue
  metadata:
    serverAddress: {{ $address | quote }}
    query: {{ $query }}
    threshold: {{ $threshold | quote }}
{{- end -}}

{{/*
Disruption-gate predicates (merged_bug_378): name the AXIS a
multi-replica rule keys on, so call sites say WHICH question they ask
instead of repeating raw ternaries that silently pick the wrong axis.
Both take a component values dict (.autoscaling.{enabled,minReplicas,
maxReplicas} + .replicas) and return "true"/"" for use under `if`.

rio.mayRunMultiple — CEILING: true when MORE THAN ONE pod may exist
(autoscaling ceiling, else static replicas). For rules that must hold
whenever multiple pods are POSSIBLE: required one-per-node
anti-affinity (constrains nothing while one pod exists, must already
hold when the autoscaler adds the second).
*/}}
{{- define "rio.mayRunMultiple" -}}
{{- $c := . -}}
{{- if gt (int (ternary $c.autoscaling.maxReplicas ($c.replicas | default 1) $c.autoscaling.enabled)) 1 -}}true{{- end -}}
{{- end -}}

{{/*
rio.alwaysRunsMultiple — FLOOR: true when MORE THAN ONE pod is
GUARANTEED (autoscaling floor, else static replicas). For rules that
are safe ONLY with a guaranteed second pod: an explicit
maxUnavailable:1 rollout strategy (at one live replica it makes the
Deployment Available at zero ready pods) and minAvailable-style
budgets (against one pod they block every drain).
*/}}
{{- define "rio.alwaysRunsMultiple" -}}
{{- $c := . -}}
{{- if gt (int (ternary $c.autoscaling.minReplicas ($c.replicas | default 1) $c.autoscaling.enabled)) 1 -}}true{{- end -}}
{{- end -}}
