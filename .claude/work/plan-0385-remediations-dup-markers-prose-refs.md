# Plan 0385: remediations/phase4a duplicate col-0 r[...] markers → prose refs

consol-mc175: six remediation docs at [`docs/src/remediations/phase4a/`](../../docs/src/remediations/phase4a/) contain sixteen col-0 `r[domain.area.detail]` paragraphs — **marker definitions**, not references. Every one of these markers ALSO exists canonically in `docs/src/components/*.md` (verified cross-check — all 16 have a canonical home). The remediation docs were authored as implementation-guide prose and the authors pasted the marker identifier at col-0 for cross-reference, inadvertently creating a second definition-site.

**Invisible today:** [`config.styx:27-37`](../../.config/tracey/config.styx) `specs.include` lists only `docs/src/components/*.md` + `observability.md` + `security.md` — `remediations/` is NOT scanned. `tracey query validate` shows 0 errors. **Drift risk on bump:** when a canonical marker is bumped (`tracey bump` → `r[gw.conn.keepalive]` → `r[gw.conn.keepalive+2]`), the remediation doc's copy silently retains the unversioned form. Human readers see stale marker text. **Include-expansion bomb:** if anyone ever adds `docs/src/remediations/**` to the `include` glob (e.g., to define remediation-specific `r[rem.*]` markers), tracey immediately sees 16 duplicate-ID errors.

**Clause-4(a) docs-only** — `docs/src/remediations/phase4a/*.md` is outside `config.styx` include AND outside any nix drv fileset; no behavioral change, no CI-tier invalidation.

## Tasks

### T1 — `docs(remediations):` convert 16 col-0 r[...] definitions to prose references

Six files, sixteen markers. For each: convert the col-0 standalone `r[...]` paragraph to an inline backtick-fenced reference (`` `r[...]` ``) within a sentence, OR delete if the marker is already prose-referenced elsewhere in the same section.

| File | Line | Marker | Canonical home |
|---|---|---|---|
| [`01-poison-orphan-recovery.md:352`](../../docs/src/remediations/phase4a/01-poison-orphan-recovery.md) | 352 | `r[sched.recovery.poisoned-failed-count]` | [`scheduler.md`](../../docs/src/components/scheduler.md) |
| [`02-empty-references-nar-scanner.md:1241`](../../docs/src/remediations/phase4a/02-empty-references-nar-scanner.md) | 1241 | `r[worker.upload.references-scanned]` | [`worker.md`](../../docs/src/components/worker.md) |
| `02-empty-references-nar-scanner.md:1251` | 1251 | `r[worker.upload.deriver-populated]` | `worker.md` |
| `02-empty-references-nar-scanner.md:1261` | 1261 | `r[store.signing.empty-refs-warn]` | [`store.md`](../../docs/src/components/store.md) |
| `02-empty-references-nar-scanner.md:1268` | 1268 | `r[store.gc.empty-refs-gate]` | `store.md` |
| [`09-build-timeout-cgroup-orphan.md:794`](../../docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md) | 794 | `r[worker.cgroup.kill-on-teardown]` | `worker.md` |
| `09-build-timeout-cgroup-orphan.md:797` | 797 | `r[worker.timeout.no-reassign]` | `worker.md` |
| `09-build-timeout-cgroup-orphan.md:800` | 800 | `r[worker.status.nix-to-proto]` | `worker.md` |
| `09-build-timeout-cgroup-orphan.md:803` | 803 | `r[worker.cancel.flag-clear-enoent]` | `worker.md` |
| [`11-tonic-health-drain.md:659`](../../docs/src/remediations/phase4a/11-tonic-health-drain.md) | 659 | `r[common.drain.not-serving-before-exit]` | [`observability.md`](../../docs/src/observability.md) |
| [`15-shutdown-signal-cluster.md:533`](../../docs/src/remediations/phase4a/15-shutdown-signal-cluster.md) | 533 | `r[worker.shutdown.sigint]` | `worker.md` |
| [`18-ssh-hardening.md:677`](../../docs/src/remediations/phase4a/18-ssh-hardening.md) | 677 | `r[gw.conn.session-error-visible]` | [`gateway.md`](../../docs/src/components/gateway.md) |
| `18-ssh-hardening.md:682` | 682 | `r[gw.conn.channel-limit]` | `gateway.md` |
| `18-ssh-hardening.md:688` | 688 | `r[gw.conn.keepalive]` | `gateway.md` |
| `18-ssh-hardening.md:693` | 693 | `r[gw.conn.nodelay]` | `gateway.md` |
| `18-ssh-hardening.md:698` | 698 | `r[gw.conn.real-connection-marker]` | `gateway.md` |

