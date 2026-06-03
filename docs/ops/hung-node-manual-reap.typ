#import "/lib/rio.typ": *
#show: rio.with(domains: none)

Pull-mode executor pods do not register or heartbeat: liveness is the Job's
lifecycle, and the only scheduler-side time-based repair is the establishment
sweep (an open attempt past its intent deadline plus the report slack is
established as an unreported executor crash and requeued). A node whose
kubelet or container runtime wedges while the Node object stays `Ready` is
therefore invisible to the heartbeat-fed hung-node detector for pull-mode
attempts: its builds simply run out their deadlines one by one. The
controller-side per-node deadline-clustering aggregation (the permanent OA2
signal, landed at executor-lifecycle slice 1c) automates exactly the
clustering described below — a node accumulating expired open attempts for
two or more distinct derivations inside the 30-minute window is marked
Dead-equivalent (#(refs.metric)("rio_controller_node_wedge_marked_total"))
and reaped through the existing capped NodeClaim Dead arm. The
`RioSchedulerAttemptEstablishmentCluster` alert plus this runbook remain the
independent operator-facing tripwire and the manual confirmation/fallback
procedure (OA2 option C's compensating controls, kept as deliverables).

= When the alert fires

The alert is a fleet-wide tripwire: two or more establishment events inside
30 minutes (#(refs.metric)("rio_scheduler_attempt_requeue_seconds") with
`cause="establishment"`). Establishments are expected to be rare — a single
one usually means one pod crashed without reporting (node OOM, forced
deletion) and the sweep did its job. A burst is what needs triage: it is
either a genuinely wedged node (the OA2 signature) or a systemic cause
(scheduler unable to serve `ReportOutcome`, store outage failing every build
at once).

The controller's automated clustering now applies this discrimination
itself (#rref("ctrl.nodeclaim.wedge-two-axis")): when more than half of the
attributed build fleet is past the expiry threshold in one tick it marks
NOTHING and increments
#(refs.metric)("rio_controller_wedge_systemic_suppressed_total") instead of
rolling Dead-reaps across the fleet. A non-zero suppression counter is the
automation telling you to run THIS runbook's systemic triage; per-node
wedges keep flowing to the Dead arm without operator action. Evidence is
also build-class only and anchored at first observation, so store-side
materialization fetches and one stuck derivation re-observed for hours no
longer pollute the per-node signal.

+ Confirm the burst is real: the alert value is the count over the window;
  #(refs.metric)("rio_scheduler_open_attempts") shows how many attempts are
  still open right now.

+ Run the per-node clustering query (the ledger is the source of truth — the
  establishment histogram is not labeled by node). On the scheduler
  PostgreSQL:

  ```sql
  SELECT e.source_node,
         count(DISTINCT a.derivation_id) AS distinct_drvs,
         count(*)                        AS establishments,
         min(a.recorded_at)              AS first_seen,
         max(a.recorded_at)              AS last_seen
  FROM drv_attempts a
  JOIN drv_executions e ON e.exec_id = a.exec_id
  WHERE a.outcome_class = 'executor_crash'
    AND a.termination_reason = 'unreported'
    AND a.recorded_at > now() - interval '30 minutes'
  GROUP BY e.source_node
  ORDER BY distinct_drvs DESC;
  ```

  `source_node` is the controller-authoritative binding persisted by the pull
  transaction; `NULL` rows are attempts whose binding was never observed
  (treat a cluster of NULLs as "not node-attributable" — investigate the
  scheduler/store first, not a node).

+ Two or more *distinct derivations* establishing on *one* node inside the
  window is the hung-node signature. One derivation establishing repeatedly
  is a build/derivation problem (look at its history), not a node problem.

= Manual NodeClaim reap (the confirmed-wedge response)

The blast radius is already bounded: every establishment charged the failing
node through the source-keyed exclusion, so retries avoid it. The node still
holds capacity and any still-open attempts on it will keep running out their
deadlines, so reap it once confirmed:

+ Identify the NodeClaim that owns the node:
  `kubectl get nodeclaims -o wide | grep <source_node>`

+ Cordon the node so nothing new lands while pods drain:
  `kubectl cordon <source_node>`

+ Delete the NodeClaim (Karpenter drains and replaces it; the finalizer
  handles instance termination):
  `kubectl delete nodeclaim <nodeclaim-name>`

  If the apiserver-side deletion hangs because the kubelet cannot ack pod
  eviction, force-delete the stuck pods
  (`kubectl delete pod <p> --force --grace-period=0`) and, as the last
  resort, terminate the instance at the cloud provider — the NodeClaim
  finalizer reconciles afterwards.

+ Verify the replacement: a fresh NodeClaim goes `Launched → Registered →
  Ready`, and the requeued derivations (the establishment already requeued
  them) build elsewhere. The exclusion keeps them off the dead node's name
  even if it briefly reappears.

= Rules while operating on this signal

- *Never reap a `Ready` node manually on this alert alone.* The alert is
  fleet-wide and over-approximates; only the per-node ledger query (or the
  controller-side aggregation's own marking, which applies the same
  two-distinct-derivations discrimination and is bounded by the per-tick
  dead-reap cap) justifies touching a node. Bulk-reaping Ready nodes on a
  noisy tripwire converts a latency problem into a capacity outage.
- While the alert is quiet, no manual action is owed: single establishments
  are the sweep working as designed.
- Karpenter NodeRepair only covers nodes whose `Ready` condition goes
  `False`/`Unknown` past its toleration window. The wedged-but-Ready failure
  mode this runbook exists for is exactly the case NodeRepair does *not*
  cover — do not assume it will fire.
- Tuning: the thresholds (2 establishments / 30 minutes) are deliberately
  sensitive while the fleet has few pull-mode pools. If the alert becomes
  noisy from unrelated systemic causes, fix the systemic cause; only retune
  the thresholds with a recorded justification (deployment-time validation
  checklist row D3).
