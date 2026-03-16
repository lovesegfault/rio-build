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
TLS mount block — volume + volumeMount + RIO_TLS__* env. Same for every
component; only the secret name varies. Call from inside the container
spec; the caller must also append the matching volumes: entry.
*/}}
{{- define "rio.tlsEnv" -}}
- name: RIO_TLS__CERT_PATH
  value: /etc/rio/tls/tls.crt
- name: RIO_TLS__KEY_PATH
  value: /etc/rio/tls/tls.key
- name: RIO_TLS__CA_PATH
  value: /etc/rio/tls/ca.crt
{{- end -}}

{{- define "rio.tlsVolumeMount" -}}
- name: tls
  mountPath: /etc/rio/tls
  readOnly: true
{{- end -}}

{{- define "rio.tlsVolume" -}}
- name: tls
  secret:
    secretName: {{ . }}
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
Coverage profraw collection. Self-guarded on .Values.coverage.enabled
— renders nothing when disabled. Include unconditionally in each pod
template alongside rustLogEnv/tlsVolumeMount.

LLVM writes profraws via an atexit handler. Graceful shutdown
(SIGTERM → main returns) flushes them. hostPath lands the files
on the NODE filesystem so collectCoverage can tar them after pod
deletion.

POD_NAME in the filename: all pods on a node share the hostPath.
In containers, the main process is PID 1 — %p alone does NOT
disambiguate two pods of the same binary on the same node. When
a Deployment replaces a killed pod (leader-election failover,
controller restart, gateway rollout), the replacement lands on
the SAME node and its PID-1 profraw OVERWRITES the predecessor's.
Kubelet's dependent-env-var expansion substitutes $(POD_NAME)
with metadata.name at container start, before LLVM sees the
string. %p still covers in-container restarts (CrashLoop within
the same pod), %m covers same-PID-different-binary.
*/}}
{{- define "rio.covEnv" -}}
{{- if .Values.coverage.enabled }}
- name: POD_NAME
  valueFrom:
    fieldRef:
      fieldPath: metadata.name
- name: LLVM_PROFILE_FILE
  value: /var/lib/rio/cov/rio-$(POD_NAME)-%p-%m.profraw
{{- end }}
{{- end -}}

{{- define "rio.covVolumeMount" -}}
{{- if .Values.coverage.enabled }}
- name: cov
  mountPath: /var/lib/rio/cov
{{- end }}
{{- end -}}

{{- define "rio.covVolume" -}}
{{- if .Values.coverage.enabled }}
- name: cov
  hostPath:
    path: /var/lib/rio/cov
    type: DirectoryOrCreate
{{- end }}
{{- end -}}