**Conversion shape** (applies uniformly — the remediation docs typically have a "Tracey markers" section that intros the markers before a diff/code block):

```markdown
<!-- BEFORE: col-0 definition (tracey-invisible today, drift risk on bump) -->
r[gw.conn.keepalive]

<!-- AFTER: prose reference with backtick fence (tracey never parses this) -->
Tracey marker: `r[gw.conn.keepalive]` — see [`gateway.md`](../../components/gateway.md).
```

Where the markers are clustered (e.g., `18-ssh-hardening.md` has five in a block at `:677-698`), collapse to a single "Tracey markers: `r[gw.conn.session-error-visible]`, `r[gw.conn.channel-limit]`, `r[gw.conn.keepalive]`, `r[gw.conn.nodelay]`, `r[gw.conn.real-connection-marker]` — all in [`gateway.md`](../../components/gateway.md)." paragraph.

**Self-verify loop:** `grep -rn '^r\[' docs/src/remediations/phase4a/` MUST return 0 hits after. That's the mechanical proof every col-0 definition is gone.

## Exit criteria

- `grep -rn '^r\[' docs/src/remediations/phase4a/` → 0 hits
- `grep -c '\`r\[' docs/src/remediations/phase4a/*.md` ≥ 16 (each former col-0 definition now a backtick-fenced prose ref)
- `nix develop -c tracey query validate` → `0 total error(s)` (no change — markers were invisible before, still invisible after; this proves no accidental `impl`/`verify` text leaked)

## Tracey

References existing markers (prose-only — this plan REMOVES spurious definition-sites, doesn't add coverage):
- All 16 markers in the T1 table above are canonically defined in `docs/src/components/*.md` + `docs/src/observability.md`; this plan touches none of those canonical sites.

## Files

```json files
[
  {"path": "docs/src/remediations/phase4a/01-poison-orphan-recovery.md", "action": "MODIFY", "note": "T1: :352 col-0 r[sched.recovery.poisoned-failed-count] → backtick prose ref"},
  {"path": "docs/src/remediations/phase4a/02-empty-references-nar-scanner.md", "action": "MODIFY", "note": "T1: :1241,:1251,:1261,:1268 — 4 col-0 markers → collapsed prose ref paragraph"},
  {"path": "docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md", "action": "MODIFY", "note": "T1: :794,:797,:800,:803 — 4 col-0 markers → collapsed prose ref paragraph"},
  {"path": "docs/src/remediations/phase4a/11-tonic-health-drain.md", "action": "MODIFY", "note": "T1: :659 col-0 r[common.drain.not-serving-before-exit] → backtick prose ref"},
  {"path": "docs/src/remediations/phase4a/15-shutdown-signal-cluster.md", "action": "MODIFY", "note": "T1: :533 col-0 r[worker.shutdown.sigint] → backtick prose ref"},
  {"path": "docs/src/remediations/phase4a/18-ssh-hardening.md", "action": "MODIFY", "note": "T1: :677,:682,:688,:693,:698 — 5 col-0 gw.conn.* markers → collapsed prose ref paragraph"}
]
```

```
docs/src/remediations/phase4a/
├── 01-poison-orphan-recovery.md      # T1: 1 marker
├── 02-empty-references-nar-scanner.md # T1: 4 markers
├── 09-build-timeout-cgroup-orphan.md  # T1: 4 markers
├── 11-tonic-health-drain.md           # T1: 1 marker
├── 15-shutdown-signal-cluster.md      # T1: 1 marker
└── 18-ssh-hardening.md                # T1: 5 markers
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [295], "note": "discovered_from=consol-mc175. No hard deps — all six files exist since phase-4a. Clause-4(a) docs-only: remediations/phase4a/ is outside config.styx include (:27-37) AND outside crane filesets; tracey-validate drv IS affected by flake.nix:401 cleanSource (per P0304-T29's pending fileset.difference fix) but execution output byte-identical — BEHAVIORAL drv-identity tier. soft_dep P0295: T13 in that batch adds MOOT-P0294 tombstones to phase4a.md and T14 edits 09-build-timeout-cgroup-orphan.md (Build CR→rio-cli refs at :443,:618,:695) — this plan edits :794-803 in the same file, non-overlapping sections. Sequence-independent."}
```

**Depends on:** none.

**Conflicts with:** [`09-build-timeout-cgroup-orphan.md`](../../docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md) — [P0295-T14](plan-0295-doc-rot-batch-sweep.md) edits `:443/:618/:695`; this plan edits `:794-803`; non-overlapping. Remaining five files have no other UNIMPL plan touching them.
