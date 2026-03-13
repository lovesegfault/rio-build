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
