# Audit Batch B2 Decisions — 2026-03-18 (multi-plan cascades)

Items 15-23 from `.claude/notes/plan-decision-audit.md`. 6 bugs + 3 design choices.

## Bug fixes (confirmed, no alternative)

| # | Plan | Bug | Fix |
|---|---|---|---|
| 17 | **P0251** | Loop `state.ca_output_unchanged = matched` overwrites per output — last wins. outputs=[miss, match] → true → skip → WRONG | AND-fold: `state.ca_output_unchanged = results.iter().all(\|r\| *r)` |
| 18 | **P0232** | `serde_json::Value` for resources breaks P0233:38 `.clone()` into `Option<ResourceRequirements>` | `pub resources: ResourceRequirements` (typed + `schema_with=any_object`, matching workerpool.rs:70 pattern; non-Option — classes need distinct profiles) |
| 19 | **P0241** + **P0220** | jq `.origin=="P0220"` never matches — origin Literal is reviewer\|consolidator\|… Silent fall-through. | `jq 'select(.source_plan=="P0220")'` + encode outcome in `description` (zero schema change) |
| 20 | **P0277** | `createConnectTransport` = Connect protocol, NOT gRPC-Web. Envoy's grpc_web filter doesn't translate it → malformed HTTP/1.1 at scheduler | `createGrpcWebTransport` (same package, one word) |
| 22 | **P0229** | Dedupe breaks P0230's zip (truncates → stale cutoffs). Test already asserts no-op. | Delete dedupe/clamp options; `assignment.rs:102` already tolerates equal cutoffs (stable sort + first-match) |
| 23 | **P0232** | ClassStatus missing `ready_replicas` vs phase4c.md:25 spec. Ready/Desired is THE operator signal. | Add `ready_replicas: i32` (6 fields total, keep `child_pool`) |

## Design decisions

| # | Plan | Issue | **Decision** | Delta |
|---|---|---|---|---|
| 15 | **P0238** | InputsResolved condition has NO proto source (BuildEvent oneof lacks it) | **Add `BuildEvent::InputsResolved` to proto.** Scheduler fires after closure substitution. Accept scope bleed — P0238 becomes multi-crate (proto + scheduler + controller). | P0238 files fence grows; types.proto gets new oneof variant |
| 16 | **P0270** | Plan targets admin.proto (has no messages). critpath needs scheduler DAG — only scheduler can compute. | **Both proto + CRD.** `BuildEvent::Progress` gets `critpath_remaining_secs` + `assigned_workers` (scheduler computes, emits). Controller `apply_event` mirrors to CRD (kubectl sees). Dashboard + rio-cli read from gRPC stream. 3 crates. | P0270 files fence: types.proto (not admin.proto) + rio-scheduler + crds/build.rs |
| 21 | **P0259** + **P0260** | P0259 takes bare `VerifyingKey` (moved by value). P0260 expects hot-swappable. | **Amend P0259: `Arc<RwLock<VerifyingKey>>` from the start.** P0259 UNIMPL — signature malleable. Touches 2 main.rs (per B1 #9). RwLock plenty for annual rotation. | P0259 interceptor signature change; P0260 works as written |

## #16 data flow (settled)

```
scheduler (computes critpath from DAG + ema)
   │
   └─► Progress event (proto) ──┬─► gRPC stream ──► dashboard, rio-cli
                                │
                                └─► controller apply_event ──► CRD status ──► kubectl
```

Scheduler is single source of truth. No derivation tracking in the controller.

## Batch C pending — single-plan items #24-29 (6 items)

Contained blast radius. Likely quick.
