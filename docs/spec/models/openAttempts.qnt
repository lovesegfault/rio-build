// -*- mode: Bluespec; -*-
// quint-policy-latches: driverLost, ackedUnsettled, ghostClaim, chargedNoFault, credentialClobbered, refusalFiledAsLost, confirmMinted, mintAfterGone
//
// openAttempts — one derivation's open pull-mode attempt, RE-STATED
// over the claim/settlement laws of the materialize-disposition wave
// (the §4-R2 rework; bughunt-2 bug_357 + bug_182 + merged_bug_055 +
// bug_251, on the bug_347 base).
//
// WHAT IS MODELED
//   The original cancel-closure plane (kept):
//     - the terminal status persist closing the assignment row, with
//       the leader-scoped status outbox re-driving a failed persist
//       ("latch on Ok only"; sched.attempt.cancel-close-driven), and
//     - the establishment sweep through the total kernel: a cancelled
//       or absent node closes CHARGE-FREE; only live-wanted work may
//       be charged. Failover is honest: the outbox clears with the
//       tenure; a non-durable cancel evaporates.
//   PLUS the wave's claim plane:
//     - the mint sets the job-view holder and PERSISTS a redelivery
//       credential (bug_251: the claim nonce lands in the same fenced
//       transaction as the mint);
//     - a mint whose RESPONSE is lost redelivers against that
//       credential un-charged (`resumeRedeliver`) — only a worker that
//       really crashed (credential ledger lost) settles through the
//       charged establishment window, the signed residual;
//     - the consumption close follows the ack law (bug_182): a
//       settled close acks and its companion releases the claim; a
//       FAILED close NACKs retryably and keeps the attempt open for
//       the store's bounded redelivery;
//     - a failed companion still RELEASES the claim
//       (merged_bug_055: claimable-but-unparked dominates
//       wedged-claimed-forever).
//
// bug_357 (the §4-R2 repair): the old `openAttemptHasDriver` was a
// propositional tautology — its consequent `sweepWouldCloseFree(node)`
// is constant `true` at `node == NCancelled`, so the invariant could
// not fail on ANY transition system. It is REPLACED by
// `notDriverLost`: `driverLost` is a live-computed latch seated at the
// transition that must arm the driver (`cancelFailApply` — the failed
// cancel persist MUST latch the outbox; the sweep is the backstop,
// never the driver), so the law is over the transition relation and
// the `openattempts-outbox-dropped` twin falsifies it.
//
// All four latches are computed LIVE by the oracle-seated sub-actions
// below — never by calibration overrides (the fencedWrites repair
// pattern; quint-policy P4/P5). Twins perturb DECISIONS only and route
// through the same seats:
//   openattempts-charge-blind      -> cancelledNeverChargedAsCrash
//   openattempts-outbox-dropped    -> notDriverLost
//   openattempts-ack-on-failed-close -> ackImpliesSettledOrArmed
//   openattempts-no-fallback-release -> claimedImpliesOpenAttempt
//   openattempts-nonceless-mint    -> noFaultNeverCharged
//   openattempts-clobbered-credential -> noCredentialClobber
//   openattempts-refusal-as-lost   -> noRefusalFiledAsLost
//   openattempts-minting-confirm   -> confirmNeverMints
//   openattempts-rotation-skew     -> noFaultNeverCharged
//   openattempts-unfenced-gone     -> noMintAfterGoneAnswer
//   (the merged_bug_072 budget law lives in openAttemptsBudget.qnt --
//    its own file; see the wedge note there)
//
// bughunt-3 S5 (the rule-4b client-loop repairs): three more laws on
// the claim plane, each seated like the four above —
//   - the fresh pass is mint-gated by the ledger (merged_bug_096): a
//     live credential is never clobbered by a second mint for the
//     same job (`freshClaimSeat`);
//   - a MINT-DISPROVING refusal resolves the credential (bug_119,
//     narrowed by bughunt-4 merged_bug_074: only InvalidArgument/
//     Unimplemented -- the request shape can never mint; auth-layer
//     codes judge the PRESENTATION and keep the entry via
//     `authRefusalSeat`): it is never filed as a lost response
//     (`answeredRefusalSeat`);
//   - a confirm-only probe is a READ (merged_bug_083): it can never
//     mint (`confirmProbeSeat` — the scheduler screens DeliverNew).
//
// bughunt-5 S4 (merged_bug_011): the FENCE plane -- a Gone answer is
// the token's exit-0 license and MUST be fence-written ahead of the
// reply (sched.executor.confirm-fence); the DeliverNew screen refuses
// mints for fenced tokens. goneAnswerSeat is the oracle seat (the
// write-ahead decision); mintApply computes the mintAfterGone latch
// live (a mint while the token holds a Gone license is the invisible
// orphan); resubmitReready models the content-addressed resubmit that
// re-readies the drv (the straggler chain's middle step). The
// unfenced-gone twin perturbs exactly the live-loop write-ahead
// decision (the as-built pre-fix arm).
//
// Tier-1 budget: 3 (node) × 4 (attempt) × 2^7 (booleans) × 2^8
// (latches) = upper bound only; TLC exhausts in seconds (state count
// re-measured at the quint-open-attempts entry comment).

