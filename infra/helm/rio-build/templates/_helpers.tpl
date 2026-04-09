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
  {{- include "rio.mounts" (dict "root" . "form" "mount"  "want" (list "tls" "cov" "jwtVerify")) | nindent 12 }}
  {{- include "rio.mounts" (dict "root" . "form" "volume" "want" (list "tls" "cov") "tlsSecret" "rio-gateway-tls") | nindent 8 }}

`want` is the ordered list of families to emit; each entry self-guards
on its `.on` flag so callers include unconditionally and drop the
hand-rolled `{{ if or .Values.tls.enabled .Values.coverage.enabled ... }}`
wrapper around `volumes:` / `volumeMounts:` (a null list-key is valid
PodSpec — k8s treats it as empty).

Families:
  tls        .Values.tls.enabled. Per-component secret (cert-manager
             Certificate per pod CN) — caller passes `tlsSecret`.
  jwtVerify  .Values.jwt.enabled. SCHEDULER + STORE. ConfigMap
             rio-jwt-pubkey (PUBLIC ed25519 verifying key). Without
             the mount, cfg.jwt.key_path stays None → interceptor is
             inert → silent fail-open. See r[sec.jwt.pubkey-mount].
  jwtSign    .Values.jwt.enabled. GATEWAY ONLY. Secret rio-jwt-signing
             (private ed25519 seed). Same RIO_JWT__KEY_PATH env var as
             jwtVerify — JwtConfig is a shared type; gateway loads it
             as SigningKey seed, scheduler/store as VerifyingKey.
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
{{- $tlsSecret := .tlsSecret | default "" -}}
{{- $fams := dict
      "tls" (dict
        "on"   $root.Values.tls.enabled
        "vol"  "tls" "path" "/etc/rio/tls" "ro" true
        "src"  (dict "secret" (dict "secretName" $tlsSecret))
        "env"  (list
          (dict "name" "RIO_TLS__CERT_PATH" "value" "/etc/rio/tls/tls.crt")
          (dict "name" "RIO_TLS__KEY_PATH"  "value" "/etc/rio/tls/tls.key")
          (dict "name" "RIO_TLS__CA_PATH"   "value" "/etc/rio/tls/ca.crt")))
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

Self-guarded on NOT coverage.enabled: coverage mode mounts a hostPath
at /var/lib/rio/cov for LLVM profraw collection. hostPath is NOT
subject to fsGroup (k8s docs: "fsGroup ignored for hostPath"), so
a UID-65532 process can't write to root-owned /var/lib/rio/cov →
profraw atexit flush fails EACCES → zero coverage. Under coverage
mode the k3s-full fixture overrides namespaces.{system,store}.psa
to privileged (k3s-full.nix optionalAttrs coverage block) so the
unguarded pod is still admitted.

rio.podSecurityContext goes at spec.template.spec (pod-level);
rio.containerSecurityContext at spec.template.spec.containers[]
(container-level). PSA restricted requires BOTH — seccomp/runAsNonRoot
at pod level, allowPrivilegeEscalation/capabilities/readOnlyRoot at
container level.
*/}}
{{- define "rio.podSecurityContext" -}}
{{- if not .Values.coverage.enabled }}
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
{{- define "rio.optBool" -}}
{{- $ctx := index . 0 -}}
{{- $key := index . 1 -}}
{{- $val := index . 2 -}}
{{- if hasKey $ctx $key -}}
{{ $key }}: {{ $val }}
{{- end -}}
{{- end -}}

{{/*
Dual-stack Service ipFamily fields. Emits ipFamilyPolicy + ipFamilies on
every in-cluster Service builders dial when dualStack.enabled. PreferDualStack
(not Require) so the chart still applies on v4-only clusters — the apiserver
quietly assigns the single available family. P0542: builders may run on a
v6 pod CIDR (I-073/I-079 IPv4 subnet exhaustion at autoscale); the Services
they dial need an AAAA-backed ClusterIP / pod-IP set.

Usage (root context, inside a Service spec block):
  spec:
    {{- include "rio.ipFamily" . | nindent 2 }}
    type: ClusterIP
*/}}
{{- define "rio.ipFamily" -}}
{{- with .Values.dualStack -}}
{{- if .enabled -}}
ipFamilyPolicy: {{ .policy | default "PreferDualStack" }}
{{- with .ipFamilies }}
ipFamilies:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
NetworkPolicy egress rule fragments. Each renders ONE `egress:` list
item — include with `| nindent 4` inside a policy's `egress:` block.
Single edit point for port/selector changes; before extraction these
three stanzas were copy-pasted verbatim across builder-egress,
fetcher-egress, rio-controller-egress, store-egress(-upstream) and
rio-dashboard-egress (a port move on scheduler/store meant editing 6+
sites in a security-critical file with no compile-time check they
stayed in sync).

rio.egressDns takes no context (kube-system is a literal). Scheduler
and Store take the root context to read .Values.namespaces.{system,
store}.name. TCP/53: DNS falls back to TCP for responses >512 bytes
(DNSSEC, large SRV records, long CNAME chains) — missing TCP =
intermittent resolution failures on large responses.
*/}}
{{- define "rio.egressDns" -}}
- to:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: kube-system
  ports:
    - {protocol: UDP, port: 53}
    - {protocol: TCP, port: 53}
{{- end -}}

{{- define "rio.egressScheduler" -}}
- to:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: {{ .Values.namespaces.system.name }}
      podSelector:
        matchLabels:
          app.kubernetes.io/name: rio-scheduler
  ports: [{protocol: TCP, port: 9001}]
{{- end -}}

{{- define "rio.egressStore" -}}
- to:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: {{ .Values.namespaces.store.name }}
      podSelector:
        matchLabels:
          app.kubernetes.io/name: rio-store
  ports: [{protocol: TCP, port: 9002}]
{{- end -}}
