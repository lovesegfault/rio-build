# Controller probe hardening (D-054-1a — the live_054 close, made
# durable in the chart).
#
# live_054: the controller's /healthz is an axum task on the SAME
# tokio runtime as the reconcilers. Under admitted overload the
# runtime stalled 13-21s; at the chart's then-default probe posture
# (timeoutSeconds 1, failureThreshold 3, no startupProbe) kubelet
# read the sensor silence as death and killed a healthy singleton 5
# times mid-incident — each restart re-firing the cold-start load
# that caused the stall. The rev-5 hardening was applied as a kubectl
# patch; helm's three-way merge REVERTS chart-rendered drift on the
# next upgrade (measured during live-ops), so the patch dies unless
# the chart carries the same block. This fragment pins the chart
# block to the rev-5 live values:
#
#   liveness  timeoutSeconds 10, failureThreshold 6 (60s of silence
#             tolerated before a kill verdict), periodSeconds 10
#   startup   /healthz mirror, 30 x 2s, timeoutSeconds 10 (60s boot
#             budget before liveness arms)
#   readiness timeoutSeconds 5 (sheds traffic, never kills — may stay
#             tighter than liveness by design)
#
# Guard-domain honesty: a kill-wired probe served from the guarded
# runtime stays structurally blind (round-9 banner B, D3); this block
# converts brief starvation from kill-loop into delay until the
# isolation work lands.

out=$TMPDIR/controller-probes.yaml
helm template rio . --set global.image.tag=test >"$out"

probe() {
  yq -N "select(.kind==\"Deployment\" and .metadata.name==\"rio-controller\")
         | .spec.template.spec.containers[0].$1" "$out"
}

# Liveness: /healthz, 10s period, 10s timeout, 6 failures before kill.
lp_path=$(probe 'livenessProbe.httpGet.path')
lp_to=$(probe 'livenessProbe.timeoutSeconds')
lp_ft=$(probe 'livenessProbe.failureThreshold')
lp_ps=$(probe 'livenessProbe.periodSeconds')
test "$lp_path" = "/healthz" -a "$lp_to" = "10" -a "$lp_ft" = "6" -a "$lp_ps" = "10" || {
  echo "FAIL: controller livenessProbe = path=$lp_path timeout=$lp_to ft=$lp_ft period=$lp_ps, expected /healthz/10/6/10 (D-054-1a rev-5 values)" >&2
  exit 1
}

# Startup: mirrors /healthz, 30 x 2s with 10s timeout — boot budget
# before the liveness clock arms.
sp_path=$(probe 'startupProbe.httpGet.path')
sp_ft=$(probe 'startupProbe.failureThreshold')
sp_ps=$(probe 'startupProbe.periodSeconds')
sp_to=$(probe 'startupProbe.timeoutSeconds')
test "$sp_path" = "/healthz" -a "$sp_ft" = "30" -a "$sp_ps" = "2" -a "$sp_to" = "10" || {
  echo "FAIL: controller startupProbe = path=$sp_path ft=$sp_ft period=$sp_ps timeout=$sp_to, expected /healthz 30x2s timeout 10 (D-054-1a)" >&2
  exit 1
}

# Readiness: /readyz with a 5s timeout. Readiness is shed-wired (a
# failure only removes the pod from Endpoints), so it stays tighter
# than the kill-wired liveness budget.
rp_path=$(probe 'readinessProbe.httpGet.path')
rp_to=$(probe 'readinessProbe.timeoutSeconds')
test "$rp_path" = "/readyz" -a "$rp_to" = "5" || {
  echo "FAIL: controller readinessProbe = path=$rp_path timeout=$rp_to, expected /readyz timeout 5" >&2
  exit 1
}

echo "OK"
