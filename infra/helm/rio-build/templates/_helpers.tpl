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
component; only the secret name varies. Self-guarded on
.Values.tls.enabled (renders nothing when disabled, so callers include
unconditionally with `| nindent N`) — same pattern as rio.jwt* and
rio.cov* below.

rio.tlsVolume takes (dict "root" . "name" "rio-<component>-tls") because
it needs BOTH the root context (for .Values.tls.enabled) AND the
per-component secret name.
*/}}
{{- define "rio.tlsEnv" -}}
{{- if .Values.tls.enabled }}
- name: RIO_TLS__CERT_PATH
  value: /etc/rio/tls/tls.crt
- name: RIO_TLS__KEY_PATH
  value: /etc/rio/tls/tls.key
- name: RIO_TLS__CA_PATH
  value: /etc/rio/tls/ca.crt
{{- end }}
{{- end -}}

{{- define "rio.tlsVolumeMount" -}}
{{- if .Values.tls.enabled }}
- name: tls
  mountPath: /etc/rio/tls
  readOnly: true
{{- end }}
{{- end -}}

{{- define "rio.tlsVolume" -}}
{{- if .root.Values.tls.enabled }}
- name: tls
  secret:
    secretName: {{ .name }}
{{- end }}
{{- end -}}

{{/*
JWT pubkey mount — SCHEDULER + STORE. Self-guarded on .Values.jwt.enabled
(renders nothing when disabled, so callers include unconditionally with
`| nindent 12`). ConfigMap is PUBLIC (ed25519 verifying key) — mounted
read-only, no Secret perms needed. key_path matches what the main.rs
wiring reads (rio-common JwtConfig.key_path → RIO_JWT__KEY_PATH env).

File-key-mapping: ConfigMap data key `ed25519_pubkey` → file
`/etc/rio/jwt/ed25519_pubkey`. scheduler/main.rs doc-comment already
references this path — this mount makes it real.

Without the mount, cfg.jwt.key_path stays None and the interceptor
falls through to inert mode (every RPC passes, no Claims attached) —
a silent fail-open when the operator thought jwt.enabled=true meant
enforcement. See r[sec.jwt.pubkey-mount].
*/}}
{{- define "rio.jwtVerifyEnv" -}}
{{- if .Values.jwt.enabled }}
- name: RIO_JWT__KEY_PATH
  value: /etc/rio/jwt/ed25519_pubkey
{{- end }}
{{- end -}}

