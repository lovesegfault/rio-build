# bughunt-4 S7 generated sweep sets (banner b)

Each section is the committed output of the named command at the
commit that closes the finding. Re-run the command to re-derive; a
drifted set is a failed sweep, not a stale doc. Line numbers are as of
the S7 chain tip.

## merged_bug_029 — every `servedComplete` writer

    $ grep -rn "servedComplete\s*=" rio-dashboard/src/lib/logStream.svelte.ts \
        rio-dashboard/src/lib/lineCursor.ts | grep -v "servedComplete ===" | grep -v "servedComplete:"

    logStream.svelte.ts:256  declaration (false)
    logStream.svelte.ts:486  execSwitch arm reset (merged_bug_063; the chunk-sighted reset)
    logStream.svelte.ts:565  per-chunk adoption (post-keyed-visit, cursor's own numbering)
    logStream.svelte.ts:703  law next-state assignment (THE m029 consumption site —
                             the only writer on the re-open path; the caller has
                             nowhere else to get its next servedComplete from)

    $ grep -rn "servedComplete:" rio-dashboard/src/lib/lineCursor.ts | grep -v ": boolean"

    lineCursor.ts:165  the consumption cell itself ({ kind: 'reopen', servedComplete: false }
                       on every naturalEnd re-open; transport/openFailed pass through)

    No TS-plane compiler witness exists for "no other writer" (banner b
    fallback): this committed grep IS the census. The exhaustive
    tailNext decision table pins the consumption cell on every
    naturalEnd re-open row.

## merged_bug_035 — every `graceDeadline` consumer

    $ grep -n "graceDeadline" rio-dashboard/src/lib/logStream.svelte.ts

    264  declaration (null until armed)
    377-378  loop-head UN-ARM on oracle retreat (m134 self-clearing edge)
    385-386  loop-head arm (terminal && unarmed)
    388  loop-head enforcement (cut at expiry)
    415,418  absolute grace race edge (bug_145 shape, joins while armed)
    494  execSwitch re-derive (m014 rider — old execution's world)
    552,557  PRODUCTIVE RE-ARM (THE m035 site: serve/gapThenServe only;
             skip never extends — quiet-time budget law)
    675-679  post-loop un-arm (oracle retreat, attempt-end mirror)
    681-682  post-loop arm
    684  graceExpired law input
    690  futile-reopen finalize (remaining < DRAIN_MARGIN_MS)
    708,711  reopen delay cap (lands DRAIN_MARGIN_MS before the deadline)

    All sites read/write the single closure-local deadline; no second
    grace clock exists. Both starvation polarities pinned:
    historical_drain_rearms_grace (productive re-arm) and
    skip_flood_still_expires (unproductive flood).

## merged_bug_134 — every terminality-oracle consumer

    $ grep -rln "statusOf\|isTerminal\|focusedStatus" rio-dashboard/src \
        --include="*.svelte" --include="*.svelte.ts"

    lib/buildGraphPoll.svelte.ts   THE source (statusOf over the live node set)
    components/BuildDrawer.svelte  focusedStatus = $derived(poll.statusOf(focusedDrv));
                                   isTerminal closure = buildTerminal || TERMINAL.has(live)
    components/LogViewer.svelte    isTerminal prop pass-through to createLogStream
    lib/logStream.svelte.ts        effTerminal() — the law input (+ no-oracle fallback)
    components/Graph.svelte        renders from poll.nodes (same store — no second source)

    The click-time capture variable no longer exists; svelte-check
    proves every consumer reads the store interface. Graph renders and
    the oracle derive from the SAME poll — a frozen snapshot is
    unrepresentable.

## bug_277 — every Promise.race participant

    $ grep -rn "Promise.race" rio-dashboard/src --include="*.ts" --include="*.svelte" | grep -v __tests__

    lib/logStream.svelte.ts:423  the loop's single race site

    Participants (the `edges` array construction above it):
    - nextMsg        reused across lost races BY ITERATOR CONTRACT (never two
                     next() in flight); settles on every message (keep-alive
                     cadence worst case) — reactions released, exempt
    - abortEdgeFor() PER-ITERATION + listener removed at settle (THE bug_277 fix)
    - tick sleep     per-iteration; tickCtrl.abort() + sleep()'s finish cleans up
    - grace sleep    per-iteration; same cleanup

    Witness: race_edges_are_per_iteration counts appearances per promise
    identity across all races — bounded at <=5 (pre-fix: 32 for one edge).

## bug_348 — every tailNext input axis

    The total-transition decision table in lineCursor.test.ts IS the
    witness (banner a): 3 reopen-family causes x 2 modes x 2^3 flags
    fully enumerated + both terminal causes over their full input
    cells. Callers: the loop's single decision site (logStream:685)
    and the no-oracle fallback (effTerminal) — both thread the mode.

## bug_169 — every counter vs its derivation source

    gapCount       $derived(rows.reduce(kind === 'gap')) — cannot desync
    droppedLines   single writer in applyCap, line-filtered over the
                   spliced prefix
    truncated      boolean latch (single writer, applyCap)
    rows.length    the array itself

    No other state describes row-set contents.