module openAttempts {
  type NodeState = NWantedLive | NCancelled | NAbsent
  type AttemptState = ANone | AOpen | AClosedCharged | AClosedFree

  var node: NodeState
  var attempt: AttemptState
  var outbox: bool
  // Ghost: the cancel reached durability (persist Ok or flush Ok).
  var cancelDurable: bool
  // The claim plane (bug_251 / merged_bug_055):
  // The in-memory job-view holder is set.
  var viewClaimed: bool
  // The store still holds a redelivery credential for the open
  // attempt (resume exec id or persisted claim nonce).
  var clientHoldsResume: bool
  // The mint's response never reached the store (the lost-response
  // no-fault lane; cleared by a successful redelivery).
  var responseLost: bool
  // The fence plane (merged_bug_011):
  // The confirm-fence row for this token is durable.
  var goneFenced: bool
  // A Gone answer was issued to this token (its exit-0 license).
  var goneLicensed: bool

  // --- latches (live-computed; see the header directive) -------------------
  // The failed cancel persist did not arm its driver (bug_357).
  var driverLost: bool
  // A fresh mint replaced a LIVE credential (merged_bug_096's clobber).
  var credentialClobbered: bool
  // A MINT-DISPROVING refusal left the credential live -- filed as
  // a lost response (bug_119's immortal entry, scope narrowed to
  // disproving refusals by merged_bug_074).
  var refusalFiledAsLost: bool
  // A confirm-only probe minted a fresh attempt (merged_bug_083).
  var confirmMinted: bool
  // An ack was produced while the close was not settled (bug_182).
  var ackedUnsettled: bool
  // A close left the view claimed with no open attempt
  // (merged_bug_055's claimed-no-attempt ghost).
  var ghostClaim: bool
  // A charge landed on a no-fault (lost-response) attempt (bug_251).
  var chargedNoFault: bool
  // A mint opened AFTER this token's Gone answer (merged_bug_011's
  // invisible orphan -- an attempt no sweep can see).
  var mintAfterGone: bool

  action init = all {
    node' = NWantedLive,
    attempt' = ANone,
    outbox' = false,
    cancelDurable' = false,
    viewClaimed' = false,
    clientHoldsResume' = false,
    responseLost' = false,
    goneFenced' = false,
    goneLicensed' = false,
    mintAfterGone' = false,
    driverLost' = false,
    ackedUnsettled' = false,
    ghostClaim' = false,
    chargedNoFault' = false,
    credentialClobbered' = false,
    refusalFiledAsLost' = false,
    confirmMinted' = false,
  }

  // --- oracle seats ----------------------------------------------------------
  // Shared by live actions AND calibrations: the caller passes its
  // DECISIONS; the oracles compute the latch truths from them. A twin
  // that perturbs a decision is observed by the same seat.

