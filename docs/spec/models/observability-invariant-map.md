# Observability invariant map — metric-series ownership (C3)

Workstream record for the bughunt-wave `C3 metric-ownership` series
(closes bug_322 S, merged_bug_025 S+R, bug_310 S, bug_245 S+R,
bug_357 S+R). The class: metric series whose lifecycle (birth, reset,
freshness) was owned by *incidental call sites* — an alert evaluated an
absent series until the first event; a leadership loss reset a
hand-maintained gauge subset; a leadership edge had an acquire effect
with no lose writer; a gauge's only refresh rode an RPC whose periodic
caller was retired; a renamed semantic kept its stream-era reading in
the UI. Each invariant below names the enforcing type and test — the
ownership is structural, not conventional.

## O-1 — alert-counter birth

Every counter referenced by a PrometheusRule/ScaledObject `expr:` is
born at zero on every replica at boot, with every value of its closed
label set seeded individually.

- Rule: `obs.metric.alert-counter-seeded` (observability.typ).
- Type: the per-crate `ALERT_SEEDED_COUNTERS: &[SeededSeries]` tables
  (`rio-scheduler/src/observability.rs`, `rio-store/src/lib.rs`),
  applied from each crate's `describe_metrics()` tail (the exporter is
  installed immediately before — `rio_common::server::run` ordering).
- Test: `tests/alert_metrics.rs` in both crates —
  `rio_test_support::metrics::assert_alert_metrics_covered` parses the
  live helm templates' `expr:`/`query:` blocks (line-state-machine; the
  templates are helm-templated non-YAML), classifies every referenced
  name through a kind-aware describe recorder, and fails any counter
  not in the seed table, any exact label matcher outside the seeded
  product, and any name `describe_metrics()` never declares. The
  templates ride the nextest filesets (nix/lib/nextest-args.nix) so the
  check is sandbox-true.
- Birth site is `describe_metrics()`, NOT actor/server construction:
  boot scrape-surface is a process property; the standby/leader gauge
  tests' touch-sets stay actor-clean.

## O-2 — leader-family single source

The scheduler's leader-published state gauges form one declaration
(`leader_gauges!` → `LeaderGauge`): name + closed label axis +
per-member reset value. Publishing goes through the typed accessors;
the loss sweep and the boot seed are derived from `ALL` — a member
added to the family cannot be missed by either.

- Rule: `obs.metric.scheduler-leader-gate+5`.
- Reset values are per-member because zero is wrong for ratio gauges:
  `sla_prior_divergence` resets to 1.0 (in-band neutral) — a 0.0 sweep
  would itself fire `RioSlaPriorDivergenceClamped` (`<= 0.5`) on every
  failover.
- Tests: `leader_lost_resets_every_leader_gauge` (family-driven
  sentinel sweep, labeled members included),
  `describe_metrics_births_leader_gauges_at_reset`, and the
  re-pointed ex-leader/open-attempts tests (actor/tests/misc.rs).
- Single ownership, both directions
  (`raw_gauge_emits_are_exactly_the_emptions` — see test for exact
  name): every raw `metrics::gauge!` literal in rio-scheduler src is a
  declared per-replica exemption, and every exemption row still has an
  emit. Family members carry NO per-site literals (the declaration is
  the only place the name exists), so bypassing the accessors is
  grep-caught.

### Per-replica gauge exemptions (with rationale)

| gauge | rationale |
|---|---|
| `rio_scheduler_actor_mailbox_depth` | per-replica by design: each replica's own mailbox; a standby's depth is real signal |
| `rio_scheduler_sla_hw_cost_stale_seconds` | per-replica BY SPEC (climbs while standby under spot); zeroing on lose would mask what it measures |
| `rio_scheduler_sla_class_ceiling_uncatalogued` | config-derived constant, identical on every replica, no leader edge |
| `rio_scheduler_status_outbox_depth` | own-edge-owned: `clear_persisted_state()` zeroes it with the outbox it measures (every clear caller, not just LeaderLost); family membership would double-own the reset |

## O-3 — paired leadership edges

`handle_leader_acquired` and `handle_leader_lost` iterate the same
`LEADER_EDGES` table; an acquire-side effect cannot merge without its
lose cell written (fn-pointer struct fields — no-ops are explicit and
named).

