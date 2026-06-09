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
