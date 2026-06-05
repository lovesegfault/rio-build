# Authorization invariant map (bughunt-2 wave, slot 4)

Status: LANDED with the authz-matrix-completion workstream
(2026-06-04). Model: `authz.qnt` (self-contained — deliberately not
touching `logService.qnt`). Wired checks: 1 hold (4 invariants, TLC
exhaustive, Tier-1), 1 base-model non-vacuity witness, 4
expect-violation calibrations.

## What the model covers

`authz.qnt` — one authorization question per step, three families:

- **Transport layer** (rio-authz-kernel `decide()`): a credential
  class kind × a *bootable* verifier configuration (the
  `jwt ⇒ service ∧ hmac` coherence predicate excludes the refused boot
  states — they never serve, so no layer verdict is reachable in them)
  × a presented credential kind. The law is
  enforce-when-configured on the class's **declared** knob only.
- **TailLog ownership** (`rio_store::logs::tail::authorize_tail`):
  dev-mode × build-membership × the request-string poison input
  ("the caller-supplied derivation string would prefix-match something
  the caller owns"). The law never reads the string.
- **WatchBuild tenancy** (`rio-scheduler` actor arm + the tenant-bound
  `get_build_terminal_row`): dev-mode × caller-is-owner × lifecycle
  phase. ONE law for both phases.

Verdicts are **{admit, deny} only** — status-code shape is out of
model by design. The resident-phase existence oracle
(foreign+resident = `PermissionDenied` vs foreign+terminal =
`NotFound`) is the spec-pinned asymmetry the §5-S Q4 sign-off names as
a knowingly-signed residual; modeling codes would make
`lifecyclePhaseIndependence` falsely red against the signed design.

The test-tier mirror of the same laws is the owner-signed
`EXPECTED_LAYER` matrix (`rio-store/src/authz.rs`,
`composed_authz_matrix_layer_tier`: 625 verdict cells + 75
refused-state cells); the proof-tier mirror is the rio-authz-kernel
kani battery (foreign-knob independence over ALL 8 configurations,
including refused ones).

| invariant | finding | meaning |
|---|---|---|
| `enforcementFromDeclaredVerifiersOnly` | bug_237 | the layer verdict is a function of the class's declared knob alone: the assigned verdict equals the declared-knob law, and all bootable config pairs agreeing on the declared family agree on every verdict |
| `noUndeclaredAdmitWhenKeyed` | bug_237 dead leg / merged_bug_122 | a keyed class admits ONLY its declared credential kind — tenant claims never admit a Service method, an assignment header never admits a TenantJwt method |
| `lifecyclePhaseIndependence` | bug_213 | the WatchBuild admit/deny verdict for a (caller, owner) pair is identical in the resident and terminal/cleaned phases — the durable-row fallback observes the same tenant authority as the actor arm |
| `requestStringIndependence` | merged_bug_064 | the TailLog ownership verdict is a function of (dev-mode, build-membership) only — the request string appears in no ownership predicate |

## Non-vacuity witness (expect-violation on the BASE model)

| check | witness invariant | proves |
|---|---|---|
| `quint-authz-witness-advertised-leg` | `advertisedLegSilent` | the advertised legs really admit: a keyed class's declared credential is admitted somewhere in the sweep. An enforcement collapse into deny-everything would hold the two layer invariants vacuously — this turns that state into a red check |

## Calibrations (expect-violation, one frozen pre-fix law each)

| calibration | finding | frozen pre-fix law | falsifies |
|---|---|---|---|
| `authz-237-foreign-knob` | bug_237 | the Service arm keys on the JWT knob (the pre-kernel `ServiceOrTenant` leg) | `enforcementFromDeclaredVerifiersOnly` |
| `authz-122-dead-leg` | merged_bug_122 / bug_237 | tenant claims admit a KEYED Service method (recorded red: TenantClaims on TriggerGC → grpc-status None) | `noUndeclaredAdmitWhenKeyed` |
| `authz-213-phase` | bug_213 | the terminal arm ignores the caller (unbound `get_build_terminal_row`; recorded red: foreign watch post-cleanup streamed the settled verdict) | `lifecyclePhaseIndependence` |
| `authz-064-string` | merged_bug_064 | the ownership fallback consults the request string (the LIKE-prefix IDOR; recorded red: verbatim own-drv + foreign-pin admitted) | `requestStringIndependence` |

## Re-run commands

```
quint verify docs/spec/models/authz.qnt \
  --invariant enforcementFromDeclaredVerifiersOnly,noUndeclaredAdmitWhenKeyed,lifecyclePhaseIndependence,requestStringIndependence

quint verify --backend=tlc --main=authz \
  --invariant=advertisedLegSilent docs/spec/models/authz.qnt   # expect violation

quint verify --backend=tlc --main=authzCalib237ForeignKnob --step=calibStep \
  --invariant=enforcementFromDeclaredVerifiersOnly \
  docs/spec/models/calibration/authz-237-foreign-knob.qnt      # expect violation
# (analogous for authz-122-dead-leg / authz-213-phase / authz-064-string)
```

All six wired as `checks.x86_64-linux.quint-authz*` (see
`nix/quint.nix`).