- The bug_310 cell: the cost-table edge-reload latch had an acquire
  consumer (`cost_reload_notify`) and no lose writer — an A→B→A lease
  flap inside one 600s housekeeping tick left `cost_was_leader` true,
  the prelude skipped the reload, and the tick body persisted the
  deposed tenure's prices. The lose cell stores `false` directly
  (NOT a notify: `Notify` coalesces — lose+reacquire permits collapse
  into one wake observing `was_leader == true`, re-creating the bug;
  the false-store is wake-timing independent and monotone-safe).
- Tests: `leader_lost_writes_cost_latch_false` (red-first),
  `leader_edges_acquire_cells_fire`, composing with the existing
  `r[verify sched.sla.cost-leader-edge-reload]` prelude tests into:
  *the first leader tick after ANY acquire edge reloads before
  persist*.

## O-4 — store gauge self-publication

Every `rio_store_*` gauge is periodically self-published by the store
process from its owning data source; an RPC handler MAY mirror on
call, MUST NOT be a gauge's only writer.

- Rule: `obs.metric.store-gauge-ownership`.
- Type: `spawn_store_gauge_tick(pool, gate, shutdown)` →
  `publish_store_gauges` (ONE periodic publisher), plus
  `AdmissionPermit` whose `Drop` releases the permit then republishes
  utilization — the admission gate owns BOTH edges of its gauge
  (bug_245: acquire-only froze 1.0 after the burst drained; GetLoad's
  retired ComponentScaler caller had been the only periodic refresh —
  the same orphan class that froze the PG-pool panel at the CR
  removal). Concurrent drops may transiently overstate by one permit;
  the 30s tick heals it — locking deliberately rejected.
- Tests: `permit_drop_republishes_utilization` (red-first: frozen-1.0
  captured), `store_gauge_tick_publishes_without_get_load` (red-first:
  pg-only tick captured), `get_load_tracks_*` (the mirror stays), and
  vm-substitute-scale subtest 4b (decay to 0.0 within 45s of drain —
  the end-to-end frozen-gauge regression net).

## bug_357 — semantic re-point (A5)

`ExecutorInfo.last_heartbeat`/`connected_since` (both carrying
attempt-open time) → one `attempt_opened` field, renamed in place
(wire-stable, field 8; 9 reserved). The producer contract forbids
client-side staleness thresholds: the timestamp never advances
mid-build, so the dashboard's stream-era ">30s = dead executor"
highlight inverted into "every long build screams red".

**Rejected: a deadline-keyed highlight.** ExecutorInfo carries no
deadline; any client threshold re-creates the inverted-signal class
one knob over. The wedged-pod signal is owned by the OA2 alert (able
to fire from boot per O-1) and the controller's Job census. The page
and CLI show plain relative age.

## Rejected formal models (the directive-2 record)

Two candidate models were weighed and rejected — enforcement landed as
types + CI tests instead:

1. **Extending leaderElection.qnt with the cost-latch protocol** —
   wrong layer: the latch is in-process plumbing strictly downstream
   of the verified election; its only interleaving freedom (wake
   timing of the housekeeping select) is exactly what the false-store
   design makes irrelevant. The mode-invariant unit test + the
   existing prelude test pin the composed property at the right
   layer.
2. **A fresh costReload.qnt** — would verify the 4-state latch
   machine the LEADER_EDGES table makes structurally total; the model
   would re-state the table row by row (a description, not a check).

Series-lifecycle plumbing (Prometheus birth semantics, metrics-rs
registration, RPC-caller topology) has no adversarial interleaving the
registry types do not foreclose; the binding CI enforcement is the
parity tests + the bidirectional gauge-policy test + the family-driven
sweeps above.

## Adoption note (controller/gateway)

`rio_controller_*`/`rio_gateway_*` counters referenced by alerts are
other workstreams' rosters; the parity helper is parameterized
(`assert_alert_metrics_covered(paths, prefix, describe_fn, ...)`) and
adopting a crate is one test file + its seed table. Until adopted, a
new controller/gateway alert over an unseeded counter is NOT
mechanically caught — the helper exists, the wiring is this map's
recipe.