  /// Mint seat: deliver the claim. `credentialPersisted` is the
  /// bug_251 decision (the nonce persists in the mint's fenced
  /// transaction); `lost` marks the response as never reaching the
  /// store. merged_bug_011 oracle: a mint while this token already
  /// holds a Gone license is the invisible orphan -- the latch is
  /// computed HERE, at the only transition that can open it (the
  /// callers' screen guard is what makes it unreachable live).
  action mintApply(credentialPersisted: bool, lost: bool): bool = all {
    attempt' = AOpen,
    viewClaimed' = true,
    clientHoldsResume' = credentialPersisted,
    responseLost' = lost,
    mintAfterGone' = or { mintAfterGone, goneLicensed },
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    // A fresh mint is backed by its open attempt: no ghost here.
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// Gone-answer seat (merged_bug_011): the scheduler answers this
  /// token Gone -- its exit-0 license. `writesFence` is the
  /// write-ahead decision: the live protocol fences EVERY keyed Gone
  /// before the reply (the law is uniform across the live-loop and
  /// confirm arms); the unfenced-gone twin flips it for the live
  /// loop (the as-built pre-fix decision).
  action goneAnswerSeat(writesFence: bool): bool = all {
    goneLicensed' = true,
    goneFenced' = or { goneFenced, writesFence },
    attempt' = attempt,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
    mintAfterGone' = mintAfterGone,
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// Settled-close seat: one SETTLED attempt close (consumption Ok,
  /// cancel persist, outbox flush, establishment sweep). The caller
  /// decides whether the close charges and whether the companion
  /// releases the claim; the oracles compute:
  ///   ghostClaim     — the view stayed claimed with no open attempt
  ///                    (the merged_bug_055 ghost);
  ///   chargedNoFault — a charge landed while the attempt's only
  ///                    "fault" was a lost response (bug_251).
  /// A settled close's ack is lawful (bug_182), so ackedUnsettled is
  /// framed here; its writer is the failed-close seat below.
  /// (Seat parameters are deliberately bool-only: a sum-type action
  /// parameter converts to a silently-disabled arm under quint→TLA —
  /// the constructor-reachability probes in the header guard this.)
  action closeSettledApply(charged: bool, releasesClaim: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    attempt' = if (charged) AClosedCharged else AClosedFree,
    viewClaimed' = (viewClaimed and not(releasesClaim)),
    ackedUnsettled' = ackedUnsettled,
    ghostClaim' = or {
      ghostClaim,
      viewClaimed and not(releasesClaim),
    },
    chargedNoFault' = or {
      chargedNoFault,
      charged and responseLost,
    },
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
    driverLost' = driverLost,
  }

  /// Failed-close seat: the close write did NOT settle — the attempt
  /// stays open, the claim is kept, and `acks` is the bug_182
  /// decision: the live action NACKs (false) so the store's bounded
  /// redelivery re-presents; the ack-on-failed-close twin flips it
  /// and the oracle latches ackedUnsettled.
  action closeFailedApply(acks: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    attempt' = attempt,
    viewClaimed' = viewClaimed,
    ackedUnsettled' = or { ackedUnsettled, acks },
    ghostClaim' = ghostClaim,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
    driverLost' = driverLost,
  }

