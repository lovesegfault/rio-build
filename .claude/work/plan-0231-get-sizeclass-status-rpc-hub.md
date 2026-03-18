# Plan 0231: GetSizeClassStatus RPC — HUB

phase4c.md:20 — the admin RPC that exposes the live SITA-E state: effective vs configured cutoffs, per-class queue depth, sample counts. **This is the Y-join hub.** Three downstream plans consume it: [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) (WPS autoscaler reads it), [P0236](plan-0236-rio-cli-cutoffs.md) (CLI prints it), [P0237](plan-0237-rio-cli-wps.md) (CLI shows effective cutoffs in WPS describe). If this plan stalls, the entire right half of phase 4c stalls.

**types.proto hot-file alert.** Collision count = 27 (HOTTEST proto file). [P0205](plan-0205-proto-heartbeat-store-degraded.md) and [P0214](plan-0214-per-build-timeout.md) both touch it in 4b. This plan MUST serialize after both. **EOF-append only** — new messages go at the end of the file, never inserted mid-stream.

Pattern: `GetSizeClassSnapshot` `ActorCommand` variant mirroring `ClusterSnapshot` at [`command.rs:199`](../../rio-scheduler/src/actor/command.rs). O(n) ready-queue scan is fine — this RPC is operator/autoscaler-facing, not hot path.

## Entry criteria

- [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) merged (RwLock wired — handler reads `size_classes.read()` for `effective_cutoff`)
- [P0205](plan-0205-proto-heartbeat-store-degraded.md) merged (types.proto serialized — `HeartbeatRequest.store_degraded` field in place)
- [P0214](plan-0214-per-build-timeout.md) merged (types.proto serialized — timeout fields in place)

## Tasks

### T1 — `feat(proto):` SizeClassStatus message + RPC

MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) — **EOF-append ONLY**:

```protobuf
// SITA-E size-class status snapshot. Returned by GetSizeClassStatus.
message SizeClassStatus {
  string name = 1;
  double effective_cutoff_secs = 2;  // current (post-rebalancer) cutoff
  double configured_cutoff_secs = 3; // static TOML cutoff (for drift visibility)
  uint64 queued = 4;                 // derivations in ready-queue for this class
  uint64 running = 5;                // derivations currently assigned to workers of this class
  uint64 sample_count = 6;           // build_samples rows in the 7-day window
}

message GetSizeClassStatusRequest {}

message GetSizeClassStatusResponse {
  repeated SizeClassStatus classes = 1;
}
```

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto) — add RPC after `:36`:

```protobuf
rpc GetSizeClassStatus(GetSizeClassStatusRequest) returns (GetSizeClassStatusResponse);
```

### T2 — `feat(scheduler):` ActorCommand variant

MODIFY [`rio-scheduler/src/actor/command.rs`](../../rio-scheduler/src/actor/command.rs) — near `:199` `ClusterSnapshot`:

```rust
GetSizeClassSnapshot {
    reply: oneshot::Sender<Vec<SizeClassSnapshot>>,
},

// in the companion types:
pub struct SizeClassSnapshot {
    pub name: String,
    pub effective_cutoff_secs: f64,
    pub configured_cutoff_secs: f64,
    pub queued: u64,
    pub running: u64,
}
```

MODIFY [`rio-scheduler/src/actor/mod.rs`](../../rio-scheduler/src/actor/mod.rs) — match arm for the new variant:

```rust
ActorCommand::GetSizeClassSnapshot { reply } => {
    let classes = self.size_classes.read();
    let snapshots: Vec<_> = classes.iter().map(|c| {
        let queued = self.ready_queue.iter()
            .filter(|d| d.assigned_class == c.name)
            .count() as u64;
        let running = self.running.values()
            .filter(|d| d.assigned_class == c.name)
            .count() as u64;
        SizeClassSnapshot {
            name: c.name.clone(),
            effective_cutoff_secs: c.cutoff_secs,
            configured_cutoff_secs: self.initial_config.get(&c.name).map(|i| i.cutoff_secs).unwrap_or(c.cutoff_secs),
            queued, running,
        }
    }).collect();
    let _ = reply.send(snapshots);
}
```

O(n) scan where n = ready-queue depth. Fine for an admin RPC.

### T3 — `feat(scheduler):` admin handler + wire

NEW `rio-scheduler/src/admin/sizeclass.rs`:

