# bughunt-4 S3 generated sweep sets (banner b)

Each section is the committed output of the named command at the
commit that closes the finding. Re-run the command to re-derive; a
drifted set is a failed sweep, not a stale doc.

## bug_220 — every `defer_until` writer

    $ grep -rn "defer_until\s*=" rio-scheduler/src --include="*.rs" | grep -v tests/ | grep -vE "defer_until: |//|defer_until = defer"

    rio-scheduler/src/actor/materialize.rs:231  release_claim_deferring (THE production chokepoint)
    rio-scheduler/src/actor/materialize.rs:263  test_set_defer_until (#[cfg(test)] seeding only)
