#import "/lib/rio.typ": *
#show: rio.with(domains: none)


Deployment checklist for the gateway connection/session lifecycle (the
gw-session campaign's operator handoff, relocated from the retired
gw-session invariant map). The campaign added no deployment gates
(verify-only close-out); these are the operational consequences of the
accepted residuals and the design-checkpoint decisions, for whoever first
deploys the gateway. Each row's window bound is machine-pinned by a wired
quint check; the bounds below are the operator-facing halves.

= GW-D1 — TCP_USER_TIMEOUT setsockopt-failure alert

Alert on the `set TCP_USER_TIMEOUT failed` warn line
(`rio-gateway/src/server/mod.rs`). The W7 acceptance rests on the premise
that this setsockopt effectively cannot fail on the Linux/EKS target — the
alert is the check on that load-bearing premise. There is no `errors_total`
label for it (the optional label was not commissioned); the warn line is
the only signal. If it fires: parked-write reclamation degrades from the
designed \~305 s to the \~1 h inactivity backstop, and the out-of-model
sliver (slow-output + zero-window-ACKing peer) is unbounded in-process
until restart.

= GW-D2 — `channels_active` autoscaling caveat

`rio_gateway_channels_active` (the autoscaling signal) momentarily
under-counts winding-down sessions during mass disconnects with an
unresponsive scheduler — the W2 guard-held divergence, ≤ \~30 s per session
(the cancel-loop bound; structurally bounded — the diverged proto task can
only occupy `cancelling`, whose exit needs no peer, upstream, or timer
cooperation). Do not tune autoscaler reactions tighter than that window;
treat dips during mass-disconnect events as expected.

= GW-D3 — memory headroom for NAR buffering

Per-connection worst-case memory is NOT bounded by the lifecycle caps: a
half-open vanish (W5) or a slow client can pin \~512 KiB of duplex buffers
plus a NAR assembly buffer (up to `MAX_NAR_SIZE` = 4 GiB) for the full
\~300--330 s reap window, per occurrence — alongside one conn permit (of
1000), one session permit (of 4096), the gauge slots, and the in-flight
scheduler/builder work, until the designed transport reap fires. Size pod
memory limits for expected concurrent uploads × realistic NAR sizes on top
of the steady-state footprint (the W8 amplification has no in-process cap;
the existing 4 GiB pod limit was sized for drv_cache, not concurrent NAR
uploads).

= GW-D4 — conn-permit occupancy alert

Alert on sustained `rio_gateway_errors_total` `conn_cap` growth.
Conn-permit-at-accept is a fixed fact (owner decision B4): probes and
SYN-flood-with-completion can transiently hold conn permits with no
auth-level signal, indistinguishable in-process from legitimate load.

= GW-D5 — SIGKILL / terminationGracePeriodSeconds

SIGKILL is outside the verified envelope (owner decision B2). Set the
gateway pod's `terminationGracePeriodSeconds` ≥ the drain budget
(accept-stop + session-drain timeout + 5 s `CANCEL_GRACE`) so the verified
three-stage drain is what actually runs on eviction; the scheduler orphan
watcher and store backstops are the named assumptions behind anything a
SIGKILL interrupts.

= The four accepted windows behind these rows

- *W2 — guard-vs-proto-task divergence* (owner decision B1, guard-held
  accounting): the gauge under-count of GW-D2; pinned by the
  server-side-release witness check.
- *W5 — half-open vanish occupancy:* the occupancy bound of GW-D3 (a bound,
  not a violation); pinned by the vanish-reclaimed witness check.
- *W7 — degraded-tier reclamation:* with the setsockopt failed, reclamation
  of a parked-write connection degrades to the \~1 h inactivity backstop;
  the gating premise is GW-D1's observable; pinned by the
  l1-no-inactivity falsification probe.
- *W10 — non-Wire removal orphan:* a non-Wire stream failure with a dead or
  absent client orphans a running build for up to \~300 s until the
  scheduler orphan watcher cancels it — bounded by a named environment
  assumption (the watcher), not by the gateway's own checks; pinned by the
  s16-terminal-only falsification probe.