```rust
pub async fn get_size_class_status(
    actor_tx: &ActorTx,
    db: &SchedulerDb,
) -> Result<GetSizeClassStatusResponse, Status> {
    let (tx, rx) = oneshot::channel();
    actor_tx.send(ActorCommand::GetSizeClassSnapshot { reply: tx }).await
        .map_err(|_| Status::unavailable("actor unavailable"))?;
    let snapshots = rx.await.map_err(|_| Status::internal("snapshot dropped"))?;

    // sample_count from DB (not in-actor state)
    let sample_counts = db.query_sample_counts_by_class().await
        .unwrap_or_default();  // best-effort; 0 on error

    let classes = snapshots.into_iter().map(|s| SizeClassStatus {
        name: s.name.clone(),
        effective_cutoff_secs: s.effective_cutoff_secs,
        configured_cutoff_secs: s.configured_cutoff_secs,
        queued: s.queued,
        running: s.running,
        sample_count: sample_counts.get(&s.name).copied().unwrap_or(0),
    }).collect();

    Ok(GetSizeClassStatusResponse { classes })
}
```

MODIFY [`rio-scheduler/src/admin/mod.rs`](../../rio-scheduler/src/admin/mod.rs) — wire the handler into the `AdminService` impl.

### T4 — `test(proto):` roundtrip test

Proto roundtrip test (in `rio-proto` or `rio-scheduler` tests):

```rust
#[test]
fn sizeclass_status_proto_roundtrip() {
    let orig = GetSizeClassStatusResponse {
        classes: vec![SizeClassStatus {
            name: "small".into(),
            effective_cutoff_secs: 62.5,
            configured_cutoff_secs: 60.0,
            queued: 5, running: 3, sample_count: 142,
        }],
    };
    let bytes = orig.encode_to_vec();
    let decoded = GetSizeClassStatusResponse::decode(&bytes[..]).unwrap();
    assert_eq!(orig, decoded);
}
```

This catches proto syntax errors fast — `.#ci` includes proto regen so a malformed message fails at build, but a roundtrip test catches semantic drift.

## Exit criteria

- `/nbr .#ci` green (includes proto regen — syntax errors fail here)
- `sizeclass_status_proto_roundtrip` passes
- `grpcurl -plaintext <scheduler> rio.admin.AdminService/GetSizeClassStatus` on a fresh scheduler returns all configured classes with `sample_count: 0`

## Tracey

No markers — this is plumbing (the hub). The consumers ([P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md), [P0236](plan-0236-rio-cli-cutoffs.md)) carry the behavior markers.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: SizeClassStatus + Get*Request/Response messages — EOF-APPEND ONLY (count=27, serialized after P0205+P0214)"},
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T1: GetSizeClassStatus rpc after :36"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "T2: GetSizeClassSnapshot variant near :199 ClusterSnapshot pattern"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T2: match arm — O(n) ready-queue scan; serialized after P0230"},
  {"path": "rio-scheduler/src/admin/sizeclass.rs", "action": "NEW", "note": "T3: get_size_class_status handler"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T3: wire handler into AdminService"}
]
```

```
rio-proto/proto/
├── types.proto           # T1: EOF-append messages (count=27 HOTTEST)
└── admin.proto           # T1: rpc decl
rio-scheduler/src/
├── actor/
│   ├── command.rs        # T2: variant
│   └── mod.rs            # T2: match arm (serial after P0230)
└── admin/
    ├── sizeclass.rs      # T3 (NEW): handler
    └── mod.rs            # T3: wire
```

## Dependencies

```json deps
{"deps": [230, 205, 214], "soft_deps": [], "note": "HUB — Y-join blocker. 3 downstream (P0234, P0236, P0237). types.proto count=27 HOTTEST — EOF-append only, serialized after P0205+P0214. Dispatch with elevated priority once P0230 merges."}
```

**Depends on:** [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) — reads `size_classes.read()` for `effective_cutoff`. [P0205](plan-0205-proto-heartbeat-store-degraded.md) + [P0214](plan-0214-per-build-timeout.md) — **types.proto serialization** (both 4b plans touch the same file).
**Conflicts with:** types.proto count=27 — EOF-append only, hard-serialized. admin.proto count=2, low risk. actor/mod.rs — serialized after P0230 via dep.

**Hidden check at dispatch:** `git log -p -- rio-proto/proto/types.proto | head -50` — confirm P0205/P0214 both merged and no trailing conflict markers.
