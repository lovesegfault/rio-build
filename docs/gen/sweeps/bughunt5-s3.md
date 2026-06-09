# bughunt-5 S3 generated sweep sets (banner b)

Each section is the committed output of the named command at the
commit that closes the finding. Re-run the command to re-derive; a
drifted set is a failed sweep, not a stale doc.

## merged_bug_001 — every exposure-uid producer and consumer

    $ rg -n 'format!\("exposure|EventUid::new\(|event_uid: Some' rio-controller/src rio-scheduler/src/admin/mod.rs rio-scheduler/src/admin/tests/mod.rs -g '*.rs'

    rio-controller/src/reconcilers/node_informer.rs:1269  EventUid::new body — the ONLY format! mint (fixed-here; demands cluster + hw + gate-admitted WindowId)
    rio-controller/src/reconcilers/node_informer.rs:1325  queue_exposure_slices — the sole mint CALL site (fixed-here; flush arm banks only under WindowGate::admit → Some)
    rio-controller/src/reconcilers/node_informer.rs:1143  report_exposure wire write (event_uid: Some(slice.uid.as_str()) — typed; reworded doc)
    rio-scheduler/src/admin/tests/mod.rs:143              legacy uid fixture — verified n/a: pins the OPAQUE-key dedup contract the reworded handler doc states explicitly; the scheduler never parses uids; file outside the S3 plane (its r13-allow(opaque-consumer) tag rides S8's WO-S8-11 conversion census, comment-only)

    Docs/spec carriers reworded with the close (not uid producers):
    node_informer.rs:794-808 unshipped-queue doc · :1110-1131 report_exposure doc
    · :1289-1305 PendingExposure doc · controller.typ ctrl.informer.exposure-recredit
    (+1→+2, 10 markers re-stamped) · admin/mod.rs:1235-1246 handler doc (commit 2).
    M_047 = untouched (checksum-frozen; `git diff --stat rio-migrations/` empty).
    Interrupt-leg Event uids = verified n/a (apiserver-minted v4 UUIDs; the
    per-cluster apiserver is the implicit axis; collision probability is the
    UUID guarantee — run_spot_interrupt_watcher passes `ev.metadata.uid`
    through untouched).

    A `format!`-minted uid for the M_047-constrained exposure column no
    longer typechecks: `PendingExposure.uid` is `EventUid`, whose only
    constructor demands every identity axis.

## merged_bug_033 — every await in run()'s select arms

    $ awk '/^pub async fn run\(/,/^}/' rio-controller/src/reconcilers/node_informer.rs | grep -n '\.await\|cancelled()'

    (offsets relative to run() at node_informer.rs:775)
    +69   shutdown.cancelled()                       = the biased disclosure arm (UNCHANGED — the one sanctioned forfeiture exit, counted per slice)
    +86   shutdown.run_until_cancelled(nodes.list)   = fixed-here (wrapped; a hung apiserver cannot pin the loop past SIGTERM)
    +169  admin_call(append_interrupt_sample)        = fixed-here (inside the ship closure — structurally inside ship_all's budget + cancel race; per-RPC bounded by ADMIN_RPC_TIMEOUT)
    +183  ship_all(...).await                        = fixed-here (THE combinator: budget-bounded, shutdown-preemptible; the only ship path)
    +218  shutdown.run_until_cancelled(config.load)  = fixed-here (wrapped; load internally bounded at 5 attempts ≤ ~56s worst case — still raced)

    Same-file sibling loops = verified n/a with reason:
    run_spot_interrupt_watcher / run_pod_annotator are independent
    spawn_monitored tasks driven by watcher streams — no
    biased-disclosure arm to starve; their awaits (stream.next, one
    GET/patch/append per rare event) gate no counted forfeiture.

    Re-pointed tests (markers under exposure-recredit+2 / the new
    drain-budget rule): failed_exposure_slice_recredits_to_next_flush,
    retained_class_without_fresh_slice_still_retries,
    retried_slice_carries_identical_uid,
    conservation_holds_across_deferred_window — all drive ship_all
    with production-minted backlogs (minted_backlog →
    WindowGate::admit + queue_exposure_slices); injected ship closures
    speak only the production outcome alphabet (bool).