{{- define "rio.jwtVerifyVolumeMount" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-pubkey
  mountPath: /etc/rio/jwt
  readOnly: true
{{- end }}
{{- end -}}

{{- define "rio.jwtVerifyVolume" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-pubkey
  configMap:
    name: rio-jwt-pubkey
{{- end }}
{{- end -}}

{{/*
JWT signing seed mount — GATEWAY ONLY. Secret (private ed25519 seed).
Same self-guard pattern; gateway main.rs reads RIO_JWT__KEY_PATH for
the SIGNING seed path (JwtConfig is shared type, both sides use
key_path — gateway loads it as a SigningKey seed, scheduler/store
load it as a VerifyingKey). Gateway decodes the Secret's base64 layer
→ 32 raw bytes → SigningKey::from_bytes.
*/}}
{{- define "rio.jwtSignEnv" -}}
{{- if .Values.jwt.enabled }}
- name: RIO_JWT__KEY_PATH
  value: /etc/rio/jwt/ed25519_seed
{{- end }}
{{- end -}}

{{- define "rio.jwtSignVolumeMount" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-signing
  mountPath: /etc/rio/jwt
  readOnly: true
{{- end }}
{{- end -}}

{{- define "rio.jwtSignVolume" -}}
{{- if .Values.jwt.enabled }}
- name: jwt-signing
  secret:
    secretName: rio-jwt-signing
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
smarter-device-manager conf.yaml body. Single source of truth for the
fuse + kvm devicematch list — consumed by BOTH the DaemonSet ConfigMap
(k3s/kind path, device-plugin.yaml) and the static-pod initContainer
(EKS path, karpenter.yaml userData). Keeps the kvm entry — added in
dd9a5c41 — from drifting between the two delivery modes.
*/}}
{{- define "rio.devicePluginConf" -}}
- devicematch: ^fuse$
  nummaxdevices: {{ .Values.devicePlugin.fuseMaxDevices }}
- devicematch: ^kvm$
  nummaxdevices: {{ .Values.devicePlugin.kvmMaxDevices }}
{{- end -}}

{{/*
Standalone Pod manifest for the device-plugin, base64-encoded into
Bottlerocket's settings.kubernetes.static-pods (karpenter.yaml userData).
kubelet starts it locally at node boot — no DaemonSet schedule + image-
pull + register round-trip (~5-15s on the DS path; ~1-2s here). Mirrors
P0541's seccomp bootstrap-container approach: bake the per-node agent
into userData, let Karpenter Drift roll it on change.

Differences vs the DaemonSet pod template (device-plugin.yaml):

  - namespace: kube-system. Static-pod mirror objects land in whatever
    namespace the manifest says; rio-builders may not exist yet on a
    fresh cluster's first node. kube-system always does.

  - NO nodeSelector/affinity/tolerations. Static pods are node-local —
    kubelet starts them unconditionally. The userData is attached to
    EC2NodeClass rio-default, which backs ALL Karpenter NodePools
    (builder/fetcher/metal AND rio-general). The plugin therefore
    runs on rio-general too (16Mi waste); builder/fetcher pods stay
    off rio-general via their own node-role selector + taint, and the
    NodeOverlay only advertises synthetic capacity for builder/fetcher,
    so cold-start bin-packing is unchanged.

  - NO ConfigMap volume. Static pods can't depend on Helm-managed
    ConfigMaps in other namespaces (cross-ns mount forbidden) and
    shouldn't block on apiserver reachability at node boot. The
    initContainer writes conf.yaml to an emptyDir from the inlined
    rio.devicePluginConf helper. The smarter-device-manager image is
    alpine-based (has /bin/sh), so no extra image pull — same digest-
    pinned image for init + main, one pull.
*/}}
{{- define "rio.devicePluginPodManifest" -}}
{{- $dp := .Values.devicePlugin -}}
apiVersion: v1
kind: Pod
metadata:
  name: rio-device-plugin
  namespace: kube-system
  labels:
    app.kubernetes.io/name: rio-device-plugin
    app.kubernetes.io/part-of: rio-build
spec:
  priorityClassName: system-node-critical
  hostNetwork: true
  terminationGracePeriodSeconds: 30
  initContainers:
    - name: write-config
      # smarter-device-manager image is distroless (no /bin/sh — found
      # the hard way: Init:RunContainerError on first deploy). Reuse
      # rio-seccomp-bootstrap (busybox-based, ALREADY pulled at boot by
      # the bootstrap-container stanza above this in karpenter.yaml
      # userData) so this init adds zero image-pull latency.
      image: {{ include "rio.image" (list . "rio-seccomp-bootstrap") }}
      command: ["/bin/sh", "-ec"]
      args:
        - |
          cat > /config/conf.yaml <<'EOF'
          {{- include "rio.devicePluginConf" . | nindent 10 }}
          EOF
      volumeMounts:
        - name: config
          mountPath: /config
  containers:
    - name: smarter-device-manager
      image: {{ $dp.image }}
      securityContext:
        privileged: true
        allowPrivilegeEscalation: true
      resources:
        requests:
          cpu: 10m
          memory: 16Mi
        limits:
          cpu: 100m
          memory: 32Mi
      volumeMounts:
        - name: device-plugin
          mountPath: /var/lib/kubelet/device-plugins
        - name: dev
          mountPath: /dev
        - name: config
          mountPath: /root/config
  volumes:
    - name: device-plugin
      hostPath:
        path: /var/lib/kubelet/device-plugins
    - name: dev
      hostPath:
        path: /dev
    - name: config
      emptyDir: {}
{{- end -}}
