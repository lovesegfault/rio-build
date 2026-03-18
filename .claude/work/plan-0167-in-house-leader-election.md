# Plan 0167: In-house leader election — resourceVersion guard, observed-record expiry

## Design

Replaced `kube-leader-election` 0.43 with an in-house implementation modeled on client-go's `leaderelection` package. Triggered by the standby's 17k-lines/day INFO spam, but the load-bearing reason was two structural bugs in the crate:

1. **`acquire_lease()` used `Patch::Merge` with no `resourceVersion` precondition.** Two standbys whose poll ticks align both GET, both see expired, both PATCH, both get 200, both believe they acquired. Dual-leader until one discovers `holderIdentity` isn't theirs on the next renew. Not a partition edge case — happens whenever the leader crashes and standbys tick within ~100ms.

2. **`has_lease_expired()` compared local wall clock against the lease's `renewTime`** (written by the LEADER's clock). Standby >10s fast → perpetually thinks a healthy lease is expired.

**New `election.rs` (~300 LOC with tests, `54eb4f5`):** All mutations via `kube::Api::replace()` (PUT). Echoes the GET's `metadata.resourceVersion`; apiserver rejects with 409 if stale. Exactly one racer wins. **Observed-record expiry:** standby tracks `(holder, transitions)` + a local `Instant` when first seen; if unchanged for TTL of LOCAL monotonic time, steal. Cross-node skew irrelevant. 409 on renew → immediate lose transition (someone stole). 409 on steal → another standby raced and won. No log on the standby steady state.

**Fix (`cdb70c2`):** observed-record should track `resourceVersion`, not `(holder, tx)` — a leader that crashes and restarts with the same identity within TTL would look "unchanged" to the observed-record check but have a fresh resourceVersion.

Supporting work: `e7f85be` extracted `ApiServerVerifier` from `rio-controller` into `rio-test-support` for kube-mock testing; `4870971` added a phase3a VM assertion `leaseTransitions=0` on lease create; `c02e0af` bumped tracey (enabled `.nix` parsing — first time VM tests could carry `# r[verify ...]`); `58c0145` fixed a `workers_active` gauge race (stream-then-heartbeat path missing increment); `dc4c8ac`/`89750ca`/`29d6c0a` kubeconform hook churn (added then removed).

## Files

```json files
[
  {"path": "rio-scheduler/src/lease/election.rs", "action": "NEW", "note": "in-house: replace() with resourceVersion; observed-record local-Instant expiry; 409 handling"},
  {"path": "rio-scheduler/src/lease/mod.rs", "action": "MODIFY", "note": "wire new election.rs; kube-leader-election dep removed"},
  {"path": "rio-test-support/src/kube_mock.rs", "action": "NEW", "note": "ApiServerVerifier extracted from rio-controller"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "workers_active gauge: stream-then-heartbeat path adds missing increment (58c0145)"},
  {"path": "nix/tracey.nix", "action": "MODIFY", "note": "bump to main + real pnpm-built dashboard + .nix parsing (c02e0af)"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "assert leaseTransitions=0 on lease create"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "drop kube-leader-election dep"}
]
```

## Tracey

- `r[verify sched.lease.k8s-lease]` — `54eb4f5`
- `.nix` parser enabled in `c02e0af` → VM test markers become parseable (7 `# r[verify ...]` annotations in `c02e0af`'s diff as first users of the new parser)

8 marker annotations (1 Rust verify + 7 .nix verify annotations unlocked by the tracey bump).

## Entry

- Depends on P0166: balanced channel (the lease loop flips the same Arc<AtomicBool> that ensure_leader reads)
- Depends on P0114: phase 3a lease (this rewrites the implementation, same spec behaviors)

## Exit

Merged as `dc4c8ac`, `c02e0af`, `e7f85be`, `54eb4f5`, `4870971`, `89750ca`, `cdb70c2`, `29d6c0a`, `58c0145` (9 commits). `.#ci` green. Two standbys race-starting on leader crash: exactly one wins.