  /// Cancel-failure seat (THE bug_357 restatement): the failed
  /// terminal persist's one law is that it ARMS the driver —
  /// `latchesOutbox` is the sched.attempt.cancel-close-driven
  /// decision. `driverLost` is computed HERE, at the transition that
  /// must arm the driver, not as a state implication whose consequent
  /// was constant.
  action cancelFailApply(latchesOutbox: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    node' = NCancelled,
    attempt' = attempt,
    outbox' = latchesOutbox,
    driverLost' = or { driverLost, not(latchesOutbox) },
    ackedUnsettled' = ackedUnsettled,
    ghostClaim' = ghostClaim,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// Fresh-claim seat (merged_bug_096): the fresh pass consults the
  /// ledger BEFORE minting. `mintsOverLive` is the perturbable
  /// decision -- the live client cannot take it (begin_fresh_claim
  /// refuses while an entry is live; the skip falls out of the API).
  /// The oracle: minting over a live credential is the clobber.
  action freshClaimSeat(mintsOverLive: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    credentialClobbered' = or { credentialClobbered, and { mintsOverLive, clientHoldsResume } },
    // The skip is a read -- no state moves besides the latch truth.
    attempt' = attempt,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// Answered-refusal seat (bug_119, re-scoped to MINT-DISPROVING
  /// refusals by merged_bug_074): the scheduler ANSWERED with a
  /// rejection that disproves a mint (InvalidArgument/Unimplemented
  /// -- the request shape can never mint, on this pull or the
  /// original one). `resolvesEntry` is the decision; the live client
  /// resolves (the RejectedDisproving arm routes ledger.resolve).
  /// Filing a mint-disproving refusal as a lost response (keeping
  /// the credential) is the immortal-entry defect. Auth-layer
  /// rejections do NOT route here -- they have their own seat
  /// (`authRefusalSeat`) because they disprove nothing.
  action answeredRefusalSeat(resolvesEntry: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    clientHoldsResume' = and { clientHoldsResume, not(resolvesEntry) },
    responseLost' = and { responseLost, not(resolvesEntry) },
    refusalFiledAsLost' = or { refusalFiledAsLost, and { clientHoldsResume, not(resolvesEntry) } },
    attempt' = attempt,
    viewClaimed' = viewClaimed,
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    confirmMinted' = confirmMinted,
  }

  /// Auth-refusal seat (merged_bug_074): the scheduler's CREDENTIAL
  /// layer rejected the resume presentation (PermissionDenied /
  /// Unauthenticated -- the rotation-skew trace, emitted without
  /// consulting attempt state). The rejection judges the
  /// PRESENTATION, not the mint the ORIGINAL unanswered pull may
  /// have committed: the live client KEEPS the entry (Unanswered
  /// disposition; `resolvesEntry = false`). `responseLost` is
  /// deliberately UNTOUCHED either way -- destroying the credential
  /// while the response stays lost is exactly what routes the
  /// attempt into the charged establishment window
  /// (`sweepEstablish`'s charged arm latches `chargedNoFault`), so
  /// the rotation-skew twin that flips this decision falsifies
  /// noFaultNeverCharged through the EXISTING machinery -- no new
  /// latch needed.
  action authRefusalSeat(resolvesEntry: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    clientHoldsResume' = and { clientHoldsResume, not(resolvesEntry) },
    responseLost' = responseLost,
    refusalFiledAsLost' = refusalFiledAsLost,
    attempt' = attempt,
    viewClaimed' = viewClaimed,
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    confirmMinted' = confirmMinted,
  }

  /// Confirm-probe seat (merged_bug_083): the probe is a READ of the
  /// puller's holdings. `mints` is the decision the wire
  /// discriminator forbids (the scheduler screens DeliverNew to
  /// NotYetReady on confirm_only); a minting confirm is the dying
  /// pod's probe creating fresh work for itself.
  action confirmProbeSeat(mints: bool): bool = all {
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    // The probe's mint-perturbation (the minting-confirm twin) is
    // equally a mint-after-license when one was issued.
    mintAfterGone' = or { mintAfterGone, and { mints, attempt != AOpen, goneLicensed } },
    confirmMinted' = or { confirmMinted, and { mints, attempt != AOpen } },
    attempt' = if (mints and attempt != AOpen) AOpen else attempt,
    viewClaimed' = or { viewClaimed, and { mints, attempt != AOpen } },
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
  }

  // --- live actions ------------------------------------------------------------

  /// The pull mint commits and the response reaches the store: claim
  /// held, credential persisted (bug_251), nothing lost.
  // Mirrors: rio-scheduler/src/actor/pull.rs:pull_assignment_inner
  action mintDelivered = all {
    node == NWantedLive,
    attempt != AOpen,
    not(viewClaimed),
    // The DeliverNew screen (sched.executor.confirm-fence): a fenced
    // token's mint is refused -- its exit was declared.
    not(goneFenced),
    mintApply(true, false),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// The pull mint commits but the RESPONSE is lost (worker timeout,
  /// stream cut). The credential persisted AT MINT — the fix — so the
  /// store's next poll can redeliver.
  // Mirrors: rio-scheduler/src/actor/pull.rs:pull_assignment_inner
  action mintResponseLost = all {
    node == NWantedLive,
    attempt != AOpen,
    not(viewClaimed),
    not(goneFenced),
    mintApply(true, true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// The store's next poll presents its credential and the SAME exec
  /// re-delivers, un-charged (bug_251; kernel
  /// `redelivery_credential_ok`).
  // Mirrors: rio-evidence-kernel/src/pull.rs:redelivery_credential_ok
  action resumeRedeliver = all {
    attempt == AOpen,
    responseLost,
    clientHoldsResume,
    responseLost' = false,
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,

    node' = node,
    attempt' = attempt,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    ghostClaim' = ghostClaim,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// Consumption close settles and the companion runs (release on
  /// both companion outcomes — merged_bug_055's law folds the
  /// companion-failure fallback into the same release decision). The
  /// settled close acks (bug_182).
  // Mirrors: rio-scheduler/src/actor/pull.rs:handle_report_outcome
  action consumeCloseOk = all {
    attempt == AOpen,
    node == NWantedLive,
    closeSettledApply(false, true),
    responseLost' = false,
    clientHoldsResume' = false,
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// The consumption close write FAILED: NACK retryably (bug_182 —
  /// PullRejection::ConsumptionNotDurable). The attempt stays open,
  /// the claim is kept, and the store's bounded report redelivery
  /// re-presents the same outcome.
  // Mirrors: rio-scheduler/src/grpc/actor_guards.rs:pull_rejection_to_status
  action consumeCloseFailedNack = all {
    attempt == AOpen,
    node == NWantedLive,
    closeFailedApply(false),
    responseLost' = responseLost,
    clientHoldsResume' = clientHoldsResume,
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// The cancel's terminal persist succeeds: one statement closes the
  /// assignment (db/derivations.rs terminal_assignment_status); the
  /// stamped close family's companion releases the view claim.
  // Mirrors: rio-scheduler/src/db/derivations.rs:update_derivation_status_in_tx
  action cancelPersistOk = all {
    node == NWantedLive,
    attempt == AOpen,
    closeSettledApply(false, true),
    node' = NCancelled,
    outbox' = outbox,
    cancelDurable' = true,
    responseLost' = false,
    clientHoldsResume' = false,
  }

  /// The cancel's terminal persist FAILS: the in-memory transition
  /// stands and the owned batch latches in the outbox
  /// (sched.attempt.cancel-close-driven) — the live decision is
  /// `latchesOutbox = true`; the outbox-dropped twin flips it and the
  /// seat latches `driverLost`.
  // Mirrors: rio-scheduler/src/actor/mod.rs:latch_status_batch
  action cancelPersistErr = all {
    node == NWantedLive,
    attempt == AOpen,
    cancelFailApply(true),
    cancelDurable' = cancelDurable,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
  }

  /// tick_flush_status_outbox re-drives the latched batch to Ok:
  /// dropped on Ok only; the close lands, the claim releases.
  // Mirrors: rio-scheduler/src/actor/housekeeping.rs:tick_flush_status_outbox
  action flushOk = any {
    all {
      outbox,
      attempt == AOpen,
      closeSettledApply(false, true),
      node' = node,
      outbox' = false,
      cancelDurable' = true,
      responseLost' = responseLost,
      clientHoldsResume' = clientHoldsResume,
    },
    all {
      outbox,
      attempt != AOpen,
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
      attempt' = attempt,
      viewClaimed' = viewClaimed,
      node' = node,
      outbox' = false,
      cancelDurable' = true,
      responseLost' = responseLost,
      clientHoldsResume' = clientHoldsResume,
      driverLost' = driverLost,
      ackedUnsettled' = ackedUnsettled,
      ghostClaim' = ghostClaim,
      chargedNoFault' = chargedNoFault,
      credentialClobbered' = credentialClobbered,
      refusalFiledAsLost' = refusalFiledAsLost,
      confirmMinted' = confirmMinted,
    },
  }

  /// The flush fails this tick: the entry is pushed back (front).
  // Mirrors: rio-scheduler/src/actor/housekeeping.rs:tick_flush_status_outbox
  action flushErr = all {
    outbox,
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    node' = node,
    attempt' = attempt,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    ghostClaim' = ghostClaim,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// Leadership loss + successor recovery. The outbox is
  /// leader-scoped (clear_persisted_state) and the in-memory view
  /// wipes with the tenure; the CREDENTIAL survives — it persisted in
  /// the mint's transaction (bug_251) and the successor recovers it
  /// off the guarded assignments join. A non-durable cancel
  /// evaporates; a durable one leaves a node the sweep closes free.
  // Mirrors: rio-scheduler/src/actor/mod.rs:clear_persisted_state
  action failover = all {
    node' = if (node == NCancelled and not(cancelDurable)) NWantedLive
            else if (node == NCancelled) NAbsent
            else node,
    // The fence plane survives failover: the row is durable PG state
    // and the license rides the TOKEN, not the tenure.
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    attempt' = attempt,
    outbox' = false,
    cancelDurable' = cancelDurable,
    viewClaimed' = false,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    ghostClaim' = ghostClaim,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  /// The establishment sweep, post-fix: routed through the total
  /// kernel. Live-wanted expired attempts charge; cancelled/absent
  /// close charge-free. The guard models the window ordering of
  /// bug_251's signed residual: a lost-response attempt whose
  /// credential is still held redelivers (the store's 600s report
  /// window) BEFORE the ~1h establishment window can charge it — the
  /// charged settle is reachable only when the credential is gone
  /// (a real crash) or the response was never lost.
  // Mirrors: rio-scheduler/src/actor/housekeeping.rs:tick_sweep_open_pull_attempts
  action sweepEstablish = any {
    // Live-wanted expired attempt: charged.
    all {
      attempt == AOpen,
      node == NWantedLive,
      not(responseLost and clientHoldsResume),
      closeSettledApply(true, true),
      node' = node,
      outbox' = outbox,
      cancelDurable' = cancelDurable,
      responseLost' = responseLost,
      clientHoldsResume' = false,
    },
    // Cancelled/absent node: charge-free (the kernel's total arm).
    all {
      attempt == AOpen,
      node != NWantedLive,
      not(responseLost and clientHoldsResume),
      closeSettledApply(false, true),
      node' = node,
      outbox' = outbox,
      cancelDurable' = cancelDurable,
      responseLost' = responseLost,
      clientHoldsResume' = false,
    },
  }

  /// LIVE (merged_bug_096): the fresh pass over a live credential
  /// SKIPS -- the ledger API refuses the second mint.
  // Mirrors: rio-store/src/materialize/client.rs:begin_fresh_claim
  action freshClaimSkipsLive = all {
    clientHoldsResume,
    freshClaimSeat(false),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// LIVE (bug_119, mint-disproving scope per merged_bug_074): a
  /// refusal that disproves a mint resolves the credential -- the
  /// entry leaves on the authoritative answer.
  // Mirrors: rio-store/src/materialize/client.rs:standing_effect
  action claimRefusedAnswered = all {
    clientHoldsResume,
    answeredRefusalSeat(true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// LIVE (merged_bug_074): an auth-layer refusal of the resume
  /// presentation keeps the credential -- the entry survives
  /// rotation skew untouched (a deliberate frame: the disposition IS
  /// "nothing changes"; the rotation-skew twin perturbs the seat
  /// decision to true and the charge path falsifies
  /// noFaultNeverCharged).
  // Mirrors: rio-store/src/materialize/client.rs:standing_effect
  action claimRefusedAuthSkew = all {
    clientHoldsResume,
    authRefusalSeat(false),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// LIVE (merged_bug_083): the confirm-only probe reads holdings and
  /// never mints (the screen).
  // Mirrors: rio-scheduler/src/actor/pull.rs:pull_assignment_inner
  action confirmProbeReads = all {
    confirmProbeSeat(false),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// LIVE (merged_bug_011): the scheduler answers this token's pull
  /// Gone -- the node is terminal/absent and nothing is held -- and
  /// fences the answer AHEAD of the reply (writesFence = true; the
  /// law is uniform across the live-loop and confirm arms). The
  /// unfenced-gone twin perturbs exactly this decision.
  // Mirrors: rio-scheduler/src/db/confirm_fences.rs:insert_confirm_fence
  action goneAnswered = all {
    node != NWantedLive,
    attempt != AOpen,
    goneAnswerSeat(true),
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  /// LIVE (merged_bug_011's straggler chain, middle step): a new
  /// submission re-readies the content-addressed drv -- same
  /// intent_id == drv_hash, fresh wantedness. The old cancel's
  /// durability is the PREVIOUS lifecycle's fact and resets with it;
  /// the fence plane is per-TOKEN and survives (which is exactly what
  /// screens the straggler).
  // Mirrors: rio-scheduler/src/dag/mod.rs:merge
  action resubmitReready = all {
    node != NWantedLive,
    attempt != AOpen,
    node' = NWantedLive,
    cancelDurable' = false,
    // The prior lifecycle's cancel-driver dies with it: a latched
    // outbox batch belongs to the cancel it was armed for, and the
    // resubmit reset supersedes that close (without this, the stale
    // flush would stamp the OLD cancel's durability onto the NEW
    // lifecycle -- the TLC artifact trace this guard removes).
    outbox' = false,
    attempt' = attempt,
    viewClaimed' = viewClaimed,
    clientHoldsResume' = clientHoldsResume,
    responseLost' = responseLost,
    goneFenced' = goneFenced,
    goneLicensed' = goneLicensed,
    mintAfterGone' = mintAfterGone,
    ghostClaim' = ghostClaim,
    driverLost' = driverLost,
    ackedUnsettled' = ackedUnsettled,
    chargedNoFault' = chargedNoFault,
    credentialClobbered' = credentialClobbered,
    refusalFiledAsLost' = refusalFiledAsLost,
    confirmMinted' = confirmMinted,
  }

  action step = any {
    mintDelivered,
    mintResponseLost,
    resumeRedeliver,
    consumeCloseOk,
    consumeCloseFailedNack,
    cancelPersistOk,
    cancelPersistErr,
    flushOk,
    flushErr,
    failover,
    sweepEstablish,
    freshClaimSkipsLive,
    claimRefusedAnswered,
    claimRefusedAuthSkew,
    confirmProbeReads,
    goneAnswered,
    resubmitReready,
  }

  // --- invariants ------------------------------------------------------------

  // bug_347's exclusion-pollution half: cancelled work is never
  // charged. Two forms — the node form (cancelled/absent is never the
  // charged party) and the ghost form (a durable cancel is never
  // followed by a charge).
  val cancelledNeverChargedAsCrash =
    not(attempt == AClosedCharged and node != NWantedLive)
    and not(attempt == AClosedCharged and cancelDurable)

  // bug_347's liveness half RESTATED over the transition relation
  // (bug_357 — replaces the tautological openAttemptHasDriver): the
  // failed cancel persist armed its driver on every step that needed
  // one.
  val notDriverLost = not(driverLost)

  // bug_182: every ack the model produced had a settled (or fenced —
  // folded into settled here, the Q20 posture) close behind it.
  val ackImpliesSettledOrArmed = not(ackedUnsettled)

  // merged_bug_055: a held view claim is always backed by an open
  // attempt — both the latch form (a close that strands the claim
  // latches at its seat) and the state form.
  val claimedImpliesOpenAttempt = and {
    not(ghostClaim),
    viewClaimed implies (attempt == AOpen),
  }

  // bug_251: an attempt whose only fault was a lost response is never
  // charged — the credential redelivers it first.
  val noFaultNeverCharged = not(chargedNoFault)

  // merged_bug_096: a live credential is never clobbered by a second
  // mint for the same job -- the ledger is the mint authority.
  val noCredentialClobber = not(credentialClobbered)

  // bug_119 (mint-disproving scope, merged_bug_074): a refusal that
  // DISPROVES a mint is never filed as a lost response (no immortal
  // ledger entries). Auth-layer refusals are out of scope by design
  // -- they keep the credential (see claimRefusedAuthSkew).
  val noRefusalFiledAsLost = not(refusalFiledAsLost)

  // merged_bug_083: a confirm-only probe never mints.
  val confirmNeverMints = not(confirmMinted)

  // merged_bug_011: no mint ever opens after this token's Gone answer
  // -- the exit-0 license is terminal for the token (write-ahead +
  // DeliverNew screen; sched.executor.confirm-fence).
  val noMintAfterGoneAnswer = not(mintAfterGone)

  val openAttemptsInvariants = all {
    cancelledNeverChargedAsCrash,
    notDriverLost,
    ackImpliesSettledOrArmed,
    claimedImpliesOpenAttempt,
    noFaultNeverCharged,
    noCredentialClobber,
    noRefusalFiledAsLost,
    confirmNeverMints,
    noMintAfterGoneAnswer,
  }
}

// OP-1 (MBT-1): the conformance-replay run module — quint-test-only
// (no TLC wiring; the `mbt-rio-openattempts` nextest check is its
// sole consumer via `quint test --out-itf`). RUNS ONLY: the step
// alphabet above is untouched; every live action is already
// deterministic (parameterless guards + fixed seat decisions), so the
// runs compose them directly.
//
// rio-scheduler/src/actor/tests/mbt_openattempts.rs mirrors each
// action list verbatim through the production materialization-claim
// lane (handle_pull_assignment / handle_report_outcome /
// tick_sweep_open_pull_attempts) and diffs the projected state
// (attempt / node / viewClaimed / clientHoldsResume / cancelDurable)
// after every step. The runs double as in-model regression tests:
// each `.expect` re-checks the post-state on every `quint test`.
module openAttemptsReplay {
  import openAttempts.*

  /// Mint -> settled consumption: the claim delivers with its
  /// credential persisted (bug_251), the consumption close settles
  /// CHARGE-FREE and the companion releases the claim (bug_182 /
  /// merged_bug_055).
  run oaHappyPathRun =
    init
      .then(mintDelivered)
      .then(consumeCloseOk)
      .expect(and {
        attempt == AClosedFree,
        node == NWantedLive,
        not(viewClaimed),
        not(clientHoldsResume),
        not(responseLost),
        not(chargedNoFault),
      })

  /// The bug_251 redeliver-first window (the OQ-12 acceptance, GREEN
  /// half): the mint's response is LOST but the credential persisted
  /// in the mint's transaction, so the store's next poll REDELIVERS
  /// un-charged; only then does the establishment window charge the
  /// (now genuinely expired) attempt — `chargedNoFault` stays false.
  /// The sweep guard `not(responseLost and clientHoldsResume)` is the
  /// WINDOW-ORDERING abstraction (store redelivery << establishment
  /// window), which is why this run must take the redeliver step
  /// before the sweep can fire.
  run oaSweepWindowRun =
    init
      .then(mintResponseLost)
      .then(resumeRedeliver)
      .then(sweepEstablish)
      .expect(and {
        attempt == AClosedCharged,
        node == NWantedLive,
        not(viewClaimed),
        not(clientHoldsResume),
        not(responseLost),
        not(chargedNoFault),
      })
}
