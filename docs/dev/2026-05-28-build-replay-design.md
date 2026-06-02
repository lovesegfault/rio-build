# Build replay: source-agnostic record/replay for rio-build

**Status:** draft — all §12.2 review questions resolved and folded into the body (2026-05-28)
**Date:** 2026-05-28
**Branch:** nixpkgs-parity

**Abstract.** rio-build currently has two partially overlapping systems for answering the same question — *can rio replay a set of recorded builds accurately, and where do outcomes differ?* The nixpkgs-parity branch built an in-cluster campaign engine that rebuilds Hydra evaluations and scores agreement against cache.nixos.org; the xtask-replay branch built a laptop-orchestrated replayer for nixbuild.net production archives with an in-band worker-protocol transport, timing-faithful scheduling, and client-side content supply. This document specifies their convergence into one subsystem, called **build replay**: any number of recorders write a single versioned, source-agnostic **replay archive** format (v1); one in-cluster **replay engine** (the existing campaign engine, evolved to use the worker-protocol client operations as its transport) executes **replay campaigns** over those archives; and a layered comparison model (verdicts, dispositions, report policies) replaces the two source-branded vocabularies that exist today. Where an archive came from never influences how it is replayed or judged; provenance is opaque metadata for humans. The migration is phased; the engine transport moves to the client-ops path as a clean cutover (no nix-CLI fallback is implemented or retained), each phase is validated offline (conformance, fake-backend end-to-end, golden report and format tests), and the first live validation is a smoke campaign run on the converged system once the operator deploys it.

## Table of contents

- [2. Motivation & background](#2-motivation--background)
- [3. System model & terminology](#3-system-model--terminology)
- [4. Archive format v1](#4-archive-format-v1)
- [5. Recorders](#5-recorders)
- [6. The replayer](#6-the-replayer)
- [7. Comparison model](#7-comparison-model)
- [8. Supply planning](#8-supply-planning)
- [9. Scheduling](#9-scheduling)
- [10. Operator surface](#10-operator-surface)
- [11. Migration & sequencing](#11-migration--sequencing)
- [12. Risks & open questions](#12-risks--open-questions)
- [13. Appendices](#13-appendices)
  - Appendix A — Field-level mapping: eval-set artifacts → archive v1
  - Appendix B — Mapping: nxb-replay v0 archives → v1 neutral vocabulary
  - Appendix C — Mapping: current parity buckets and replay verdicts → unified verdicts/dispositions
  - Appendix D — Glossary of rio components

## 2. Motivation & background

### 2.1 What the parity branch built

The nixpkgs-parity branch (this branch) contains a complete, working validation pipeline for replaying Hydra evaluations against a rio cluster:

- **The eval-set layer** (`rio-parity/src/evalset/`, `src/cmd/eval.rs`, `src/hydra.rs`, `src/nixcache.rs`, `src/s3.rs`). Because rio does not evaluate and Hydra's `.drv` files cannot be downloaded, a one-shot big-memory eval Job re-evaluates nixpkgs at the pinned revision with Hydra-faithful arguments, verifies drvPath fidelity against Hydra (exhaustive for scoped sets, sampled for full sets), and packages a write-once, digest-keyed **eval set** into S3: `manifest.jsonl`, `eval-errors.jsonl`, `fidelity.json`, `dep-closure.jsonl`, `drvs.tar.zst`, and `evalset.json` uploaded last as the completeness marker under a conditional PUT. Hydra itself is touched under a strict politeness budget; mass per-path ground truth comes from cache.nixos.org narinfo.
- **The campaign engine** (`rio-parity/src/run/`). A long-running in-cluster k8s Job driven by a spec ConfigMap: marker-gated stages (plan → hydra-truth → warm → submit ∥ collect ∥ watchdog → report), greedy dual-capped batching, submission via stock `nix copy` + `nix build --store ssh-ng://…` child processes, collection via the scheduler's `GetBuildGraph`/`ListPoisoned` Admin RPCs, a two-signal infra-vs-target attribution rule, a 15-bucket classification, append-only JSONL state with S3 sync and resume across pod restarts, watchdog suspension, a PAUSE file, deadline-partial behavior, and a report whose headline (build-outcome parity %) always travels with a comparability block.
- **The operator surface** (`xtask/src/parity/`): `cargo xtask parity eval | launch | status | report`, tenant provisioning, SSH/HMAC secret plumbing, pre-flight checks, and the spec ConfigMap + campaign Job machinery.
- **Cluster enablement**: per-tenant `keep_going` and `force_build_roots` build policy in the gateway and scheduler (with spec rules and tests), the parity Helm/network-policy/IRSA/ECR wiring, and the three campaign tenants (`parity-leaf`, `parity-selfhosted`, `parity-warm`).

What this branch got right: in-cluster durability at multi-day scale (resume, S3 state sync, watchdog, deadline-partial), reporting rigor (comparability block, explicit denominator rules, infra-rate gating), truth handling at nixpkgs scale (politeness budget, narinfo sweep, fidelity gate), supply through the scheduler's existing bulk substitution (the warm stage moves no bytes through the engine), and a disciplined write-once S3 artifact model.

What it got wrong, or at least outgrew: the transport. Submission shells out to `nix build` and scrapes stderr for a build id and relayed failure reasons; collection reconstructs per-derivation results from `GetBuildGraph`, which truncates at 5,000 nodes and motivated a not-yet-wired `QueryDerivationStatuses` RPC. Truth is fetched at campaign time (the hydra-truth stage), so a campaign depends on cache.nixos.org being reachable and unchanged. And the comparison vocabulary is source-branded: `hydra-only-failure`, `hydra-unknown`, `HydraOutcome`, `hydra.jsonl`.

### 2.2 What the xtask-replay branch built

The xtask-replay branch (not checked out here; readable at `origin/xtask-replay`) built a replayer for recorded build load, designed around archives produced from nixbuild.net production data by the external `nxb-replay` tool:

- **The v0 archive contract** (`xtask/src/k8s/replay/archive.rs`): `manifest.json`, `requests.jsonl`, `builds.jsonl`, `impure-env.json`, narinfo sidecars, and embedded `nix/store/` content (derivation ATerm files plus unpacked store paths), packed as a DwarFS image or left as a plain directory, with a committed fixture serving as the executable specification.
- **A real worker-protocol client** in `rio-nix` (`protocol/client.rs`, `protocol/wire/framed.rs`): `client_build_paths_with_results`, `client_query_valid_paths`, `client_query_path_info`, `client_add_to_store_nar`, `client_add_multiple_to_store`, a typed stderr drain, and an incremental `FramedWriter` — all transport-agnostic, all under caller-supplied deadlines, with an abandon-the-channel-after-framed-error contract, conformance-tested against the real gateway session handler in `rio-gateway/tests/client_ops_conformance.rs`.
- **An SSH transport** (`replay/client.rs`): an in-process russh connection pool with a fixed 4-channels-per-connection fan-out, fail-closed host-key policy, lazy re-dial, and per-op deadlines.
- **A substituter client** (`replay/substituter.rs`): narinfo probes and verified, streaming NAR fetches over HTTPS and S3 with decompression.
- **A supply planner** (`replay/supply.rs`, `replay/prewarm.rs`): the workload rule (never supply outputs the target must rebuild), a per-path source ladder, reference-safe upload planning, cross-request upload claims, and a prewarm phase that uploads in topological levels before the clock starts.
- **Timing-faithful scheduling** (`replay/timeline.rs`): each request fires at its recorded offset divided by a speedup factor, recorded client disconnects are reproduced by abandoning the SSH channel at the recorded relative time, and dispatch lateness is accounted.
- **A verdict taxonomy** (`replay/compare.rs`): match, skip, regression, non-reproducible (per-output NAR-hash maps), failure-not-reproduced, cancellation-not-reproduced, disconnect-replayed, upload-rejected, request-error, with a streamed divergence log and a fail-on exit policy.
- **An offline dry-run** exercised in CI against the committed fixture.

What this branch got right: the transport (in-band per-derivation results over `BuildPathsWithResults` instead of stderr scraping and graph reconstruction), client-side supply for content no substituter can provide, the archive-as-contract framing with both a packed and a directory form, the workload-output measurement rule, timing fidelity as a first-class capability, and the conformance-test loop that keeps the client and the gateway honest.

What it could not do: run at campaign scale. The run loop is laptop-orchestrated (one process, one operator machine, port-forward tunnels), has no resume, no S3 state, no watchdog, no multi-day posture, and its comparison vocabulary is keyed directly to nixbuild.net status codes.

### 2.3 Why they converge

Both systems answer the same question — can rio replay a set of builds accurately, and where do outcomes differ? — against different sources, and each is strong exactly where the other is weak. The parity branch has the right runtime (an in-cluster, resumable, observable campaign engine with an operator workflow) and the right reporting discipline; the replay branch has the right data plane (worker-protocol client ops, supply planning, timing) and the right archive framing. Maintaining two archive formats, two engines, two verdict vocabularies, and two operator surfaces would mean every improvement lands twice or drifts. Convergence produces one engine for which a Hydra-derived archive and a nixbuild.net-derived archive are indistinguishable inputs, one comparison model whose names carry no source words, and one operator workflow.

The shape of the converged system is fixed by decision: **N recorders → one versioned archive format → one in-cluster replayer → per-campaign policies.** Recorders are free to be source-specific; everything downstream of the archive boundary is source-agnostic.

### 2.4 Goals

- **G1.** One versioned, source-agnostic replay archive format (v1), evolved from the xtask-replay v0 contract, with explicit format versioning, capability flags, a neutral expected-outcome vocabulary, dependency-closure data, opaque provenance, content-addressed identity, and a write-once S3 layout.
- **G2.** One replayer: the existing in-cluster campaign engine, which absorbs the xtask-replay transport (client ops behind the existing Submitter seam), supply planner, and timeline scheduler, and keeps its k8s Job runtime, state/resume model, watchdog, and report pipeline (§6 The replayer).
- **G3.** A layered, source-neutral comparison model — verdict core, disposition layer, report policies — with an explicit rename mapping from today's parity buckets and replay verdicts (§7 Comparison model).
- **G4.** Truth baked into archives at creation time, so campaigns never re-query the source of a recording.
- **G5.** Recorders stay decoupled: the Hydra eval pipeline and nxb-replay evolve independently behind the archive contract; a future rio-native recorder slots in without engine changes (§5 Recorders).
- **G6.** Durability: fat archives that replay with no external cache, write-once digest-keyed S3 prefixes, completion markers.
- **G7.** Operator-surface continuity: the xtask launch/status/report workflow, the spec ConfigMap, and the campaign Job model survive; the xtask-replay branch's laptop orchestration is replaced by a thin launcher plus a small dev mode (§10 Operator surface).
- **G8.** A phased migration in which every phase is validated offline and the first live validation is a smoke campaign on the converged system at deploy time (§11 Migration & sequencing).

### 2.5 Non-goals

- **N1.** A rio-native recorder. The format anticipates one (§5.3); building it is out of scope.
- **N2.** Multi-session tenancy / identity mapping for replaying rio-against-rio with per-session tenants. Deliberately deferred; the manifest is designed so it can be added under a format version bump (§4.10).
- **N3.** Changing gateway or scheduler behavior beyond what the parity branch already landed (per-tenant `keep_going`, `force_build_roots`) — with one bounded, explicitly authorized exception: extending the gateway's `handle_build_paths_with_results` to return true per-root results instead of cloning the DAG-level result (§6.4). That is the single rio-side change this convergence requires; it is scheduled with the transport swap (§11.4) and carries its own conformance-test and spec-rule updates. In every other respect the replayer remains a client of the cluster, not a component of it.
- **N4.** Forwarding impure client environment variables to builds. Units that declare `impureEnvVars` are demoted, as on both branches today.
- **N5.** Bit-reproducible archive creation. Two recordings of the same source state are distinct archives with distinct identities.
- **N6.** Replacing the parity headline metric. The parity report (headline percentage with denominator rules and a comparability block) survives unchanged in meaning — it becomes one report policy among several rather than the only output the machinery can produce.
- **N7.** Automated archive retention: no scheduled pruning, no age- or size-based expiry, no garbage collection of unreferenced archives. Manual per-archive management is in scope — `cargo xtask replay list` / `replay delete <short-id>` (§10.1) let an operator inspect and remove published archives by hand; nothing deletes an archive without an operator naming it.

**Deployment policy (dev clusters, full-wipe).** Every rio-build deployment relevant to this subsystem is a dev/experimental cluster, deployed by full wipe. There is no production deployment to stay compatible with, and no expectation that previously deployed rio components or previously uploaded campaign/eval-set state survive a redeploy. The design therefore carries no runtime detection of older deployed components, no fallback paths for them, and no migration paths for pre-existing campaign or eval-set S3 state: cluster-side prerequisites (e.g. the gateway per-root results change, §6.4) are simply deployed before the phase that needs them, and superseded S3 state is abandoned and regenerated. The one deliberate exception is recorded data that cannot be regenerated — previously recorded nxb-replay archives of past production windows stay readable through the v0 shim (§4.12); that is recorded data, not deployed-component state.

## 3. System model & terminology

### 3.1 The pipeline

```
   recorders                 archive store                replayer                   policies
┌──────────────────┐      ┌──────────────────┐      ┌─────────────────────┐      ┌──────────────────┐
│ Hydra eval        │      │                  │      │  in-cluster replay  │      │ parity report    │
│ pipeline          ├─────▶│  replay archives │─────▶│  engine (campaign   ├─────▶│ regression gate  │
│ nxb-replay        │      │  (v1, S3,        │      │  Job; plan/supply/  │      │ …chosen at       │
│ (future: rio      │      │  write-once)     │      │  execute/collect/   │      │  launch, not in  │
│ recorder)         │      │                  │      │  report)            │      │  the archive     │
└──────────────────┘      └──────────────────┘      └─────────────────────┘      └──────────────────┘
```

Recorders are source-specific programs that observe some build source — a Hydra evaluation plus the public binary cache, nixbuild.net production traffic, eventually rio itself — and write a **replay archive**: a self-contained, versioned package of workload, expected outcomes, dependency data, and (optionally) embedded content. The **replayer** is one program, the in-cluster replay engine, which executes a **campaign** over exactly one archive against exactly one target cluster and produces verdicts, dispositions, and reports under **policies** chosen at launch time.

### 3.2 The source-agnosticism principle

Where an archive came from never matters to the replayer. Concretely:

- No source names appear in the archive schema, in engine code paths, in the verdict or disposition vocabulary, or in report semantics. There is no "source kind" or "flavor" field anywhere; behavior differences are expressed exclusively through **capability flags** (§4.3) that any recorder may set.
- Provenance is an opaque block (§4.2) carried through to reports verbatim for humans and audit. The engine never branches on its contents.
- Recorders translate their native status codes and truth sources into the neutral expected-outcome vocabulary (§4.6) **at archive creation time**. The replayer never re-queries the source of a recording; everything it needs to judge a campaign is in the archive.
- Source words are permitted in exactly three places: inside provenance values, inside recorder code, and in recorder documentation (§5).

This principle is what lets a fat Hydra-derived archive be replayed years later with cache.nixos.org unreachable, lets nxb-replay archives run through the same engine as nixpkgs campaigns, and lets a future rio recorder appear without touching the engine.

### 3.3 Terminology

| Term | Definition |
|---|---|
| **Replay archive** (archive) | A self-contained, versioned package — published as a single DwarFS image; a plain directory is the local working form (§4.1) — containing the workload, expected outcomes, dependency data, optional embedded store content, and provenance for one recording. Specified in §4. |
| **Recorder** | A source-specific program that produces archives. Today: the Hydra eval pipeline and nxb-replay (§5). Recorders own all knowledge of their source. |
| **Replayer** / **replay engine** | The single in-cluster program (the `rio-replay` run subsystem) that executes campaigns over archives (§6 The replayer). |
| **Campaign** (replay campaign) | One execution of the replayer over one archive against one target cluster under one set of policies, identified by a campaign id, with durable state and a report. |
| **Workload unit** (unit) | One derivation the archive directs the replayer to realize and judge, identified by its `.drv` store path. The workload is the union of all requests' targets. |
| **Request** | One recorded client submission: a set of workload units submitted together, attributed to a session, optionally at a recorded time offset. The unit of timed scheduling (§9 Scheduling). |
| **Submission** | One channel-level build request the engine issues to the gateway: a packed batch of workload units in timeless mode, one recorded request in timed mode (§6.4, §9). |
| **Session** | An opaque integer grouping requests that were submitted over one recorded client connection. Carries no identity or tenancy semantics in v1 (§4.10). |
| **Expected outcome** | The per-unit outcome recorded in the archive as truth, expressed in the neutral vocabulary of §4.6 (`built`, `failed`, `resource-exhausted`, `cancelled`, `disconnected`, `indeterminate`, `unknown`). |
| **Actual outcome** | What the campaign observed for a unit on the target cluster. |
| **Verdict** | The per-unit result of comparing expected outcome against actual outcome (§7 Comparison model). |
| **Disposition** | The per-unit explanation of why a unit was not attempted or does not count toward verdicts (closure overlap, cached prior, substituted, demoted, filtered, …) (§7 Comparison model). |
| **Report policy** | A campaign-level aggregation chosen at launch: the parity report (headline percentage with denominator rules and a comparability block), the regression gate (fail-on semantics), or both (§7 Comparison model). Policies are never baked into archives. |
| **Truth** | Shorthand for the expected outcomes and expected output hashes stored in the archive. Truth is established by the recorder at creation time. |
| **Thin archive** | An archive that embeds only content no configured substituter can provide; everything else is supplied from the target's substituters or relayed from the archive's listed relay substituters at campaign time. |
| **Fat archive** | An archive that embeds everything the workload's closures need beyond what the target must build itself, so a campaign needs no relay substituters. Fat archives are the durable, hermetic form. |
| **Timed archive** | An archive whose requests carry meaningful recorded offsets (capability `timed`), enabling the timed scheduling mode (§9 Scheduling). |
| **Timeless archive** | An archive without timing; only the timeless (drain) scheduling mode applies. |
| **Supply** | Making a path the target is not being measured on (a dependency, an input source, or the output of a non-attemptable unit) available on the target before or during execution (§8 Supply planning). |
| **Prefetch** | Supply performed scheduler-side by submitting paths for bulk substitution from the target's own substituters (today's warm stage). Bytes never transit the engine. |
| **Relay** | Supply performed engine-side by fetching a NAR from one of the archive's relay substituters and uploading it over the worker protocol. |
| **Provenance** | The opaque metadata block recorders write into the manifest for humans and audit. Never interpreted by the engine. |
| **Recorder mapping** | The translation a recorder performs from its native status/truth representation into the neutral expected-outcome vocabulary, at creation time. |

Naming: the subsystem as a whole is called **build replay**, and the implementation and deployment identifiers carry the neutral name set defined in §11.7 — the Phase 5 operator-convergence cutover that renamed the legacy `parity` identifiers has landed. The engine crate is `rio-replay` (§6.1), the namespace, ServiceAccount, Secret and mount names are `rio-replay` / `rio-replay-ssh` / `/etc/rio/replay/…` (§10.2), the S3 root is `replay/`, and the campaign tenants are `replay-*`. The term "parity" survives only as the name of the parity report policy (the headline-percentage aggregation); it does not name the archive format, the comparison vocabulary, the operator command family, or any concept a recorder or report consumer sees.

## 4. Archive format v1

### 4.1 Container and logical layout

An archive is a tree of member files with a fixed logical layout. The published, at-rest form is a single DwarFS image of that tree: every v1 archive in S3 is one `.dwarfs` object (plus the small standalone control objects of §4.11), produced with `mkdwarfs` and read in-process via the `dwarfs` crate (already a workspace dependency on the xtask-replay branch). The plain-directory form of the same tree is a local working representation only — recorder staging before packing, dev fixtures, dry-runs, CI — and is never the published S3 form. The reader supports both backends with identical member paths and identical semantics, so engine and tooling code never care which form a local input takes:

- **Image form** (published): a single DwarFS image. The only form an archive takes at rest in S3 (§4.11) and the form in-cluster campaigns consume.
- **Directory form** (local working/dev only): a plain directory holding the members. Used for recorder staging, committed fixtures, `--dry-run`, and dev-mode inputs; never uploaded as-is.

Logical layout (member paths are exact):

```
manifest.json                      required   archive metadata, capabilities, provenance, integrity
requests.jsonl                     required   the recorded workload requests
outcomes.jsonl                     optional   expected outcomes (truth), neutral vocabulary
units.jsonl                        optional   per-unit display/filter metadata (labels, system, outputs, features)
closures.jsonl                     optional   direct dependency adjacency for the union closure
impure-env.json                    optional   drv → impure environment variable names
exclusions.jsonl                   optional   scope items the recorder could not turn into workload units
narinfo/<hash>.narinfo             required for every embedded non-drv store path (also accepted: <hash>-<name>.narinfo)
nix/store/<hash>-<name>.drv        required   derivation ATerm text; full requisite drv closure of every unit
nix/store/<hash>-<name>/…          optional   embedded store paths, unpacked trees
```

Unknown member files are permitted and ignored by the replayer; recorders may add their own QA artifacts (the Hydra recorder keeps `fidelity.json` this way, §5.1). Unknown JSON fields inside known members are ignored. All v1 JSON uses `snake_case` field names and UTF-8; `.jsonl` members are one JSON object per line.

### 4.2 manifest.json

| Field | Type | Required | Meaning |
|---|---|---|---|
| `format_version` | string `"MAJOR.MINOR"` | yes | `"1.0"` for archives written to this spec. The reader refuses an unknown major and accepts any minor of a known major (additive evolution: new optional fields, new optional members, new capability flags). Absence of this field identifies a v0 archive (§4.12). |
| `created_at` | string, RFC 3339 timestamp | yes | When the archive was created. |
| `from` | string, RFC 3339 timestamp | yes | Start of the recorded window. For timeless archives, recorders set `from == to` to the time the source state was captured (e.g. the start of the truth sweep). |
| `to` | string, RFC 3339 timestamp | yes | End of the recorded window. `offset_s` and `stop_offset_s` values are relative to `from`. |
| `capabilities` | object | yes | Capability flags, §4.3. |
| `counts` | object | yes | Informational counts, §4.4. Mismatches with member contents are warnings, not errors. |
| `substituters` | object | no (default both lists empty) | `{ "relay": [string], "target": [string] }`. `relay`: binary caches the engine may relay content from at campaign time (https:// or s3:// URLs; the engine refuses plain http://). `target`: caches the recorder expects the target cluster's tenants to have configured; advisory — the campaign spec is authoritative for the target's substituters. |
| `fat` | bool | no (default `false`) | Recorder's claim that the archive embeds everything required beyond what the target must build itself (§4.9). Advisory; the supply planner verifies coverage path-by-path regardless (§8 Supply planning). |
| `provenance` | object | yes | Opaque. Carried verbatim into campaign records and reports; never interpreted by the engine. Recommended keys (not load-bearing): `recorder` (string), `recorder_version` (string), `description` (string), `source` (object), `recipe_digest` (string), `references` (array of strings). Source-specific names and identifiers belong here and only here. |
| `files` | object: member path → `{ "sha256": string, "size": u64 }` | yes | Integrity table over the metadata members present in the archive: `requests.jsonl`, `outcomes.jsonl`, `units.jsonl`, `closures.jsonl`, `impure-env.json`, `exclusions.jsonl`. `sha256` is the lowercase hex SHA-256 of the member's bytes; `size` its byte length. Members not present are omitted. `manifest.json` itself is never listed. |
| `content_digests` | object | yes | Aggregate digests over the bulk content and the narinfo sidecars, §4.5. Together with `files`, makes the archive identity a content address. |

There is no source, flavor, or recorder-kind field outside `provenance`, by design.

### 4.3 Capability flags

`capabilities` is an object of booleans, all defaulting to `false` when absent. Flags state what the archive contains; the engine gates modes and comparison features on them. Recorders set exactly the flags whose backing data they actually wrote.

| Flag | Meaning | What it gates |
|---|---|---|
| `timed` | `requests.jsonl` offsets are meaningful recorded times (and `outcomes.jsonl` may carry `stop_offset_s`). | The timed scheduling mode, cancellation/disconnect reproduction, dispatch-lateness accounting (§9 Scheduling). Timeless archives can only run in drain mode. |
| `expected_outcomes` | `outcomes.jsonl` is present and is authoritative truth for the workload. | Verdict comparison (§7 Comparison model). Without it every unit ends in a no-truth verdict; the campaign is a load/exercise run. |
| `output_hashes` | `built` expected outcomes carry per-output NAR hashes. The flag asserts that every `built` outcome the recorder could hash carries `outputs`; readers treat per-record absence as not-comparable, never as a format error. | Output-divergence verdicts (§7 Comparison model). |
| `embedded_store_paths` | `nix/store/` contains embedded non-drv store paths, each with a narinfo sidecar. | The archive rung of the supply ladder (§8 Supply planning). |
| `impure_env` | `impure-env.json` is present. | Impure demotion (§7, §8). |
| `dependency_closures` | `closures.jsonl` is present and covers the full union closure of the workload. | Plan-time closure computation (batching, overlap analysis, supply planning) without parsing every embedded ATerm (§6 The replayer). When false the engine falls back to walking the embedded `.drv` files. |

Additive minor versions may introduce new flags; readers ignore flags they do not know.

### 4.4 Counts

`counts` is informational and exists so operators and tools can size a campaign without scanning members.

| Field | Type | Meaning |
|---|---|---|
| `requests` | u64 | Number of records in `requests.jsonl`. |
| `workload_units` | u64 | Number of distinct workload units across all requests. |
| `expected_outcomes` | u64 | Number of records in `outcomes.jsonl` (0 when absent). |
| `embedded_drvs` | u64 | Number of `.drv` members under `nix/store/`. |
| `embedded_store_paths` | u64 | Number of embedded non-drv store paths. |

### 4.5 Identity, integrity, and content addressing

The archive identity is derived from content, not chosen by the recorder:

- **`archive_id`** = the lowercase hex SHA-256 of the bytes of `manifest.json` exactly as stored in the archive. The **short id** is its first 16 hex characters and is what appears in S3 prefixes, campaign pins, and operator output. `archive_id` is not (and cannot be) a field inside `manifest.json`; it is recorded in the upload completion marker (§4.11) and in campaign records.
- Because the manifest embeds `files` (per-member digests of the metadata members) and `content_digests` (aggregate digests of the bulk content and the narinfo sidecars), `archive_id` is a Merkle-style content address over the whole archive: any change to any covered member changes the id (extra, ignored members such as a recorder's own QA artifacts are deliberately outside the identity).
- `content_digests` has exactly three fields:
  - `drvs`: lowercase hex SHA-256 over the canonical listing of embedded derivations — one line per `.drv` member, `"<store path> <lowercase hex sha256 of the ATerm bytes>"`, sorted lexicographically by store path, joined with `"\n"`, with a trailing newline.
  - `embedded_store_paths`: the same construction over embedded non-drv paths, where the per-path digest is the lowercase hex SHA-256 of the path's uncompressed NAR serialization (the same value the sidecar's `NarHash` encodes).
  - `narinfo`: the same construction over the narinfo sidecar files present in the archive — one line per sidecar, `"<StorePath> <lowercase hex sha256 of the sidecar file's bytes>"`, sorted lexicographically by store path, joined with `"\n"`, with a trailing newline. This puts the sidecars' load-bearing `References` lines (which order client uploads) under the identity.
  The digest of an empty listing is the SHA-256 of the empty string for all three fields.
- Integrity at use time is layered: the metadata members are checked against `files`, and the narinfo sidecar listing against `content_digests.narinfo`, when the archive is opened (sidecars are small text files; hashing the listing is cheap); embedded non-drv paths are checked against their sidecar `NarHash`/`NarSize` when the engine serializes them for upload (and the daemon verifies again on ingest); derivation members are self-certifying because an input-addressed `.drv` store path commits to its ATerm contents, and a corrupted text surfaces as a path mismatch at import time.
- The identity is independent of the container: the directory form and the image form of the same archive have the same `archive_id`. Filenames (including the `.dwarfs` filename) are not load-bearing.
- Two recordings of the same source state are distinct archives with distinct ids (`created_at` alone guarantees this); recipe-level idempotency for recorders that want it is a recorder concern (§5.1), not a format property.

### 4.6 Expected outcomes and the neutral vocabulary

`outcomes.jsonl` carries truth: one record per (session, unit) the recorder can speak for. Recorders map their native codes into the neutral vocabulary at creation time; the engine never sees a native code.

Record schema:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `session` | i64 or null | no (default null) | When non-null, the expectation applies to that recorded session's request for this unit (the same unit may have different outcomes in different sessions). When null, it applies to any request of the unit. Lookup order in the engine: exact `(session, drv)`, then `(null, drv)`. |
| `drv` | string | yes | The unit's `.drv` store path. |
| `outcome` | string | yes | One of the neutral vocabulary values below. |
| `detail` | string | no | Free-form, human-readable detail (the recorder's native status text or code, failure phase, etc.). Never interpreted. |
| `duration_s` | f64 | no | Wall-clock duration of the source attempt, seconds. Used to size build deadlines; useful even in timeless archives. |
| `stop_offset_s` | f64 | no | Offset from `from` at which the source attempt stopped. Meaningful only in timed archives; used to reproduce cancellations and disconnects (§9 Scheduling). |
| `outputs` | object: output name → `{ "nar_hash_hex": string, "nar_size": u64 }` | no | Expected per-output content for `built` outcomes: lowercase hex SHA-256 of the uncompressed NAR and its byte size. Presence across the archive is what `output_hashes` asserts. |

Duplicate `(session, drv)` keys: last record wins, as in v0.

The session-aware lookup above serves consumers acting for a specific recorded request (the timed scheduler). The timeless engine resolves truth per workload *unit* — it has no request identity to probe with — through one canonical collapse over sessions: the session-less record when one exists (it explicitly applies to any request of the unit), otherwise the record of the highest-numbered session (sessions are opaque grouping keys, but recorders allocate them in capture order, matching the last-record-wins rule above). Scoped records that disagree with the chosen outcome are logged, since the collapse is then losing information the timeless engine cannot represent.

The neutral expected-outcome vocabulary:

| `outcome` | Meaning | How a campaign can use it |
|---|---|---|
| `built` | The source completed the unit successfully. `outputs` should be present when the recorder knows the produced hashes. | Full comparison: success/failure agreement, plus output-hash agreement when `output_hashes`. |
| `failed` | The unit failed at the source for reasons attributable to the unit itself (a deterministic build failure). | Comparison: a matching failure or an unexpected success. |
| `resource-exhausted` | The unit failed at the source by hitting a source-side resource limit (memory, disk, build-time quota) rather than a deterministic build error. | Failure-class expectation: compared exactly like `failed` (§7.1) — a replayed failure is `match-failed`, a replayed success is `unexpected-success` — but reportable separately, so source resource limits never hide inside the deterministic-failure counts. |
| `cancelled` | The source attempt was cancelled before completion. | No deterministic expectation: in timeless runs the unit classifies `truth-indeterminate` (§7); in timed mode the cancellation itself is reproduced at `stop_offset_s` (§9 Scheduling). |
| `disconnected` | The recording client disconnected before the unit finished; the source's terminal outcome is unknown. | Not a truth claim about the build. In timed mode the disconnect is reproduced by channel abandon at `stop_offset_s` (§9 Scheduling); otherwise the unit classifies `truth-indeterminate` (§7). |
| `indeterminate` | The source attempt ended for infrastructure reasons (builder error, internal error of the source) and cannot serve as truth. | `truth-indeterminate` verdict (§7); reported separately from `unknown` so source-quality and recorder-coverage problems stay distinguishable. |
| `unknown` | The recorder looked and could not determine an outcome (e.g. no cache entry and no status available). | No-truth verdict; counts toward the campaign's truth-coverage quality metrics. |

Two adjacent states are deliberately representable: a unit with an `unknown` record (the recorder examined it and could not decide) versus a unit with no record at all (the recorder did not cover it). Both produce no-truth verdicts; reports may count them separately.

`outcomes.jsonl` describes workload units only. The expected state of dependency paths is conveyed by narinfo sidecars and substituter coverage, not by outcome records.

### 4.7 Requests, units, closures, impure environment, exclusions

**`requests.jsonl`** — one record per recorded client submission, the unit of scheduling:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `session` | i64 | no (default 0) | Opaque grouping key for the recorded client connection. No tenancy semantics in v1 (§4.10). |
| `offset_s` | f64 | no (default 0.0) | Seconds after `from` at which the request was issued. Negative values are clamped to 0 at load. Meaningful only when `timed`; timeless recorders omit it. |
| `targets` | array of `{ "drv": string, "outputs": [string] }` | yes, non-empty | The derivations (and outputs) the client asked for. `outputs` of `[]` or `["*"]` both mean all outputs; writers should normalize to `["*"]`. |

The workload is the union of all `targets[].drv` across requests. The engine's timeless mode is free to coalesce and re-batch requests; the timed mode replays them as recorded (§9 Scheduling).

**`units.jsonl`** (optional) — one record per workload unit, carrying display and filtering metadata that is not derivable cheaply from the ATerm members:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `drv` | string | yes | The unit's `.drv` store path. |
| `label` | string | no | Human-facing name (for Hydra-derived archives, the job name, e.g. `python3Packages.requests.x86_64-linux`). Used by include/exclude filters and reports. |
| `system` | string | no | The unit's platform (e.g. `x86_64-linux`). |
| `outputs` | object: output name → store path | no | Statically declared output paths. |
| `required_features` | array of string | no | Required system features (from `requiredSystemFeatures`), used by feature-exclusion filters. |
| `identity_divergent` | bool | no (default `false`) | Set by the recorder when its fidelity gate found this unit's derivation identity divergent from the source it recorded. The engine assigns such units the `identity-divergent` disposition and never compares them (§7.2). |

Records for derivations that are not workload units are ignored with a warning. Archives without `units.jsonl` (or whose records cover only part of the workload) are fully usable: the engine plans the requests-derived workload regardless, recovering each uncovered unit's `system`, declared outputs, and required features from its embedded `.drv` ATerm — a workload unit with neither a `units.jsonl` record nor a readable embedded derivation is a per-unit hard error, because the archive then cannot say what the unit produces. Filters that need labels degrade to matching on store-path names.

**`closures.jsonl`** (optional, capability `dependency_closures`) — direct dependency adjacency for every derivation in the union requisite closure of the workload (workload units included), one record per derivation:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `drv` | string | yes | Derivation store path. |
| `inputs` | array of string | yes (may be empty) | Direct input derivations (`inputDrvs` keys). |
| `srcs` | array of string | no (default `[]`) | Direct input sources (`inputSrcs`): non-drv store paths this derivation needs. |
| `outputs` | object: output name → store path or null | yes | Statically declared output paths; `null` for floating content-addressed outputs whose path is not known statically. |

The adjacency form is chosen over per-unit transitive closure lists deliberately: it stores each derivation once (size linear in the closure, not in closure × units), and the engine reconstructs per-unit transitive closures, overlap sets, warm sets, and batch size estimates with one in-memory traversal at plan time (§6 The replayer). When the member is absent the engine derives the same information by parsing the embedded `.drv` files, exactly as the v0 replayer does.

**`impure-env.json`** (optional, capability `impure_env`) — a single JSON object mapping derivation store path → array of impure environment variable names the derivation declares. Identical to v0. Units listed here are demoted: their recorded outputs are supplied like dependencies and they are never judged as regressions (§7 Comparison model, §8 Supply planning).

**`exclusions.jsonl`** (optional) — scope items the recorder intended to include but could not turn into workload units, so the campaign's completeness accounting can see them:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `label` | string | at least one of `label`, `drv` | Human-facing name of the excluded item. |
| `drv` | string | — | Derivation path when one is known. |
| `reason` | string | yes | Recommended values: `eval-error`, `aggregate`, `unsupported`. Free-form values are permitted. |
| `detail` | string | no | Human-readable detail (e.g. the evaluation error message). |

When the member is present, its records enter the campaign's completeness and comparability accounting exactly as today's eval-error accounting does (they surface as excluded counts in the comparability block, §7.3). When it is absent, the comparability block notes the member's absence and applies no penalty — archives whose recorders cannot enumerate exclusions are not scored worse for it.

### 4.8 Embedded store content and narinfo sidecars

- `nix/store/<hash>-<name>.drv`: derivation ATerm text. The full requisite `.drv` closure of every workload unit MUST be embedded — the replayer imports derivations from the archive and never evaluates anything.
- `nix/store/<hash>-<name>/…`: embedded store paths as unpacked trees (regular files, executable bits, symlinks). The engine NAR-serializes them on demand when uploading.
- `narinfo/<hash>.narinfo` (the `<hash>-<name>.narinfo` spelling is also accepted): one sidecar per embedded non-drv store path, in standard narinfo text form. Required fields: `StorePath`, `NarHash` (`sha256:` in nix base32), `NarSize`, `References`. `URL` and `Compression` are optional for embedded paths (the reader synthesizes a placeholder URL); `Deriver` and signature fields are optional. Sidecars for non-embedded paths are permitted and treated as advisory metadata. Unparseable sidecars are warned about and skipped, as in v0.

### 4.9 Thin and fat archives

Both are first-class:

- A **thin** archive embeds the `.drv` closure plus only those store paths that no configured substituter can provide (for nixbuild.net recordings, the client-uploaded source residue; for Hydra recordings, typically nothing). Campaigns over thin archives lean on the target's substituters (prefetch) and on the archive's `substituters.relay` list (engine-side relay) for everything else.
- A **fat** archive embeds every path the workload's closures need other than the workload outputs themselves, so a campaign needs no relay substituters at all and survives the source caches changing or disappearing. A fat Hydra-derived archive that replays with cache.nixos.org unreachable is an explicitly supported and desirable configuration for durability.
- `fat: true` in the manifest is the recorder's claim of the latter property. The supply planner does not trust the claim: coverage is verified path-by-path and any gap is reported as a supply failure with a disposition (§8 Supply planning).
- In neither case are workload outputs embedded-and-supplied to the target: the measurement rule (never supply what the target is being asked to build) is a replayer policy (§8), but recorders SHOULD avoid embedding workload outputs in fat archives to keep them honest and small. Embedding them is not an error; the planner never uploads outputs of units that remain attemptable after closure-overlap resolution (§8.1).

### 4.10 Sessions and future tenancy

`session` in v1 is only a grouping key: it lets timed campaigns reproduce per-connection request ordering and lets expected outcomes be scoped to a specific recorded attempt. It carries no identity, tenant, or credential semantics, and the engine maps all sessions onto the campaign's single build tenant. Multi-session identity mapping (replaying rio-against-rio with per-session tenants) is deliberately out of scope; when it arrives it will take the form of an additional member (e.g. a session table mapping session ids to tenancy descriptors) plus a format version bump, and no v1 field will need to change meaning.

### 4.11 S3 layout, write-once upload, completion marker

Archives at rest live in S3 under a digest-keyed, write-once prefix, mirroring the eval-set discipline. The at-rest representation is always the DwarFS image (§4.1); the only other objects at the prefix are two small standalone control objects so that listing and probing never require fetching the image:

```
s3://<bucket>/<root>/archives/<archive_id_short>/
    manifest.json          always present; byte-identical to the manifest member inside the image (the identity bytes)
    archive.dwarfs         the packed archive (DwarFS image of the archive tree)
    complete.json          always present, always uploaded last
```

- `<root>` is the deployment's configured S3 prefix. It is configuration, not format; deployments use `replay` (IRSA grant `replay/*`) — the §11.7 cutover renamed the root from the legacy `parity` and updated the IAM/tofu grant alongside the other identifier renames. The campaign-state prefix `<root>/campaigns/<campaign-id>/` keeps its shape under any root.
- The standalone `manifest.json` is byte-identical to the `manifest.json` member inside the image — same bytes, same `archive_id` (§4.5). It exists so tooling can inspect capabilities, counts, and provenance, and the launcher can validate a pin, without downloading the image.
- Upload order: `archive.dwarfs` first, `manifest.json` next, `complete.json` strictly last with a conditional PUT (`If-None-Match: *`). A prefix without `complete.json` is incomplete and is never used by the engine or listed as available by tooling. Two racing uploads cannot both claim completeness.
- Prefixes are write-once: an uploader that finds `complete.json` already present refuses to overwrite. Re-recording produces a new archive with a new id and therefore a new prefix.
- `complete.json` schema:

| Field | Type | Meaning |
|---|---|---|
| `archive_id` | string | Full 64-hex archive id (§4.5). Verified by readers against the downloaded `manifest.json`. |
| `archive_id_short` | string | First 16 hex characters; must equal the prefix segment. |
| `objects` | object: object name → `{ "sha256": string, "size": u64 }` | Every object at the prefix except `complete.json` itself (the image and the standalone manifest), with digests and sizes for download verification. |
| `uploaded_at` | string, RFC 3339 | Upload completion time. |
| `uploader` | string | Free-form tool/version string. |

- Engine fetch sequence: read `complete.json`; download the objects it lists, verifying each against `objects`; verify `sha256(manifest.json) == archive_id` and that it matches the manifest member inside the image; open the image in place with the DwarFS reader backend — there is no unpack step. Content-Type conventions follow the eval-set uploader: `application/json` for `.json`; `application/octet-stream` for `.dwarfs`.
- Local archives (a `.dwarfs` image or a plain directory on disk) remain valid engine and tooling inputs for dev mode, fixtures, and CI; the directory form never leaves the machine it was staged on (§4.1), and S3 — always the image — is the transport for in-cluster campaigns (§10 Operator surface).

### 4.12 v0 compatibility

v0 is the xtask-replay archive contract as written today by nxb-replay and committed as the `xtask/tests/fixtures/replay/basic` fixture. The engine continues to accept v0 archives indefinitely via an upgrade-on-open shim: a manifest without `format_version` is parsed as v0 and mapped into the v1 in-memory model, so all engine code paths downstream of the reader are v1-only.

| Aspect | v0 | v1 | v0 handling on open |
|---|---|---|---|
| Version marker | none | `format_version: "1.0"` | absent ⇒ v0 path |
| Capabilities | none (implied by file presence) | explicit `capabilities` object | inferred: `timed` = true; `expected_outcomes` = `builds.jsonl` present; `output_hashes` = any build record carries outputs; `embedded_store_paths` = any embedded non-drv path present; `impure_env` = `impure-env.json` present; `dependency_closures` = false |
| Truth member | `builds.jsonl`, `status: i32` (source-native codes) | `outcomes.jsonl`, `outcome: string` (neutral) | codes mapped: `0 → built`, `6 → cancelled`, `10 → indeterminate`, `13 → disconnected`, `16 → resource-exhausted`, any other non-zero → `failed` (detail records the code). `status_msg` → `detail`; `duration_s`, `stop_offset_s`, `outputs` carried over unchanged. |
| Requests | `ssh_session_id`, `offset_s`, `paths: [[drv, [outputs]]]` | `session`, `offset_s`, `targets: [{drv, outputs}]` | field renames and pair→object mapping |
| Manifest substituters | `src_substituters`, `target_substituters` | `substituters.relay`, `substituters.target` | renamed |
| Manifest counts | flat `requests`, `drvs`, `embedded_srcs` | `counts` object | mapped (`drvs → embedded_drvs`, `embedded_srcs → embedded_store_paths`; `workload_units` recomputed) |
| Provenance | none (unknown fields ignored) | required `provenance` object | synthesized as `{}`; unknown v0 manifest fields are preserved nowhere (they were already ignored) |
| Integrity / identity | none | `files`, `content_digests`, `archive_id` | not synthesized; v0 archives have no content-addressed identity and are referenced by path. Uploading a v0 archive to the v1 S3 layout is not supported; convert or re-record to publish to S3. |
| Units / closures / exclusions | none | optional members | absent ⇒ capabilities false, fallbacks apply |
| `fat`, `from`, `to`, `created_at`, `impure-env.json`, narinfo sidecars, `nix/store/` layout | present | unchanged | carried over |

v0 status 16 maps to `resource-exhausted`, the dedicated v1 value (§4.6), with the original code preserved in `detail`. Because the comparison model treats an expected `resource-exhausted` exactly like an expected `failed` (§7.1) while keeping it reportable separately, the v0 replayer's comparison behavior is preserved within v1.

The upgrade-on-open shim itself exists for previously-recorded nxb-replay archives of past production windows, which cannot be re-recorded; that is recorded data, not deployed-component state, so the full-wipe deployment policy (§2.5) does not retire it.

### 4.13 Size envelopes

Planning estimates, not commitments. Assumptions: ~0.5 KB per `.drv` after compression (measured for today's eval-set archives), union closures of roughly 2–10 k paths for an M1-scale scope, 50–150 k for an M2-scale scope, and 300–600 k for a platform slice; ~200 GB uncompressed NAR per system for the outputs of a full rebuild (capacity-planning figure), of which a fat archive embeds the dependency surface but never the workload outputs.

| Scale | Workload units | Union drv closure (paths) | Thin archive (packed) | Fat archive (packed) |
|---|---|---|---|---|
| M1 smoke | 10–50 | ~2–10 k | tens of MB | ~1–5 GB |
| M2 (aggregate constituents) | ~500–5 000 | ~50–150 k | 100–500 MB | ~10–50 GB |
| Platform slice | 30–100 k | ~300–600 k | 0.5–2 GB | ~100–300 GB |

The dominant thin-archive members at slice scale are `closures.jsonl` and the embedded `.drv` texts, both of which compress heavily inside the DwarFS image. Fat slice archives exceed the campaign Job's current 100 Gi work volume and need either a larger ephemeral volume at launch time or ranged/streamed reads of the image straight from S3; this is recorded as R3 in §12.1.

## 5. Recorders

Recorders are source-specific by nature; this section describes the two that exist and the one the format anticipates. Nothing in this section is visible to the replayer except through the archive members and capability flags the recorder writes.

### 5.1 The Hydra evaluation pipeline as a recorder

The eval-set machinery (`rio-replay eval`, the Job behind `cargo xtask replay record`) is the first-party recorder for nixpkgs/Hydra-derived archives. Its job description changed from "produce an eval set the campaign engine knows how to interpret" to "produce a v1 replay archive with truth baked in".

**What stays unchanged:**

- The Hydra politeness budget: descriptive User-Agent, hard request cap (default 150, auto-raised for explicit job lists), 500 ms minimum spacing, and the rule that mass per-path data comes from cache.nixos.org, never Hydra.
- The reproduction recipe: pinned nixpkgs tarball download, `nix store add-path --name source`, recovered `revCount`/`shortRev`, generated `selection.nix`, `nix-eval-jobs` execution, aggregate exclusion.
- The fidelity gate: per-job drvPath comparison against Hydra (exhaustive for scoped sets, sampled for full sets); a divergent set is still written and uploaded but flagged, and the CLI still exits non-zero. Units an exhaustive check finds divergent are written with `identity_divergent: true` in `units.jsonl` (§4.7), which is what drives the `identity-divergent` disposition (§7.2). The gate's pass requires a nonzero coverage witness: if jobs are in scope but the comparison joined zero of them (e.g. a job-name format skew between the local manifest and Hydra's truth), the recorder aborts before the truth sweep — nothing is staged or uploaded — instead of publishing an unverified archive.
- The recipe key (today's `EvalSetKey`: eval id, project, jobset, systems, scope, tool versions, args/expression hash) and its digest. It moves into `provenance.recipe_digest` and remains the recorder's idempotency handle.
- The dependency-closure pass (`nix derivation show -r`) and the requiredFeatures backfill — they now feed `closures.jsonl` and `units.jsonl` instead of `dep-closure.jsonl` and `manifest.jsonl`.
- The scoped/full scope model, `--dry-run`, and the eval Job sizing/operational shape (§10 Operator surface).

**What changes:**

| Today (eval set) | v1 recorder |
|---|---|
| Truth fetched at campaign time: the engine's hydra-truth stage sweeps cache.nixos.org narinfo for every target and warm path into `hydra.jsonl`; scoped campaigns optionally read a buildstatus file. | Truth baked in at creation: the recorder performs the narinfo sweep (same concurrency and retry discipline as today's stage) and the optional buildstatus ingestion **before packaging**, and writes `outcomes.jsonl`: all outputs present upstream → `built` with `nar_hash_hex`/`nar_size` taken from the narinfo; any output missing → `unknown`; buildstatus 0 → `built`; non-zero buildstatus → `failed` with the numeric code in `detail`. Campaigns never query cache.nixos.org or Hydra for truth again. |
| `manifest.jsonl` (job, system, attr, drvPath, outputs, requiredFeatures) | `units.jsonl` (label = job, system, outputs, required_features) plus one synthesized record per unit in `requests.jsonl` (`session: 0`, `offset_s` omitted, `targets: [{drv, outputs: ["*"]}]`). The archive is timeless: `capabilities.timed = false`, `from == to`. |
| `dep-closure.jsonl` (per-target transitive closure, camelCase) | `closures.jsonl` (direct adjacency over the union closure, snake_case), `capabilities.dependency_closures = true`. |
| `drvs.tar.zst` (a `nix copy --derivation` file:// binary-cache layout, tarred) | Plain ATerm members under `nix/store/*.drv` covering the full requisite closure of every unit. The recorder copies the `.drv` files directly out of its local store; no binary-cache layout is constructed. |
| `eval-errors.jsonl` | `exclusions.jsonl` with `reason: "eval-error"`; excluded aggregates get `reason: "aggregate"`. |
| `evalset.json` (audit metadata + completeness marker) | The audit content (jobset config snapshot, evaluator argv, source store path, rev info, stats, fidelity result) moves into `provenance`; `fidelity.json` is kept verbatim as an extra, ignored member; completeness marking moves to `complete.json` in the S3 layout (§4.11). |
| S3 prefix `<root>/evals/<hydra-eval-id>/<key-short-digest>/`, write-once via `evalset.json` | S3 prefix `<root>/archives/<archive_id_short>/`, write-once via `complete.json`. The recorder packs its staged archive directory into a DwarFS image with `mkdwarfs` and uploads the image plus the standalone `manifest.json`/`complete.json` (§4.11); the eval Job image gains a pinned `mkdwarfs` for this, landing with Phase 3 (§11.5, R2). Recipe-level idempotency ("has this recipe already been recorded?") is recorder-owned: the recorder maintains a pointer object at `<root>/archives/by-recipe/<recipe_digest>.json` containing `{ "archive_id", "archive_id_short", "recorded_at" }`, written after `complete.json` and read before re-recording; the engine never reads it. `--force` continues to salt the recipe key. |
| Substituter knowledge implicit (campaign spec carries `hydra.cache_url`) | `substituters.relay = ["https://cache.nixos.org"]` (or the configured cache) and `substituters.target` as the recorder was told the target tenants use; `fat: false` by default. A `--fat` recording mode that also embeds the dependency surface (fetching NARs from the cache at creation time) is supported by the format and is the recommended way to produce durable archives for long-lived regression baselines. |

Capability flags written by this recorder: `expected_outcomes = true`, `output_hashes = true` (narinfo always carries NarHash), `dependency_closures = true`, `impure_env = true` when any unit declares `impureEnvVars` (the recorder extracts this from the parsed derivations during the closure pass), `embedded_store_paths` per thin/fat mode, `timed = false`.

Eval sets already in S3 are not migrated: per the deployment policy (§2.5), pre-v1 eval-set prefixes are simply abandoned, and a scope still wanted as a campaign input is re-recorded as a v1 archive (§11.5).

### 5.2 nxb-replay

nxb-replay is the external recorder that captures nixbuild.net production windows; it already writes the v0 contract and keeps working unchanged in its capture mechanism, thin/fat embedding policy, `mkdwarfs` packaging, narinfo sidecars, and impure-env extraction. Archives it has already produced remain consumable through the v0 shim (§4.12).

To emit v1 it needs only format-surface changes at write time:

- Write `format_version: "1.0"`, the `capabilities` object (`timed = true`, `expected_outcomes = true`, `output_hashes = true`, `embedded_store_paths` per thin/fat, `impure_env` when applicable, `dependency_closures = false` unless it chooses to emit `closures.jsonl`), the `counts` object, `files`, `content_digests`, and a `provenance` block (free to carry its source identifiers, region list, and tool version — that is exactly what provenance is for).
- Rename `ssh_session_id` → `session` and `paths` pairs → `targets` objects in `requests.jsonl`; nest `src_substituters`/`target_substituters` under `substituters.relay`/`substituters.target`.
- Write `outcomes.jsonl` instead of `builds.jsonl`, mapping its native status codes to the neutral vocabulary:

| nxb-replay status | Meaning at source | v1 `outcome` | `detail` |
|---|---|---|---|
| 0 | Built | `built` | — |
| 1 | Permanent build failure | `failed` | `"status=1"` or the source's status text |
| 4 | Output rejected | `failed` | `"status=4 output rejected"` |
| 6 | Cancelled | `cancelled` | — |
| 10 | Builder error | `indeterminate` | `"status=10 builder error"` |
| 13 | Client disconnect | `disconnected` | — |
| 16 | Resource exhaustion | `resource-exhausted` | `"status=16"` or the source's status text |
| any other non-zero | Deterministic failure | `failed` | the original code |

Everything else about the tool — its record-side SSH/postgres access, window selection, staging layout, archive naming, and its own replay subcommand — is explicitly out of scope here and unchanged. It remains an external tool; nothing requires moving it into this repository.

### 5.3 A future rio-native recorder

Out of scope for this design. The format already anticipates it:

- rio's gateway sees the same worker-protocol requests nxb-replay reconstructs from its source's logs, so a recorder embedded in or beside the gateway can emit `requests.jsonl`/`outcomes.jsonl` directly, with `timed = true` and exact per-session grouping.
- The capability flags let such a recorder start minimal (workload + outcomes, no embedded content, no closures) and grow without format changes.
- Per-session tenancy capture is the one thing v1 cannot express; it is the designated trigger for the next format version (§4.10).
- Provenance gives it a place to record cluster, tenant, and version identifiers without the engine ever depending on them.

## 6. The replayer

The replayer is the existing campaign engine — the `rio-replay` crate — evolved. It remains a single in-cluster k8s Job driven by a spec ConfigMap, checkpointing to S3, and producing the same operational artifacts (`progress.json`, `report/`, append-only JSONL state). What changes is what it consumes (a replay archive instead of a raw eval set plus a campaign-time truth sweep), how it talks to the cluster (rio-nix client ops over an in-process SSH pool instead of shelled-out `nix copy`/`nix build`), how it collects results (in-band per-derivation results instead of stderr scraping plus `GetBuildGraph` reconstruction), and how it schedules work (a second, timed dispatch mode next to the existing drain loop). This section defines the post-convergence architecture; §11 (Migration & sequencing) defines the order in which the pieces land.

### 6.1 What is kept from the campaign engine

Everything operational about the campaign engine survives unchanged in role, and most of it unchanged in code:

| Kept | Where it lives today | Notes after convergence |
|---|---|---|
| k8s Job runtime, pod sizing, labels/annotations, ServiceAccount/IRSA | `xtask/src/replay/jobs.rs`, `infra/eks/replay.tf` | Unchanged. The campaign Job remains `backoffLimit: 6`, `restartPolicy: OnFailure`, no `activeDeadlineSeconds`. |
| Spec ConfigMap input (`<campaign-id>-spec`, mounted at `/etc/rio/replay/spec.json`) | `xtask/src/replay/launch.rs`, `rio-replay/src/run/spec.rs` | `CampaignSpec` gains an `archive` reference, a `scheduling` block, and a `supply` block; the `hydra` block leaves the campaign spec (§6.8, §7.4). |
| S3 state/artifact sync, append-only JSONL state dir, atomic JSON rewrites, `markers/<stage>.done` | `rio-replay/src/run/state.rs`, `artifact.rs` | Unchanged mechanism. New artifacts: `supply.jsonl`, `dispatch.jsonl` (timed mode), `gate.json`. |
| Resume across pod restarts (`download_state_if_missing`, terminal-record skip, `collected.json`) | `rio-replay/src/run/mod.rs` | Extended for in-band collection and timed mode (§6.7). |
| Watchdog, suspension components (pause/idle/ice/dispatch), stall escalation | `rio-replay/src/run/watchdog.rs` | Unchanged in timeless mode; constrained in timed mode (§9.4). |
| `PAUSE` file, backpressure pause, deadline-partial behavior, exit-code contract (0 on drain and on deadline-partial) | `rio-replay/src/run/mod.rs` | Unchanged in timeless mode; in timed mode PAUSE/backpressure become advisory (recorded as suspension windows and lateness, never a dispatch gate — §9.4). Deadline behavior and the exit-code contract are unchanged in both modes; the regression gate never changes the engine exit code (§7.3). |
| `progress.json` + `report/summary.md` + per-class JSONL report files | `rio-replay/src/run/report.rs` | Field renames per §7.4; structure unchanged. |
| Plan stage: filters, closure-overlap analysis, validity snapshot, `campaign.json` with comparability block | `rio-replay/src/run/plan.rs`, `spec.rs` | Reads closures and expected outcomes from the archive instead of `dep-closure.jsonl` + a campaign-time narinfo sweep. |
| Greedy dual-cap batching, submit loop, requeue/cooldown, fail-fast singleton escalation | `rio-replay/src/run/batch.rs`, `submit.rs` | Becomes the **timeless** scheduling mode (§9.1). |
| Two-signal infra attribution, evidence capture (log tails, NAR identity via `BatchQueryPathInfo`) | `rio-replay/src/run/collect.rs`, `grpc.rs` | Signal sources shift with the transport (§6.6); the rule itself is unchanged. |
| The `Submitter` seam and `FakeSubmitter`/`FakeReader` test scaffolding | `rio-replay/src/run/submitter.rs`, `test_support` | The seam is the pivot point of the transport swap (§6.4). |
| Operator workflow: `xtask replay launch` / `status` / `report`, pre-flight, tenant provisioning, SSH/HMAC secret plumbing | `xtask/src/replay/` | §10 (Operator surface). The QueryDerivationStatuses pre-flight probe and `--require-qds` are removed (§6.5). |

### 6.2 What is absorbed from the xtask-replay branch, and where it lands

The xtask-replay branch contributes the transport, the supply planner, the timed scheduler, and the verdict ideas. Its laptop-side orchestration retires. Mechanism is per row: **cherry-pick** (code applies as-is on its current path), **re-home** (code moves into rio-replay, with the replay-specific assumptions removed), **absorb** (the ideas merge into an existing engine module; the source file is not kept), **drop** (not carried forward).

| Source (origin/xtask-replay) | Destination | Mechanism | Notes |
|---|---|---|---|
| `rio-nix/src/protocol/client.rs` additions: `ClientOpError`, `drain_stderr_typed`, `KeyedBuildResult`, `client_build_paths_with_results`, `client_query_valid_paths`, `client_query_path_info`, `NarPayload`, `StoreEntry`, `client_add_to_store_nar`, `client_add_multiple_to_store` | same path | cherry-pick | Library code, transport-agnostic, already conformance-tested. The deadline-on-every-op and abandon-after-mid-payload-error contracts are adopted verbatim by the engine. |
| `rio-nix/src/protocol/wire/framed.rs` `FramedWriter` (256 KiB `FRAME_CHUNK`) | same path | cherry-pick | |
| `rio-gateway/tests/client_ops_conformance.rs` | same path | cherry-pick | Extended in the same phase with multi-root per-path result coverage (§6.4). |
| `xtask/src/k8s/replay/archive.rs` (`ReplayArchive`, Dir/DwarFS backends, NAR dump) | `rio-replay/src/archive/` | re-home | Becomes the v1 archive reader plus a writer used by the recorders (§4, §5). Loses nothing; gains `format_version`/capability handling. |
| `xtask/src/k8s/replay/client.rs` (`GatewayPool`, `DaemonChannel`, `HostKeyPolicy`, `ReplayClientError`) | `rio-replay/src/run/transport.rs` | re-home | The kubectl port-forward default goes away; in-cluster the engine dials `cluster.gateway_store_url`'s host:port directly. Host-key policy is pin-only: `HostKeyPolicy` has the single form `Pinned`, and `CampaignSpec::validate()` rejects any spec without `cluster.gateway_host_key` — there is no trust-on-first-use and no accept-and-record mode. `launch` populates that field at pre-flight from the gateway host-key Secret named by the chart's `gateway.ssh.hostKeySecret` value (the same Secret the gateway mounts), and refuses to launch against a gateway running on the chart's auto-generated emptyDir host key (nothing persistent to pin — redeploy with a host-key Secret first). Dev runs are no exception: `replay dev` requires `--ssh-host-key` for live runs; only the offline `--dry-run` works without a key (§10.4). |
| `xtask/src/k8s/replay/substituter.rs` (narinfo probe, streaming NAR fetch + decompression) | `rio-replay/src/substituter.rs` | re-home | Merges with the existing `nixcache.rs` narinfo client; one HTTP/S3 binary-cache client for both recorders and the supply planner. |
| `xtask/src/k8s/replay/supply.rs` + `prewarm.rs` (`workload_set`, `walk_closure`, `resolve_source`, `plan_uploads`, `UploadClaims`, topo levels, batch splitting, circuit breaker) | `rio-replay/src/run/supply.rs` | re-home | Becomes the single supply planner (§8). The existing `warm.rs` is folded in as the scheduler-side prefetch arm; `warm.jsonl` is superseded by `supply.jsonl`. |
| `xtask/src/k8s/replay/timeline.rs` (`ScheduledRequest`, `build_schedule`, FIFO admission, disconnect anchoring, confirmation retries, `InFlightTracker`) | `rio-replay/src/run/timeline.rs` | re-home | Becomes the timed scheduling mode (§9.2). |
| `xtask/src/k8s/replay/compare.rs` (verdict taxonomy, `classify` precedence, `DivergenceLog`) | `rio-replay/src/run/classify.rs` + `model.rs` | absorb | The verdict ideas merge into the layered comparison model (§7); `divergences.jsonl` survives as a per-unit divergence stream next to `results.jsonl`. |
| `xtask/src/k8s/replay/report.rs` (`Summary`, `exit_code`, console rendering) | — | drop | Its fields fold into `progress.json`/`summary.md`; its `--fail-on` exit-code policy becomes the regression-gate report policy evaluated by the launcher, not by the engine (§7.4). |
| `xtask/src/k8s/replay/mod.rs` (`run_live` laptop orchestration, tunnel handling, `--watch`) | — | drop | The subcommand's roles move to the thin launcher (`launch --archive`) plus a local/dev mode (§10). The 30 s scheduler-metrics `--watch` line is already covered by `xtask replay status --watch`. |
| `xtask/tests/fixtures/replay/basic/` + `basic.dwarfs` | `rio-replay/tests/fixtures/archive/` | re-home | Converted to v1; a v0 copy is retained as the input of the v0-handling test described in §4. The offline dry-run CI test moves with it (§10). |

Dependency consequences: rio-replay gains `russh` (already a workspace dependency), `dwarfs`, `async-compression`, and `tokio-util`; xtask loses its replay-only dependencies once the launcher lands. Phase 2 removes the nix-CLI submission/collection path outright (§11.4); the warm-stage prefetch submission keeps its current shell-out form until it moves onto the client-ops pools with the Phase 4 supply planner, after which the engine image no longer needs the `nix` binary or an `ssh` client for the build path. GNU tar + zstd remained only for the legacy eval-set input (`drvs.tar.zst`) until that path was retired in Phase 5 — v1 archives are read directly from the DwarFS image and need neither.

### 6.3 Stage flow

Stages remain marker-gated (`markers/<stage>.done`) and resumable. The flow is identical for both scheduling modes except where marked.

1. **Bootstrap.** Validate the spec, download state from S3 if the local state dir is empty (`download_state_if_missing`), fetch and open the archive (in-cluster: the DwarFS image per the §4.11 fetch sequence; dev mode may hand the engine a local image or directory, §4.1), verify `format_version` and required capabilities for the requested mode (§9.3).
2. **Plan** (`markers/plan.done`). Apply filters to the archive's workload units (systems, include globs, feature excludes, limit, jobs file — unchanged precedence), compute per-unit closures from the archive's dependency-closure data, derive the not-attemptable set under the leaf measurement policy (closure overlap), snapshot prior validity in rio-store via `BatchQueryPathInfo` (chunk 500), write `campaign.json` with the comparability block, and write plan-time disposition records (`filtered`, `eval-error`, `not-attemptable`, `cached-prior`).
3. **Truth load** (`markers/truth.done`). Read expected outcomes and expected output hashes for every in-scope unit from the archive. This replaces the campaign-time `hydra-truth` narinfo sweep: truth is baked in at archive creation (§5), so the engine performs no outbound truth queries. Archives without the `expected_outcomes` capability mark every unit `no-truth` up front and the report says so.
4. **Supply** (`markers/supply.done` for the prewarm portion). Run the supply planner (§8): probe target validity, resolve every needed path through the ladder, execute the scheduler-side prefetch arm and/or the client-upload prewarm according to the supply policy. Timed runs require the prewarm portion to finish before the clock starts; timeless runs may interleave per-batch top-up with execution.
5. **Execute + collect** (concurrent, as today). Timeless mode: the existing submit loop packs attemptable units into dual-capped batches and submits them; timed mode: the timeline dispatcher fires recorded requests at offset/speedup (§9). Both submit through the same `Submitter` implementation and the same channel pool. Collection consumes in-band per-derivation results from settled submissions, applies infra attribution, captures evidence, assigns verdicts/dispositions, and appends `results.jsonl`. The watchdog/poller runs alongside, exactly as today.
6. **Report** (`markers/report.done`). Render the report under the campaign's report policies (§7.4), refresh the comparability block, write `gate.json` when a regression gate was requested, sync to S3.

### 6.4 The transport swap: client ops behind the Submitter seam

**Today.** `NixSubmitter::submit_batch(store_url, batch, timeout)` shells out twice per batch: `nix copy --derivation --no-check-sigs --from file://<drv-archive-dir> <roots>` to import the batch's `.drv` closure, then `nix build -L --no-link --store <ssh-ng URL> <drv^*…>` with `NIX_SSHOPTS`. Outcome knowledge comes from scraping the child's stderr (`rio: build <uuid>` via `BUILD_ID_RE`, `derivation '<drv>' failed: <reason>` via `DRV_FAILED_RE`), then the collect loop reconstructs per-derivation status via `AdminApi::get_build_graph` + `list_poisoned`.

**After.** The `Submitter` trait stays the seam; a new implementation (`ClientOpsSubmitter`) replaces `NixSubmitter`:

- One submission = one `DaemonChannel` acquired from the `GatewayPool` (4 channels per SSH connection — a client-side fan-out choice that bounds the blast radius of one dropped connection, sitting far below the gateway's own per-connection bound).
- Derivation import: the submitter asks the supply planner for the submission's missing `.drv` texts and embedded input sources (one `client_query_valid_paths` probe over the submission's drv closure, then `client_add_multiple_to_store` of the missing `DrvText`/embedded payloads in reference order). This replaces the per-batch `nix copy --derivation` and removes the engine's local Nix store from the path entirely. Until the supply planner re-homes in Phase 4, the submitter carries this import itself in a minimal drv-text-only form (§11.4); the call sequence and the seam are the same either way.
- Build: one `client_build_paths_with_results` call with the submission's roots as wire derived paths (`"<drv>!*"`, or `"<drv>!out1,out2"` for explicit outputs; the `^` spelling survives only in the human-convenience nix command line the report may print — the recorded per-job `repro_command` itself becomes engine-native, `cargo xtask replay repro <campaign-id> <drv>`, §10.1).
- Outcome: a `Vec<KeyedBuildResult>` — per requested root, a `BuildResult { status: BuildStatus, error_msg, times_built, is_non_deterministic, start_time, stop_time, cpu_user, cpu_system, built_outputs }`. `BuildStatus::is_success()` (Built | Substituted | AlreadyValid | ResolvesToAlreadyValid) is the success predicate; `Substituted`/`AlreadyValid` feed the `target-substituted`/`cached-prior` dispositions exactly as the "completed without execution" rule does today.
- Cancellation: the engine cancels a submission by abandoning the channel (`DaemonChannel::abandon()`), both for `batch_timeout_hours` enforcement (timeless) and for interruption replay (timed). `engine_cancelled` keeps its meaning.

`BatchOutcome` keeps its shape minus the child-process exit code (there is no child process to capture one from) and gains the in-band results:

```rust
pub struct BatchOutcome {
    pub build_id: Option<String>,            // kept: captured from the gateway's `rio: build <uuid>` stderr line
    pub results: Vec<PathOutcome>,           // NEW: in-band per-root results
    pub reasons: BTreeMap<String, String>,   // kept: relayed `derivation '<drv>' failed:` lines (now supplementary)
    pub stderr_tail: String,                 // kept: last 200 stderr log lines, for evidence
    pub engine_cancelled: bool,              // kept
}

pub struct PathOutcome {
    pub drv_path: String,
    pub status: String,        // BuildStatus name, e.g. "Built", "PermanentFailure"
    pub error_msg: String,
    pub start_time: u64,
    pub stop_time: u64,
}
```

`BatchRecord` (one line of `batches.jsonl`) gains the same `results` array. The collect loop consumes `results` directly; an empty or short `results` array is a transport defect handled by the requeue-then-infra rule of §6.5, never a signal to fall back to graph reconstruction. There is no `transport` spec field, no `nix-cli` value, and no A/B harness: `ClientOpsSubmitter` lands by replacing `NixSubmitter` in place, and from Phase 2 onwards the client-ops path is the only submission/collection transport (§11.4).

**What happens to stderr parsing.** The regexes survive but are demoted from collection mechanism to evidence capture. `drain_stderr_typed` gains a log-line observer hook (today it discards `STDERR_NEXT` payloads); the submitter feeds observed lines through the existing `parse_line` so the gateway-announced build id and the relayed per-derivation failure lines keep landing in `BatchOutcome.build_id` / `.reasons` and `stderr_tail`. The build id is still wanted for `AdminApi::log_tail`, for `JobRecord.buildIds`, and for the operator audit trail; the relayed reason lines remain the Signal-1 fallback for infra attribution and supplementary failure-signature evidence (§6.6).

**Per-root result fidelity and the gateway.** As implemented today, `rio-gateway`'s `handle_build_paths_with_results` resolves all requested roots into one merged DAG, submits it once, and clones the single DAG-level `BuildResult` to every requested root (success results are enriched per root with `built_outputs`; failures are echoed identically). For single-root submissions this is exact; for multi-root submissions a single failing root would mark every sibling's in-band result as failed. The xtask-replay branch tolerated this because recorded requests are small and it re-confirmed only the failing positions; a 50-job parity batch cannot.

Decision: the converged design keeps multi-root submissions as the timeless unit of work (the channel/connection budget stays at `submit_concurrency` channels for hundreds of in-flight roots) and extends the gateway to populate true per-root results. The gateway already receives per-derivation terminal events from the scheduler (it relays them as the `derivation '<drv>' failed:` stderr lines and tracks them in `BuildActivityState`); the change is to record the terminal status per requested root and write one accurate `BuildResult` per root instead of cloning the DAG-level result. This is a bounded change in `rio-gateway/src/handler/build.rs`, covered by an extension of `client_ops_conformance.rs` (multi-root request, one root failing, per-root statuses asserted) and a wording update to the corresponding `gw.opcode.*` rule in `docs/spec/components/gateway.typ`. It is the only rio-side change the convergence requires, and it lands — and is deployed — as a Phase 2 prerequisite ahead of the transport cutover (§11.4).

The deployed gateway is presumed to carry that change: per the deployment policy (§2.5), the clusters this subsystem runs against are dev clusters deployed by full wipe, so the per-root fix is simply in place before the engine that needs it. There is no runtime detection of older gateways and no reasons/`list_poisoned` reconstruction fallback for multi-root submissions; `list_poisoned` remains in use only as Signal-2 infra evidence (§6.6).

### 6.5 Collection and the fate of QueryDerivationStatuses

Collection becomes: for each settled submission, take its `PathOutcome` per root, attribute failures (§6.6), capture evidence, classify (§7), append `results.jsonl`, mark the batch processed in `collected.json`. The requeue decisions (`infra-auto-retry`, `dependency-failed-no-trigger`, `failfast-batch-mate`, `engine-cancelled`) survive unchanged; `no-derivation-rows` becomes "no in-band result for this root" (a transport defect) with the same one-requeue-then-infra behavior. Two `JobRecord` fields change provenance with the in-band path: `executed` is derived from the per-root status (`Built` ⇒ executed; `Substituted`/`AlreadyValid`/`ResolvesToAlreadyValid` ⇒ not executed), and `execId` becomes nullable — populated only when a `--debug-graph` dump was taken for triage; the `results.jsonl` schema documents it as nullable.

Consequences for the two existing read paths:

- **`GetBuildGraphReader` loses its collection role.** It is no longer the collection mechanism and its 5,000-node truncation stops being a sizing constraint on batches. It is not carried as a fallback collection path either: in-band per-root results are the only collection source, and Phase 2 deletes the GetBuildGraph collection wiring together with `NixSubmitter` (§11.4). What survives of `GetBuildGraph` is an explicit `--debug-graph` dump path for triage; `AdminApi::list_poisoned` and `AdminApi::log_tail` remain first-class (Signal-2 evidence and log capture).
- **The deferred QueryDerivationStatuses RPC (a deferred work item in the prior parity campaign design) is retired.** Its motivation was that `GetBuildGraph` truncates at 5,000 nodes and `QueryBuildStatus` evidence decays ~60 s after terminal, so collection at M2+ batch sizes needed a PG-backed batch RPC. With in-band per-root results, root status arrives on the channel that asked for the build, regardless of DAG size. What the RPC would still uniquely provide, and what replaces it:
  - per-dependency (non-root) terminal status for forensic triage of large merged DAGs — covered well enough by the relayed reason lines (`reasons`), `list_poisoned`, and `log_tail`; if a future triage tool wants exhaustive per-node status it can revisit the RPC, but nothing in the campaign loop needs it;
  - `is_fixed_output` for source-rot detection — now derived statically from the archive's derivation ATerm (`outputHash` presence), which is strictly better: today `GetBuildGraphReader` always reports `is_fixed_output: None`, so the SourceRot path could not fire at all.

  Accordingly: `QueryDerivationStatusesReader`, the unwired `DerivationStatusApi`, the `DRV_STATUS_CHUNK` constant, the launch-time QDS pre-flight probe, and the `--require-qds` flag are all deleted at convergence. No AdminService RPC is added.

NAR identity for built outputs continues to come from rio-store `BatchQueryPathInfo` (`StoreApi::query_valid`, chunk 500) — bulk-friendly, store-authoritative, and it does not hold gateway channels open. `BuildResult.built_outputs` (DrvOutput id + out path) is recorded as evidence but is not the hash source.

### 6.6 Infra attribution under the client-ops transport

The two-signal AND rule is unchanged; only the plumbing of Signal 1 changes.

- **Signal 1 — relayed scheduler reason.** Source order: the failing root's `PathOutcome.error_msg`, falling back to the captured `derivation '<drv>' failed:` relay line for that drv (`BatchOutcome.reasons`). Both carry the same scheduler terminal-failure text the gateway relays today, so `classify_reason()` and `ReasonClass { Infra, Timeout, ResourceCeiling, Target, Dependency { failing_drv } }` apply verbatim. The per-root `error_msg` is also the primary key for the failure-signature table; the relayed lines are the fallback key and are always retained as evidence. Because both sources carry the same relayed scheduler text, signature grouping does not depend on which one supplied the key; consistency between them is covered by the gateway conformance tests (§6.4), not by a live baseline comparison (R5, §12.1).
- **Signal 2 — poison evidence.** `AdminApi::list_poisoned()` unchanged: a poisoned entry with empty `failed_executors` corroborates infra; a non-empty list contradicts it (some builder genuinely ran and failed it) and the failure is charged to the workload. Evidence decay (`failed_builders: None`) is still treated as "evidence unavailable", not as corroboration.
- **Verdict:** infra only when Signal 1 says infra and Signal 2 does not contradict, or Signal 1 is absent and Signal 2 positively shows poisoned-with-no-builders; contradictions and double losses resolve to a genuine workload failure, with the `log-tail-only` evidence flag when both signals are gone. `resolve_failure_kind`'s signature and tests carry over.
- **Failure causes:** `FailureKind { Genuine, Infra, Timeout, ResourceCeiling, SourceRot }` is kept as the per-failure cause attribute. SourceRot now requires `is_fixed_output = true` derived from the archive's ATerm plus a fetch-error needle in `error_msg`/log tail — the first time this path can actually trigger (see §6.5).
- **Engine-side transport failures** (channel open failure, op timeout, mid-payload wire error, result-count mismatch) are never attributed to the workload: they requeue within the existing retry budget and otherwise classify as `infra-indeterminate` (§7.2) with the transport error as evidence.

`BuildStatus` values add corroborating texture (`TransientFailure`, `TimedOut`) but never override the two-signal rule; the rule remains the only thing that can move a failure out of the workload-charged verdicts.

### 6.7 Resume semantics

Unchanged invariants: state is append-only JSONL plus atomically rewritten JSON; the latest record per unit wins; processed submissions are recorded in `collected.json`; stages are skipped when their marker exists; the eval-set/manifest pin (now the archive digest pin) is re-verified on resume; a fresh pod volume restores from S3 when `spec.campaign_id` is pinned.

Additions:

- **Supply state.** Client uploads are not separately journaled as durable state; on resume the planner re-probes target validity (`client_query_valid_paths` / `BatchQueryPathInfo`) and re-plans. Uploads are idempotent (`AddMultipleToStore` of an already-valid path is skipped by the probe), so a crash mid-prewarm costs re-probing, not correctness. `supply.jsonl` records what was delivered for reporting, not for resume decisions.
- **Timeless mode** resumes exactly as today: terminal units are skipped, settled-but-unprocessed batches are collected, in-flight-at-crash batches have no results and their members are re-offered.
- **Timed mode** resumes with degraded timing fidelity, never silently: outcomes already terminal are kept; requests dispatched but not terminal at the crash are re-dispatched immediately (their `attempts` and `dispatch_lateness` reflect it); requests not yet due are re-anchored so that the first pending request fires immediately and subsequent ones preserve their recorded relative spacing. `progress.json` and the report carry `resume_count` and a `timing_degraded: true` flag when a timed run resumed; the parity report policy ignores it, the timed-fidelity numbers (lateness distribution, interruption reproduction) are flagged low-confidence.

### 6.8 Knobs

Spec shape: the `knobs` block keeps its name and `#[serde(default)]` discipline. The table lists disposition of every existing knob and the additions. "Mode" indicates where the knob has effect.

| Knob | Default | Mode | Status |
|---|---|---|---|
| `batch_max_jobs` | 50 | timeless | kept |
| `batch_max_nodes` | 4500 | timeless | kept (no longer tied to the GetBuildGraph cap; now purely a supply/submission sizing knob) |
| `submit_concurrency` | 8 | timeless | kept (= concurrently held build channels) |
| `narinfo_concurrency` | 64 | both | kept (substituter coverage probes in the supply planner — the per-path narinfo probes of ladder rungs 1 and 3, §8.1) |
| `s3_sync_interval_secs` | 300 | both | kept |
| `collect_poll_secs` | 60 | both | kept |
| `cluster_status_poll_secs` | 60 | both | kept |
| `spawn_intents_poll_secs` | 300 | both | kept |
| `active_stall_hours` | 6.0 | timeless | kept |
| `queued_watchdog_hours` | 2.0 | timeless | kept |
| `max_queued_requeues` | 2 | timeless | kept |
| `max_auto_retries` | 1 | both | kept (infra auto-retry budget) |
| `failfast_singleton_after` | 3 | timeless | kept |
| `batch_timeout_hours` | 24.0 | timeless | kept (timed mode uses recorded-duration deadlines instead) |
| `log_tail_bytes` | 65536 | both | kept |
| `idle_polls_for_suspend` | 3 | timeless | kept |
| `ice_masked_cells_threshold` | 3 | timeless | kept |
| `dispatch_gap_threshold` | 50 | timeless | kept |
| `dispatch_gap_polls` | 5 | timeless | kept |
| `pause_queue_depth` | None | timeless | kept |
| `infra_pause_pct` | 25.0 | timeless | kept |
| `prefetch_shortfall_pause_pct` | 10.0 | both | new (planned-but-missing prefetch paths above this percentage of the planned prefetch set pause the campaign before execution starts; below it the shortfall is recorded as a low-confidence flag — §8.4. The default is a starting point, not a calibrated value.) |
| `report_top_n` | 20 | both | kept |
| `infra_low_confidence_pct` | 5.0 | both | kept and finally consumed: threshold for the `low_confidence` comparability flag on the infra-indeterminate rate |
| `hydra_unknown_threshold_pct` | 5.0 | both | renamed `no_truth_threshold_pct`, consumed as the low-confidence threshold on the no-truth rate |
| `evidence_ttl_hours` | 24.0 | — | retired (read nowhere today; documented-only) |
| `max_sessions` | 32 | timed | new (concurrently admitted requests = held channels) |
| `connections` | ceil(max in-flight channels / 4) | both | new; explicit override of the SSH connection count |
| `op_timeout_secs` | 120 | both | new (probe/upload/path-info deadline) |
| `build_timeout_floor_mins` | 30 | timed | new |
| `build_timeout_cap_hours` | 2 | timed | new |
| `confirm_attempts` | 3 | timed | new (re-confirmation budget for unexpected failures; replaces `--confirm-regressions`) |
| `claim_wait_mins` | 10 | timed | new (cross-request upload-claim wait) |
| `speedup` | 1.0 | timed | new (finite, > 0) |
| `replay_interruptions` | true | timed | new (reproduce recorded cancellations/disconnects via channel abandon) |
| `upload_workers` | 8 | both | new (prewarm/client-upload worker pool) |
| `upload_batch_max_mib` | 256 | both | new |
| `upload_batch_max_entries` | 500 | both | new |
| `large_nar_threshold_mib` | 64 | both | new (streamed individually via `AddToStoreNar`) |
| `probe_chunk` | 2000 | both | new (`QueryValidPaths` chunk size) |
| `probe_concurrency` | 3 | both | new (channels held for validity probing) |

`spec.scheduling.mode` (`"timeless"` | `"timed"`) and the `spec.supply` policy block are spec fields rather than knobs; they are part of the campaign's identity and appear in the comparability block. Validation: `speedup`, `max_sessions`, `confirm_attempts` and the upload sizing knobs are rejected when non-positive; timed mode is rejected when the archive lacks the `timed` capability (§9.3).

## 7. Comparison model

The comparison model is layered so that "what happened" is recorded once, neutrally, and "what it means for this campaign" is a policy applied at report time:

1. **Verdict core** — per workload unit, the comparison of the archive's expected outcome against the replayed outcome. Source-free, policy-free.
2. **Disposition layer** — per workload unit, why a unit was not attempted or does not count. Derived from scope, supply, and measurement policy, never from where the archive came from.
3. **Aggregation / report policies** — per campaign, chosen at launch: how verdicts and dispositions roll up into a headline number and/or a gate.

Every workload unit ends a campaign with exactly one of: a verdict, or a disposition. `results.jsonl` records carry `verdict: Option<String>` and `disposition: Option<String>` with exactly one set; `progress.json` carries `verdictCounts` and `dispositionCounts`. Dispositions are assigned with precedence over verdicts (a unit that was filtered out is never compared), in this order: `filtered` → `eval-error` → `identity-divergent` → `not-attemptable` → `demoted-impure` → `cached-prior` → `upload-rejected` → `supply-failed` → `target-substituted` → `not-attempted`; anything that survives to an attempt receives a verdict.

The verdict vocabulary below is written against the archive's neutral expected-outcome values (§4.6): `built`, `failed`, `resource-exhausted`, `cancelled`, `disconnected`, `indeterminate`, `unknown`. No verdict, disposition, or report term names a source.

### 7.1 Verdict core

| Verdict (wire string) | Expected | Replayed | Meaning |
|---|---|---|---|
| `match-built` | built | built | Both built. Output hashes equal where comparable, or the archive carries no hashes. |
| `output-divergence` | built | built | Both built, but at least one recorded output NAR hash differs from the replayed hash. Requires the archive to carry output hashes. |
| `match-failed` | failed / resource-exhausted | failed | Both failed. Failure text is recorded as evidence but not compared. The expected value is retained on the record so expected `resource-exhausted` rows are reportable separately. |
| `unexpected-failure` | built | failed | The unit itself failed where the recording built it. Carries `failure_cause` (`genuine`/`timeout`/`resource-ceiling`; see below for `infra` and `source-unavailable`). |
| `unexpected-dependency-failure` | built | not buildable | The unit was not built because one of its dependencies failed in the replay (genuine dependency failure; `cascaded` form of the row above). |
| `unexpected-success` | failed / resource-exhausted | built | The replay built a unit the recording says failed (or could not complete within the source's resource limits). Reported, never part of any quality denominator. |
| `source-unavailable` | built | failed | The unit (or a dependency, with `cascaded: true`) failed only because a fixed-output input could not be fetched from its upstream origin. Excluded from quality denominators; tracked as ambient decay, not as a target defect. |
| `infra-indeterminate` | any | — | The replayed outcome cannot be trusted: rio-side infrastructure failure under the two-signal rule, an engine transport failure after the retry budget, or evidence loss. Counted against run confidence, never against the target. |
| `truth-indeterminate` | cancelled / disconnected / indeterminate | any | The recorded outcome was interrupted or infrastructure-dependent, so there is no deterministic expectation to compare against. (In timed runs, recorded cancellations/disconnections are instead reproduced and classified with the two timed-only verdicts below.) |
| `no-truth` | unknown / absent | any | The archive carries no expected outcome for this unit. The unit is still attempted and its replayed outcome is reported, but it cannot enter any agreement metric. |
| `interruption-replayed` (timed only) | cancelled / disconnected | interrupted | The recorded interruption was reproduced at its recorded offset (channel abandoned); the unit did not complete, as recorded. A `detail` field records `cancellation` or `disconnect`. No outcome comparison is attempted. |
| `interruption-not-reproduced` (timed only) | cancelled / disconnected | built | The replayed build completed before the recorded interruption offset (the target was faster than the recording); informational divergence in timing behavior, not a correctness defect. |

The table is total over (expected, replayed) once two precedence rules are applied. First, `infra-indeterminate` takes precedence over every comparison verdict: a replayed outcome the two-signal rule (or an exhausted transport retry budget) attributes to infrastructure is never compared, whatever was expected. Second, when the expected outcome is `failed` or `resource-exhausted`, any replayed failure of the unit — its own, dependency-cascaded, or a fixed-output fetch failure — classifies `match-failed`, with the cause retained in the `failure_cause` and `cascaded` attributes; `unexpected-dependency-failure` and `source-unavailable` are reachable only when the expected outcome is `built`. `resource-exhausted` is a failure-class expectation: it follows exactly the same rows as expected `failed`, and the expected value is carried on the result record so reports can break out source resource limits separately from deterministic failures.

Attributes carried alongside the verdict (not separate verdicts): `cascaded: bool` (the unit inherited its verdict from a dependency), `failure_cause` (the surviving `FailureKind`), `attempts: u32`, and `flaky: bool` (more than one attempt was needed before the final verdict settled on a match). NAR-hash comparison detail (`equal` / `differs` / `not-comparable` per output) is recorded under `narCompare` exactly as today; `output-divergence` is simply the verdict-level projection of "any output differs".

### 7.2 Disposition layer

| Disposition (wire string) | Assigned at | Meaning |
|---|---|---|
| `filtered` | plan | Outside the campaign's scope filters (system, glob, feature exclude, limit, jobs file). Skip reason retained verbatim. |
| `eval-error` | plan | The archive marks the unit as failing at evaluation/recording time; there is nothing to build. |
| `identity-divergent` | plan | The unit's `units.jsonl` record carries `identity_divergent: true` (§4.7) — the recorder's fidelity gate found its derivation identity divergent from the source it recorded; comparing it would compare different builds. |
| `not-attemptable` | plan | Under the leaf measurement policy, the unit's outputs lie inside another in-scope unit's dependency closure; building the other unit would supply this one, so it cannot be measured independently. |
| `cached-prior` | plan | Already valid in the target store before the campaign started (validity snapshot). |
| `target-substituted` | collect | Completed without execution during the run because the target substituted it from its own upstream. Under a force-build measurement policy this is also a policy violation and is surfaced in the comparability block. |
| `demoted-impure` | plan/supply | The unit declares impure environment variables the engine does not forward; its recorded outputs are supplied like dependencies and it is not rebuilt. |
| `upload-rejected` | supply/execute | The target refused an upload required for this unit's closure (daemon error after the one fresh-channel retry). The unit was not attempted. Charged to the target by the regression gate, but it is not an outcome comparison. |
| `supply-failed` | supply/execute | The engine could not obtain or deliver required supply for reasons not attributable to the target (relay miss or fetch failure, archive corruption, claim timeout). Counted against run confidence. |
| `not-attempted` | report | The run ended (deadline, pause, abort) before the unit was attempted. Backfilled so counts sum to the in-scope total, exactly as today. |

Per-path supply outcomes (§8.4) are recorded in `supply.jsonl` and roll up into these unit-level dispositions; they are not themselves dispositions.

### 7.3 Aggregation / report policies

Report policies are chosen at launch in the spec (`spec.report: { "policies": ["parity", "regression-gate"], "fail_on": … }`; both may be requested for one campaign) and are never baked into the archive. They read the same `results.jsonl`.

**Parity policy** (the existing headline, renamed nothing but its inputs):

- Headline build-outcome agreement = `(match-built + output-divergence) / (match-built + output-divergence + unexpected-failure + unexpected-dependency-failure)`. This is exactly today's formula with today's `match-built` split into the two new verdicts; output divergence does not hurt the build-outcome headline.
- Secondary, non-gating NAR agreement = `match-built / (match-built + output-divergence)` over units whose hashes were comparable.
- Excluded-but-reported: every other verdict and every disposition, each with its own count, exactly like today's excluded buckets.
- The comparability block keeps its fields (eval set → archive identity + digest, manifest sha256 → archive content digest, mode, build tenant, filters, engine version, signature table version, in-scope/attemptable/attempted, per-class excluded counts, completeness, low-confidence flags). Low-confidence flags now derive from `infra_low_confidence_pct` (infra-indeterminate rate), `no_truth_threshold_pct` (no-truth rate), and below-pause-threshold prefetch shortfall (§8.4), plus the existing tenant-verification flag (`tenant-upstreams-unverified`). Exclusion records from `exclusions.jsonl` enter the completeness accounting when the member is present (mirroring today's eval-error accounting); when it is absent the block notes that fact and applies no penalty (§4.7).

**Regression-gate policy** (the xtask-replay `--fail-on` semantics, generalized):

| `fail_on` | Trips on |
|---|---|
| `none` | never (observational run) |
| `regression` | `unexpected-failure` + `unexpected-dependency-failure` + `upload-rejected` + `infra-indeterminate` > 0 |
| `divergence` | everything in `regression`, plus `output-divergence` + `unexpected-success` + `interruption-not-reproduced` > 0 |

The gate result is data, not an exit code: the engine writes `report/gate.json` (`{ "policy": "regression-gate", "fail_on": "...", "tripped": bool, "counts": {...} }`) and mirrors it in `progress.json`. The engine's own exit-code contract is unchanged (0 on full drain and on deadline-partial); encoding the gate in the pod exit code would make the k8s Job retry the whole campaign (`backoffLimit: 6`). `xtask replay report --check` reads `gate.json` and exits non-zero when tripped — it is the single CI consumption point for the gate, and it is where the old `--fail-on` exit behavior now lives (§10).

### 7.4 Rename mapping

Existing classification vocabularies map onto the new verdict/disposition names as follows. These renames are results-schema changes (the wire strings in `results.jsonl`, `buckets/`, `progress.json` are frozen by `model.rs` tests today); they land at convergence (§11, phase 5), not in the earlier phases.

Current parity buckets → new vocabulary:

| Current bucket | New name | Layer |
|---|---|---|
| `match-built` | `match-built` (with `narCompare: differs` cases becoming `output-divergence`) | verdict |
| `rio-only-failure` | `unexpected-failure` | verdict |
| `rio-dependency-failure` | `unexpected-dependency-failure` | verdict |
| `rio-infra-failure` | `infra-indeterminate` | verdict |
| `upstream-source-unavailable` | `source-unavailable` | verdict |
| `hydra-only-failure` | `unexpected-success` | verdict |
| `both-failed` | `match-failed` | verdict |
| `hydra-unknown` | `no-truth` | verdict |
| `eval-divergence` | `identity-divergent` | disposition |
| `eval-error` | `eval-error` | disposition |
| `skipped` | `filtered` | disposition |
| `not-attemptable` | `not-attemptable` | disposition |
| `not-attempted` | `not-attempted` | disposition |
| `cached-prior` | `cached-prior` | disposition |
| `target-substituted` | `target-substituted` | disposition |

xtask-replay verdicts → new vocabulary:

| Replay verdict | New name | Notes |
|---|---|---|
| `match` (hashes equal) | `match-built` | |
| `match` (recorded failure also failed) | `match-failed` | |
| `match` (recorded cancellation also failed, timeless reading) | `truth-indeterminate` | timed runs use `interruption-replayed` instead |
| `non_reproducible` | `output-divergence` | |
| `regression` | `unexpected-failure` | `attempts` carries over |
| `failure_not_reproduced` | `unexpected-success` | |
| `cancellation_not_reproduced` | `interruption-not-reproduced` | timed only |
| `disconnect_replayed` | `interruption-replayed` | timed only |
| `skip` (no recorded build) | `no-truth` | |
| `skip` (impure env not forwarded) | `demoted-impure` | disposition |
| `skip` (target already had the outputs) | `target-substituted` / `cached-prior` | disposition; split by whether validity predates the campaign |
| `skip` (recorded outcome infrastructure-dependent) | `truth-indeterminate` | |
| `skip` (output hashes could not be collected) | `infra-indeterminate` | evidence loss |
| `upload_rejected` | `upload-rejected` | disposition |
| `request_error` | `infra-indeterminate` | |
| `flaky` counter | `flaky: true` attribute | no longer a pseudo-bucket |

Engine-internal renames that follow from truth becoming archive-resident: `HydraOutcome` → `ExpectedOutcome` (values per §4), `HydraSide`/`HydraOutput`/`HydraEntry` → `Expected*`, `hydra.jsonl` → gone (the truth-load stage reads the archive; no campaign-time sweep cache exists), `hydra_truth.rs` → `truth.rs`, spec `hydra: HydraBlock` → removed from the campaign spec (its cache-URL, user-agent, and buildstatus-file inputs move to the recorder, §5), `progress.json` `hydraUnknownRatePct` → `noTruthRatePct`, `warm.jsonl` → `supply.jsonl`. `WARM_TENANT`/`parity-warm` was untouched by these truth-related renames (it brands a tenant role — prefetch — not a source); like the other `parity-*` deployment identifiers it was renamed (to `replay-warm`) only in the Phase 5 cutover (§11.7).

### 7.5 Spec and tracey impact

`rio-replay/**` carries no `r[impl …]`/`r[verify …]` markers and is not in tracey's `impls` include list, so the verdict/disposition renames and the transport swap inside the engine touch no tracey rule. The parity-adjacent rules that do exist (`gw.build.per-tenant-policy+2`, `sched.merge.force-build-roots`, `sched.merge.substitute-*`, `sched.dispatch.fod-substitute+3`) describe gateway/scheduler behavior keyed by tenant and build flags; they are unaffected by what the client calls itself or how it classifies results. Two spec-touching items exist, both scheduled with their code: the gateway per-root results change (§6.4) updates the relevant `gw.opcode.*` rule and its conformance `r[verify]` markers; and if §4 introduces archive-format rules into `docs/spec/`, the recorders and the engine reader annotate against those at that time. The bucket renames themselves are a results-schema/wire change gated by the model.rs string-freeze tests and a one-line migration note in the operator docs — they were deliberately deferred to the Phase 5 cutover so campaign artifacts written before it stay internally consistent.

## 8. Supply planning

Supply is everything the engine delivers (or arranges to have delivered) so that a workload unit can be attempted: derivation texts, input sources, and dependency outputs. There is exactly one supply planner; it serves both scheduling modes, both measurement modes, and every archive, and it is the only component that decides where a path comes from and how it travels.

### 8.1 One planner, one ladder

The planner resolves every path in the union closure of the in-scope workload, in this order:

0. **Outputs of attemptable workload units are never supplied.** The rule applies after the plan stage has resolved closure overlap (§7.2): a unit whose outputs lie inside another in-scope unit's dependency closure takes the `not-attemptable` disposition — if unit A's output lies inside unit B's closure (both in scope), **A** is the unit marked, because once A's output has been supplied for B's build A can no longer be measured independently. A's outputs are then supplied exactly like ordinary dependency outputs (today's warm behavior, unchanged), keeping B a measurable leaf. For every unit that remains attemptable, its outputs are excluded from supply unconditionally — supplying what the target is being asked to build would corrupt the measurement.
1. **Target's own substituters.** If any of the target's configured upstreams answers a narinfo probe for the path, the path is delegated: the engine does not move bytes; the scheduler substitutes it (§8.2).
2. **Embedded archive content.** If the archive embeds the path (fat archives embed everything the recording's caches could not be relied on for; thin archives embed only what no configured substituter can provide), it is uploaded client-side from the archive.
3. **Relay from recorded source substituters.** If a substituter listed in the archive's manifest has the path, the engine fetches the NAR (streaming, hash- and length-verified) and uploads it client-side. Only `https://` and `s3://` relay sources are honored; the accepted relay hosts are logged before any traffic.
4. **Not supplied.** No source can provide the path. If the path is itself buildable from supplied inputs the target may still build it as part of the closure; if it is required and unobtainable, dependent units take the `supply-failed` disposition.

Derivation texts are a special case handled before the ladder: every `.drv` in a submission's closure that is not already valid on the target is uploaded as a single-file NAR of its ATerm text (path info computed by the planner: NAR hash/size, references = `inputDrvs` + `inputSrcs`, content address `text:sha256:<hash>`). This replaces the engine's `nix copy --derivation --from file://` import and is what makes the engine image independent of a local Nix store.

### 8.2 Delivery mechanism selection

The planner chooses the mechanism per path; the operator chooses policy, never mechanism.

| Mechanism | Used for | How | Scale properties |
|---|---|---|---|
| Scheduler-side prefetch ("delegate") | Paths covered by the target's own substituters, when the supply policy wants them present before measurement | The producing derivations of the covered paths are submitted as ordinary roots under the prefetch tenant (`replay-warm`) ahead of execution; the scheduler's existing bulk-substitution path fetches the NARs directly from the upstream into rio-store, attributed to `path_tenants` so they survive GC. This is today's warm stage, unchanged in mechanism. | Bytes never transit the engine pod. Estimated to handle the 1e5-path scale envisioned for platform slices (campaign-design sizing: 1–3 h on 1–2 store replicas); to be confirmed by the M2/M3 warm-stage wall-clock measurements. |
| Client upload, batched | Embedded and relayed paths < `large_nar_threshold_mib` (64 MiB), and all derivation texts | `client_add_multiple_to_store` batches in reference-safe topological levels, split at `upload_batch_max_entries` (500) / `upload_batch_max_mib` (256 MiB), spread over `upload_workers` (8) channels; payloads are materialized at send time (drv text inline, archive `dump_nar` on a blocking thread, relay via streaming fetch). | Peak engine memory ≈ `upload_workers × upload_batch_max_mib` (≈ 2 GiB at defaults). Engine pod bandwidth is the ceiling; flagged as a scale risk in §12. |
| Client upload, streamed | Embedded and relayed paths ≥ `large_nar_threshold_mib` | One `client_add_to_store_nar` per path with `NarPayload::Reader` streaming straight from the archive or the relay fetch, `LARGE_UPLOAD_TIMEOUT` 600 s, fresh channel after any error. | Constant memory per transfer. |
| None (build it) | Paths producible by derivations that are themselves in the supplied closure | Nothing delivered; the target builds them as dependencies. | This is the self-hosted measurement working as intended. |

Tenant plumbing under the client-ops transport: the engine holds one `GatewayPool` per tenant role — the build tenant for submissions and client uploads, the prefetch tenant (`replay-warm`) for the delegate arm's prefetch submissions — each dialed at `cluster.gateway_store_url` with that tenant's mounted key file (tenant selection is by SSH key, not by URL). This replaced the earlier separate `cluster.warm_store_url`, retired when the prefetch arm moved onto these pools in Phase 4 (build submission itself was client-ops-only from Phase 2 onwards, §11.4; until Phase 4 the prefetch arm kept its nix shell-out submission).

Validity probing precedes all delivery: rio-store `BatchQueryPathInfo` for the plan-time snapshot, `client_query_valid_paths` (chunks of `probe_chunk` = 2000, over `probe_concurrency` = 3 channels) immediately before uploads, so nothing already valid is re-sent and resumed campaigns re-converge cheaply.

### 8.3 Supply policies

Policies are spec-level (`spec.supply`), recorded in the comparability block:

- `workload_outputs: never` — not configurable; stated for completeness. The measurement rule of §8.1 step 0 always holds.
- `dependencies: substituters | embedded-only | none`
  - `substituters` (leaf-style measurement): the full ladder applies. Answers "can the target build each unit when its dependencies are available from its configured upstreams?"
  - `embedded-only` (hermetic replay): step 1 of the ladder is skipped — even paths the target's substituters could provide are uploaded from the archive/relay. Used to take the recorded source's caches out of the loop entirely (a fat archive replayed this way needs no external cache at all).
  - `none` (self-hosted measurement): no dependency outputs are delivered by any mechanism; only derivation texts and embedded input sources are uploaded. The target builds the entire closure itself.
- `delivery: prewarm | inline`
  - `prewarm`: all planned supply is delivered before the execution clock starts (required for timed runs; default for both modes). The scheduler-side prefetch arm is inherently prewarm-shaped.
  - `inline`: client uploads happen per submission as gaps are discovered. Lower setup latency, lower timing fidelity; allowed only for timeless runs. Inline top-up also remains the universal fallback when prewarm misses something (a path that failed prewarm is retried inline once before its dependents are marked).

The mode/tenant coupling carries over from the parity design: `replay-leaf` pairs with `dependencies: substituters`, `replay-selfhosted` with `dependencies: none`; the prefetch tenant is only provisioned when the policy includes a prefetch arm.

### 8.4 Failure handling and supply dispositions

Per-path supply outcomes are recorded in `supply.jsonl`: `{ path, source: workload-output|target-substituter|embedded|relay|none, mechanism: delegate|upload-batch|upload-stream|none, outcome: delivered|already-present|delegated|refused|unavailable|failed, detail, batch_id?, bytes? }`. The previous `warm.jsonl` dispositions map into this record shape (`already-present` → `already-present`, `substituted`/`built-fallback` → `delegated` with detail, `not-found-upstream`/`no-static-producer` → `unavailable`, `failed-after-retries` → `failed`).

Failure rules, unchanged in spirit from the absorbed planner:

- **Probe errors are not misses.** A narinfo probe HTTP 403/`AccessDenied` is an error (counted, warned once per cache), never treated as "not covered"; 404/`NoSuchKey` is a miss. Coverage probes that error fall through to the next ladder rung.
- **Upload refusals retry once on a fresh channel** with re-materialized payloads; a second refusal marks the affected paths `refused`, and dependent units take the `upload-rejected` disposition. Wire errors during an upload are treated as refusals (a refusal can race session teardown).
- **Relay failures degrade per path** (`failed`), and dependents that cannot proceed take `supply-failed`. A circuit breaker (threshold `max(2 × upload_workers, 6)` consecutive failed channel opens, latched) stops dialing a gone gateway and fails remaining planned uploads fast; the watchdog's pause/backpressure machinery then takes over.
- **Cross-request claims** (timed mode): concurrent requests needing the same path coordinate through `UploadClaims` (claim / wait up to `claim_wait_mins` / re-claim once). An expired claim lets the request proceed without the path; the resulting build failure, if any, is real and classifies normally.
- **Scheduler-side prefetch failures** are visible as `delegated`-arm records with `failed`/`unavailable` outcomes (derived from the prefetch submissions' results); affected units are not blocked — at execution time the target may still substitute or build the path. Prefetch shortfall is gated, not just reported: when the planned-but-missing prefetch paths at the end of the supply stage exceed `prefetch_shortfall_pause_pct` (§6.8) of the planned prefetch set, the campaign pauses **before execution starts**, using the same PAUSE/resume mechanics as the infra-rate pause, and the operator decides whether to resume (accepting the shortfall), top up supply, or abort. Below the threshold the run proceeds and the shortfall is recorded as a low-confidence flag, because it changes what the headline measured.

Unit-level effect: `upload-rejected` and `supply-failed` dispositions (§7.2), plus a `supply` summary block in the report (delivered/delegated/refused/unavailable counts, bytes uploaded, prefetch shortfall). Shortfall below the pause threshold surfaces as a low-confidence flag on the comparability block; shortfall above it pauses the campaign before execution as described above.

## 9. Scheduling

Two dispatch modes run over the same transport, the same supply planner, the same collection and comparison pipeline. The mode is chosen at launch (`spec.scheduling.mode`) and recorded in the comparability block; it never changes what a verdict means, only when submissions happen and which timed-only verdicts are reachable.

### 9.1 Timeless mode

Timeless mode is the existing parity submit loop, unchanged in behavior:

- Attemptable units are greedily packed into batches under the dual cap (`batch_max_jobs` = 50 jobs, `batch_max_nodes` = 4500 merged-closure nodes); oversized units become singleton batches; units resubmitted `failfast_singleton_after` (3) times are isolated into singletons.
- Up to `submit_concurrency` (8) submissions are in flight at once; each submission is one channel doing gap top-up (inline policy) followed by `BuildPathsWithResults`, bounded by `batch_timeout_hours` (24 h) and abandoned on expiry (`engine_cancelled`).
- Re-offer rules, post-settlement cooldown (`collect_poll_secs`), requeue budgets (`max_auto_retries`, `max_queued_requeues`), and the fail-fast batch-mate logic are unchanged.
- Throughput is governed by the batch caps, `submit_concurrency`, and the backpressure pause; there is still deliberately no jobs-per-hour pacing knob.
- The deadline (spec or `--deadline`) stops new submissions only; in-flight submissions drain; missing units are backfilled `not-attempted`; the run exits 0 with `partial: true`.

Timeless mode is valid for every archive, with or without timing data. It is the mode the first live smoke campaign runs (§11.1).

### 9.2 Timed mode

Timed mode reproduces the recorded request cadence:

- **Schedule construction.** Recorded requests are sorted by offset, optionally truncated (`filters.limit`), and given `due = offset / speedup`. The recorded request is the submission unit — timed mode never re-batches; whatever set of roots a recorded request carried is submitted together, so DAG-level coupling matches the recording.
- **Dispatch.** One task per scheduled request sleeps until `start + due`, then waits on a FIFO admission semaphore of `max_sessions` (32) permits. Dispatch lateness (admission time minus due time) is tracked per request; `max_dispatch_lateness_ms` and the lateness distribution are reported. Lateness is a confidence signal, never a verdict.
- **Build deadlines.** Per request: `2 ×` the slowest recorded duration among its units, clamped to [`build_timeout_floor_mins`, `build_timeout_cap_hours`]; the floor applies when the archive has no durations for the request.
- **Interruption replay.** When `replay_interruptions` is true and the archive records a cancellation or client disconnect for a unit of the request that the target cluster actually builds (impure-demoted units are supplied, never built, so their recorded interruptions are not replayed — the same workload filter the offline dry-run planner counts with, applied once at schedule construction), the engine arms an abandon deadline anchored at dispatch so supply time does not shift it. The delay is derived from the full set of interrupted workload units in the request: the earliest `(recorded stop offset − recorded start offset) / speedup` among those with a recorded stop offset, 60 s/`speedup` only when none of them records one, floored at 1 s. The build is always actually submitted (minimum 1 s of build time), then raced against the deadline; if the deadline wins the channel is abandoned — the gateway observes a client disconnect and cancels that session's builds — and the unit classifies `interruption-replayed`. The submission carries a typed deadline (build budget vs. disconnect replay, both absolute instants anchored at admission, decided before the race starts), and the batch record names which one fired: when the recorded gap lies beyond the build deadline, the engine's own budget cut is recorded as an engine cancellation, never as the interruption being reproduced. Both recorded cancellations and recorded disconnects are reproduced this way; over ssh-ng they are the same observable client behavior.
- **Confirmation retries.** A unit whose expected outcome is `built` but whose replayed result is a failure is re-submitted alone (only the failing positions, on a fresh channel) up to `confirm_attempts` (3) total attempts before `unexpected-failure` is recorded; attempts and flakiness are carried on the verdict. This is the de-correlation safeguard against request-level coupling and transient noise.
- **Output identity.** Replayed NAR hashes for built units are collected as in timeless mode (rio-store `BatchQueryPathInfo`) and compared against the archive's recorded hashes.

### 9.3 Capability gating and knob validity

Mode availability is gated by the archive's declared capabilities (§4), checked at bootstrap and at `xtask replay launch` time:

| Requirement | Needs | Behavior when absent |
|---|---|---|
| `mode: timed` | `timed` capability (per-request offsets) | launch refuses; the engine refuses at bootstrap if launched anyway |
| `replay_interruptions: true` | `timed` capability + per-unit interruption records (stop offsets optional) | knob forced false with a warning recorded in `campaign.json` |
| `interruption-replayed` / `interruption-not-reproduced` verdicts | timed mode | unreachable; recorded interruptions classify `truth-indeterminate` in timeless runs |
| Output-hash comparison (`output-divergence`, NAR agreement) | `expected_outcomes` + `output_hashes` capabilities | hash comparison skipped; affected units stay `match-built` with `narCompare: not-comparable` |
| Any expected-outcome comparison | `expected_outcomes` capability | every unit classifies `no-truth`; the report states that agreement metrics are undefined |
| `speedup`, `max_sessions`, `confirm_attempts`, `claim_wait_mins`, `build_timeout_*` | timed mode | rejected by spec validation in timeless mode |
| `delivery: prewarm` enforced | timed mode | timeless runs may choose `inline` |

A timeless campaign over a timed archive is always legitimate (the timing data is simply unused); the reverse is impossible by construction.

### 9.4 Pause, backpressure, and deadlines across modes

- **Timeless:** unchanged. The `PAUSE` file, the dispatch-gap suspension, queue-depth and infra-rate backpressure all gate new submissions; suspension windows freeze stall clocks; the deadline stops new submissions only.
- **Timed:** the engine never silently warps the clock. The `PAUSE` file and backpressure conditions do not stop dispatch; they are recorded as suspension windows and show up as dispatch lateness and a `timing_degraded` flag when they materially delay admission (same flag as resume, §6.7). The infra-rate pause threshold (`infra_pause_pct`) still exists in timed mode but acts as an abort-recommendation surfaced in `progress.json` rather than a dispatch gate; the operator decides whether to abort a timed run that is drowning in infrastructure failures, because pausing it would destroy the very property it exists to measure. (The prefetch-shortfall pause of §8.4 is unaffected by this rule: it acts before the execution clock starts, in either mode.) The deadline behaves identically in both modes: no new dispatches after it, in-flight requests drain, the run is marked partial.
- **Stall handling:** active/queued stall escalation applies in both modes (a stalled request in a timed run escalates to `infra-indeterminate` exactly as a stalled batch does today); `queued_watchdog_hours` requeues do not apply to timed mode because admission is governed by the recorded schedule, not by a queue of attemptable units.

## 10. Operator surface

The operator surface stays where it is today: a single `cargo xtask` command family that creates Kubernetes objects and reads S3 artifacts, with all measurement work running in-cluster. Nothing the operator runs locally is ever part of the data plane, with the single, explicitly dev-only exception described in §10.4. The Phase 5 cutover (§11.7) renamed the command family from the legacy `cargo xtask parity {eval,launch,status,report}` to `cargo xtask replay …`; the surface described here is the converged end state, as landed.

### 10.1 Command family

The unified subsystem keeps one xtask family. The family name follows the subsystem name (§3.3); the rename from `parity` to `replay` landed with the Phase 5 cutover (§11.7). This section writes the converged form as `cargo xtask replay`, with the pre-cutover command shown alongside. The `parity` word survives only inside the report-policy vocabulary (§7 Comparison model), not in command names.

| Pre-cutover | Converged | Role | Where it executes |
|---|---|---|---|
| `cargo xtask parity eval` | `cargo xtask replay record` (alias `eval` kept for one release) | Create the evaluation-recorder Job (§5 Recorders), follow it to completion, and summarize the published v1 archive | xtask creates and follows the Job; the recorder runs in-cluster |
| — | `cargo xtask replay list` | List the published archives under `replay/archives/` (identity, eval, scope, size, fidelity), newest first | local, read-only |
| — | `cargo xtask replay delete <short-id>` | Delete one published archive (objects + the by-recipe pointer it owns) | local |
| `cargo xtask parity launch` | `cargo xtask replay launch` | Provision tenants/secrets, write the spec ConfigMap, create the campaign Job | xtask runs locally; the engine runs in-cluster |
| — | `cargo xtask replay launch --archive <path\|s3://…>` | Same launcher, fed by any v1 archive instead of a recorder-addressed one; local paths are uploaded first | same |
| `cargo xtask parity status` | `cargo xtask replay status` | Job state + summarized `progress.json`; `--watch` re-polls every 30 s | local, read-only |
| `cargo xtask parity report` | `cargo xtask replay report` | Download `report/summary.md` + `progress.json`; new `--check` flag applies the launched gate policy to the exit code | local, read-only, never consults the cluster |
| `cargo xtask parity repro/abort/cleanup` | `cargo xtask replay repro <campaign-id> <drv>`; `abort`/`cleanup` unchanged | `repro` becomes the engine-native single-unit replay that the recorded `repro_command` field references (see below); `abort`/`cleanup` stay M2 stubs | local (creates a one-unit run against the cluster) |
| `cargo xtask k8s replay` (xtask-replay branch) | retired | Laptop-orchestrated run loop is deleted; the launcher and dev mode replace it | — |
| — | `cargo xtask replay dev` | Run the engine locally against a k3s/port-forward gateway at fixture scale; `--dry-run` plans fully offline | local, dev only |

**`record` (alias `eval`).** Flag surface: `--eval <u64>`, `--system` (repeatable, default `x86_64-linux`), `--scope constituents:<agg>|jobs:<list>|jobs-file:<path>|full`, `--jobset`, `--force`, `--detach`, `--log-level`. Job naming (`replay-eval-<eval>-<8 hex>`), sizing (always the full-evaluation shape, 160 cpu/1200Gi — there is no scoped shape and no scale flag to forget), `ttlSecondsAfterFinished=86400`, and the ECR image assertion are all retained. By default `record` follows the Job: it streams the recorder pod's logs, waits for completion, and prints a summary of the published archive (identity, S3 location, image size, counts, capabilities, fidelity, and the `replay launch --eval …` follow-up); `--detach` restores the create-and-exit behavior. A same-name 409 on Job creation re-attaches instead of erroring — the Job name encodes the request digest, so an existing Job IS this request — and interrupting the follow never cancels the in-cluster Job. The Job's output is a v1 archive with expected outcomes baked in (§4 Archive format v1, §5 Recorders), uploaded under the archive S3 layout with the completion marker last. The politeness budget, recipe reproduction, and fidelity gate are unchanged and their results land in the archive's provenance block.

**`launch`.** Existing flags survive: `--eval`, `--eval-digest`, `--mode leaf|self-hosted`, `--campaign-id`, `--limit`, `--deadline` (RFC3339, forwarded to the engine argv), `--restart-gateway`, `--log-level`. The pre-flight is mandatory — there is no `--skip-preflight`, and a deployed-vs-tree image-tag skew is always a hard refusal (no `--allow-version-skew`); debug runs that cannot satisfy the pre-flight belong in `replay dev`. The `--engine-arg` passthrough is gone: the spec plus `--deadline` cover the operator surface, and any other engine knob is a spec field. Existing responsibilities survive in the same order: campaign-id validation, tofu output resolution, ECR image assertion, archive resolution in S3 (the completion marker is the existence test, exactly as `evalset.json` is today), namespace/ServiceAccount ensure, tenant provisioning via rio-cli, per-tenant SSH keys, HMAC Secret copy, pre-flight, spec build + validation, `guard_existing_campaign`, ConfigMap apply, Job apply.

New flags:

- `--archive <path|s3://bucket/prefix>` — accept any v1 archive. A local `.dwarfs` image is uploaded to the archive prefix as-is; a local directory (the working/dev form) is first packed into an image with `mkdwarfs` (provided in the dev shell) — the published S3 form is always the image (§4.11). Uploads are content-addressed and skipped if the digest already exists; an S3 URI is verified to carry the completion marker. `--archive` and `--eval/--eval-digest` are mutually exclusive ways to name the same thing: `--eval`/`--eval-digest` are recorder-convenience aliases for Hydra-derived archives, resolved by the launcher — against the legacy eval-set prefix scheme (`<root>/evals/…`) before Phase 3, and through the recorder-owned recipe-digest pointer objects (`<root>/archives/by-recipe/…`, §5.1) afterwards. They are kept indefinitely as a convenience; the spec always pins the resolved digest-keyed archive, so the engine never sees the alias.
- `--schedule timeless|timed` (default `timeless`) and `--speedup <f64>` — scheduling mode (§9 Scheduling). Launch refuses `--schedule timed` against an archive whose manifest lacks the `timed` capability; the engine re-validates.
- `--report-policy parity|regression-gate` (repeatable; default `parity`) — selects the aggregation policies written into the spec (§7 Comparison model). Both may be requested for one campaign.
- `--fail-on <policy-specific value>` — recorded into the regression-gate policy block; it is **not** an engine exit-code knob (the engine keeps its exit-0-on-drain contract), it is evaluated by `report --check`.

The `--require-qds` flag and the QueryDerivationStatuses pre-flight probe are deleted in Phase 2, together with the rest of the nix-CLI submission surface the client-ops transport replaces (§6.5 covers what replaces that RPC's role); until then they behave as today.

**`status`.** Unchanged: positional campaign id, `--watch` (30 s), prints Job active/succeeded/failed plus the summarized `progress.json` (stage, counts, rates, suspension windows, completeness).

**`report`.** Unchanged download behavior (bail if `report/summary.md` is missing; never consults the cluster), plus `--check`: when the campaign was launched with a regression-gate policy, the gate result recorded in the report artifacts is mapped to the process exit code, giving CI a single command to consume. `report --check` is the single CI consumption point for the gate; it reproduces the xtask-replay branch's `--fail-on` semantics without making the in-cluster engine exit nonzero on divergences.

**`repro`.** The per-job `repro_command` recorded with every failure-class verdict is engine-native: `cargo xtask replay repro <campaign-id> <drv>` derives a one-unit campaign spec from the original campaign's stored record (`campaign.json` + `results.jsonl` — same archive pin, cluster endpoints, tenants, and knobs, scope narrowed to the named unit) and applies it as a fresh repro campaign Job; the engine pod fetches the pinned archive and replays the unit over the same client-ops transport and supply policy the campaign used. The verdict arrives asynchronously: `repro` prints the follow-up `replay status`/`replay report` commands that surface it. The report text may additionally print the equivalent `nix build --store <ssh-ng url> '<drv>^*'` line as a human convenience, but the recorded field is the engine-native invocation — it requires no local archive copy and no local Nix store. (Today's `repro` stub gains this behavior at convergence.)

**`cargo xtask k8s replay`.** The subcommand and its laptop-orchestrated run loop (tunnel + russh pool + timeline + classification on the operator machine) are retired in Phase 5. Its three roles are redistributed: launching is `launch --archive`, dev-scale interactive runs are `dev`, and the offline plan is `dev --dry-run`. This deliberately adjusts the original convergence decision, which had `cargo xtask k8s replay` itself become the thin launcher: the launcher role lands as `replay launch --archive`, while the `k8s replay` subcommand name retires rather than surviving as a permanent alias. During Phase 5 the old name may remain as a thin alias that prints the new invocation; it is removed afterwards so there is exactly one entry point.

### 10.2 Spec and ConfigMap surface

The transport between xtask and the engine is unchanged: a ConfigMap named `<campaign-id>-spec` with key `spec.json`, mounted read-only at `/etc/rio/replay/spec.json` (`SPEC_MOUNT_PATH`), plus the `rio-replay-ssh` Secret (one key file per tenant under `/etc/rio/replay-ssh/<tenant>`) and the copied `rio-service-hmac` Secret at `/etc/rio/hmac/service-hmac.key`. Pod env stays `RUST_LOG`, `AWS_REGION`, `AWS_USE_DUALSTACK_ENDPOINT`, `RIO_REPLAY_S3_BUCKET`, `HOME=/work`, `XDG_CACHE_HOME`, `TMPDIR`. Mount paths, Secret names, namespace (`rio-replay`), ServiceAccount, and IRSA wiring carry the neutral subsystem name set the Phase 5 operator-convergence cutover renamed them to (§11.7), together with the S3 root, crate, and command-family renames.

The spec itself evolves in place (exact field schemas are owned by §6 The replayer; this is the surface shape):

| Spec block | Today | Converged |
|---|---|---|
| `campaign_id`, `mode`, `s3`, `cluster`, `tenants`, `filters`, `knobs`, `cluster_versions`, `deadline` | as on the branch | unchanged |
| `eval_set: EvalSetRef {hydra_eval_id, key_digest, s3_bucket, s3_prefix}` | pins an eval set | replaced by an archive reference `{s3_bucket, s3_prefix, digest}`; the recorder-specific eval id lives in archive provenance, not in the spec |
| `hydra: HydraBlock {cache_url, user_agent, buildstatus_file}` | campaign-time truth source | retired at Phase 3: expected outcomes are read from the archive; `buildstatus_file` becomes a recorder input |
| — | — | `scheduling {mode}` (§9); the timed-mode parameters (`speedup`, `max_sessions`, `connections`, `replay_interruptions`, …) live in `knobs` (§6.8) |
| — | — | `supply {dependencies, delivery}` (§8 Supply planning); the measurement rule of §8.1 step 0 (outputs of attemptable units are never supplied) is an invariant, not a knob |
| — | — | `report {policies: [...], fail_on}` — parity policy parameters (denominator rules) and/or regression-gate parameters (§7) |

`launch` continues to be the only writer of the spec; the engine continues to be its only reader. Unknown spec fields remain tolerated so a newer xtask can launch against a one-version-older engine image during rollouts.

### 10.3 What runs in-cluster vs locally

In-cluster, always:

- the evaluation-recorder Job (`record`) — evaluation, dependency-closure extraction, drv export, expected-outcome sweep, archive packaging, archive upload;
- the campaign Job (the replayer engine, §6) — plan, supply/prewarm, submission over ssh-ng, collection, classification, report rendering, S3 state sync. All build data-plane traffic (drv import, NAR upload, build submission, result collection) originates from this pod inside the cluster.

Operator-local (xtask):

- everything `launch` does before the Job exists: image assertion, tenant/Secret/ConfigMap provisioning, pre-flight, spec construction, archive upload for `--archive <local path>`;
- `status` and `report`, which read the Kubernetes Job object and S3 artifacts only;
- nothing after the Job is created is required from the operator machine — laptops can sleep, campaigns keep running (this is unchanged from today and is the reason the xtask-replay laptop loop retires).

External recorders (today nxb-replay) run wherever they run; their archives enter the system exclusively through `launch --archive` or a plain S3 upload to the archive prefix followed by `launch` against the digest.

### 10.4 Dev mode (k3s) and the offline dry-run CI test

Two dev affordances survive convergence, both deliberately small:

1. **`dev` mode.** `cargo xtask replay dev --archive <path> [--store ssh-ng://…|--provider k3s] [--limit N] [--schedule …]` runs the engine binary locally (no Job, no ConfigMap, `--no-s3`, state dir under the repo's scratch directory) against either an explicit `--store` endpoint or a provider port-forward to `svc/rio-gateway`, using the same pin-only host-key policy as the in-cluster engine: live dev runs must pass the gateway's host key via `--ssh-host-key` (the engine rejects a spec without `cluster.gateway_host_key`); only the offline `--dry-run` works without a key. This is the surviving remnant of the xtask-replay run loop, scoped to fixture- and smoke-sized archives for engine development and k3s cluster bring-up. It is not a measurement surface: no comparability block from a `dev` run is publishable, and the command says so in its output.
2. **Offline dry-run in CI.** `dev --dry-run` builds the plan and supply resolution fully offline (no cluster, no substituter probes) and prints the plan counts, exactly as the xtask-replay `--dry-run` does today. The committed fixture archive (today `xtask/tests/fixtures/replay/basic/` plus `basic.dwarfs`) moves with the archive reader when it re-homes in Phase 1, and the existing offline integration test (`dry_run_on_fixture_completes_offline`) is re-pointed at the engine's planner so the merge gate always exercises archive open → schedule → supply resolution without network access.

k3s engine runs go through `replay dev` (which wraps the engine's local input path and `--allow-unverified-tenants` directly); the `launch` path always runs the full pre-flight, so it targets clusters that can satisfy it. Nothing in the converged design is EKS-only except the IRSA/S3 plumbing that is already optional today (`s3.bucket = None` disables sync).

## 11. Migration & sequencing

### 11.1 Sequencing: implementation now, first live validation at deploy time

There is no live-campaign precondition on this migration. Implementation of all five phases proceeds immediately; no phase is gated on a campaign run against the old transport, and no live bucket-count baseline from the nix-CLI path is produced or required (the accepted residual risk of that absence is R5, §12.1). Phase-level validation is offline: the existing engine test suite over the `FakeSubmitter`/`FakeReader` scaffolding (extended so the fake backend exercises the client-ops shapes end-to-end), the golden report tests, the offline dry-run fixture test, the format round-trip and content-digest golden tests, and the gateway `client_ops_conformance` tests including the multi-root per-root case (§11.4). The first live validation is a smoke campaign (10–50 jobs, leaf mode) run on the converged system once the operator deploys it; it keeps the smoke-scale exit criteria from the campaign design — a scope containing at least one unit the target is expected to fail and at least one unit whose recorded outcome is `failed`, zero `target-substituted` under the leaf tenant, a same-day spot-audit of verdict and disposition assignments, resume proven by killing the engine pod mid-submit with identical final counts, supply-stage and archive-fetch wall clocks recorded, one sampled drvPath fidelity-gate run on the recorder side — and it is the first point at which live behavior is observed.

### 11.2 Phase overview

| Phase | Scope (one line) | Validation gate | Depends on | Lands independently? |
|---|---|---|---|---|
| 1 | Archive v1 format: spec text, reader/writer library, v0 ingestion, fixtures | Fixture round-trips; v0 fixture reads bit-identically through the compat path; content-digest golden test | — | Yes — pure library, no engine behavior change |
| 1a (parallel with 1) | Cherry-pick the rio-nix client ops + FramedWriter + gateway conformance test | `client_ops_conformance` green in the checks gate | — | Yes — additive library + test code |
| 2 | Engine submission/collection replaces the nix-CLI path outright with the client-ops transport behind the `Submitter` seam (gateway per-root results land as a prerequisite) | Offline: multi-root `client_ops_conformance` case, fake-backend end-to-end over the client-ops shapes, golden report tests; checks gate green | 1a | Yes (does not need Phase 1) |
| 3 | Evaluation recorder emits v1 archives (DwarFS-packed, truth baked in); pre-v1 eval sets abandoned per the deployment policy (§2.5) | Recorder output round-trips through the v1 reader; outcome-mapping and content-digest golden tests; offline dry-run over a recorder-produced archive | 1 | Yes (does not need Phase 2) |
| 4 | Timed scheduling mode + client-upload supply rungs land in the engine | nxb-production archive replayed at small scale on a dev cluster; disconnect replay observed; dispatch-lateness reported; prewarm/inline both exercised; upload-throughput and plan-memory measurements recorded | 1, 2 | No — needs both predecessors |
| 5 | Operator convergence: launcher absorbs `xtask k8s replay`, vocabulary renames, the legacy `parity`→`replay` identifier cutover, spec/tracey/doc updates, retire the branch | One campaign end-to-end through the new surface (the first live smoke campaign, §11.1); rename mapping proven count-preserving on a stored results file | 1–4 | No |

Phases 2 and 3 are intentionally independent of each other (transport vs input format) so neither blocks the other. There is no merge-ordering precondition ahead of any phase: implementation starts immediately (§11.1) and no phase waits on a live campaign. Phase 1a is additive library and test code that carries its own tests and changes no engine behavior.

### 11.3 Phase 1 — archive v1 format and reader/writer

Scope:

- Re-home the v0 reader (`xtask/src/k8s/replay/archive.rs` on origin/xtask-replay) into a library crate owned by the engine side (crate/module placement per §6 The replayer; the constraint here is only that xtask stops owning it). The `dwarfs` dependency moves with it.
- Extend it to v1 per §4 Archive format v1: `format_version`, capability flags, the neutral expected-outcome vocabulary, dependency-closure data, provenance block, content-addressed identity, completion-marker upload discipline, S3 layout.
- Add the writer half (used by the Phase 3 recorder) and the v0 ingestion path (§4 owns the exact compat semantics; the requirement here is that a v0 archive — including every archive nxb-replay has already produced — opens without re-recording: it maps onto the full v1 in-memory model, is inspectable, and plans offline timed dry-runs, with the engine's ATerm fallbacks covering the members v0 never carries. Live campaigns additionally require the v1 content-addressed identity — campaign pinning, resume, and publication key on `archive_id`, which v0 archives lack — so a live campaign over a v0 recording is out of scope until an identity for id-less archives is designed).
- Move the committed fixture archive with the reader; add a v1 sibling fixture.

Validation: unit round-trip tests for every file in the format; the v0 fixture parses to the same in-memory model through the compat path as it does today through the v0 reader; a golden test pins the content-digest computation (same discipline as the EvalSetKey golden digest). No engine behavior changes in this phase.

### 11.4 Phase 2 — transport swap behind the Submitter seam

Scope:

- Implement a client-ops submitter and an in-band result reader on the rio-nix ops from Phase 1a (`client_build_paths_with_results`, `client_query_valid_paths`, `client_query_path_info`, `client_add_to_store_nar`, `client_add_multiple_to_store`, typed stderr drain, `FramedWriter`), behind the existing `Submitter`/`ResultReader` seams, with the SSH channel pool and host-key policy re-homed from the branch's `client.rs`. §6 The replayer owns the design (what replaces stderr scraping, what remains of GetBuildGraph/ListPoisoned for infra attribution, and the disposition of the deferred QueryDerivationStatuses RPC).
- The gateway half of the swap lands in the same phase, as a prerequisite: `rio-gateway`'s `handle_build_paths_with_results` records true per-root results instead of cloning the DAG-level `BuildResult` (§6.4 — the single rio-side change of this convergence, carved out as the N3 exception), the multi-root extension of `client_ops_conformance.rs` (one failing root among several, per-root statuses asserted), and the wording update to the corresponding `gw.opcode.*` rule. Per the deployment policy (§2.5) any cluster the engine runs against is deployed with this gateway change in place; no older-gateway detection or fallback path exists in the engine.
- Derivation import in this phase is a minimal drv-text upload path owned by the submitter: parse the batch's `.drv` ATerms from the eval-set drv archive (the same files `nix copy` imports today), compute their text path infos, and upload the missing ones with `client_add_multiple_to_store` in reference order. The full supply planner — ladder, claims, prewarm, embedded/relayed store paths — waits for Phase 4; dependency outputs keep coming from the warm stage and the target's substituters exactly as today.
- The nix-child path (`nix copy` + `nix build` per batch) is replaced outright, not run alongside: `NixSubmitter`, the `GetBuildGraphReader` collection wiring, and the `--require-qds`/QDS-probe surface are deleted in the same change that lands the client-ops submitter. There is no `transport` spec field, no A/B harness, and — per the deployment policy (§2.5) — no emergency-reversion path. The warm stage's prefetch submission is outside this phase's scope: it keeps its current shell-out form until the Phase 4 supply planner absorbs it (§8.2).

Validation — offline, with no live run against the old transport (§11.1):

- The multi-root conformance case is green: one failing root among several, per-root statuses and error messages asserted against what the scheduler reported.
- `client_ops_conformance` as a whole and the full checks gate stay green throughout.
- The fake-backend end-to-end test (the engine driven against `FakeSubmitter`/`FakeReader`) is extended to the client-ops shapes — in-band per-root results, transport-error requeue, channel-abandon cancellation — and the golden report tests pass on its output, pinning bucket assignment and report rendering across the swap.
- The two-signal attribution tests (`resolve_failure_kind`, `classify_reason`) carry over unchanged and pass with Signal 1 sourced from per-root `error_msg`.
- The first live exercise of the transport is the deploy-time smoke campaign on the converged system (§11.1), whose same-day spot-audit of failure-class verdicts stands in for the bucket-count comparison an A/B would have provided; the accepted residual risk of having no old-transport baseline is recorded as R5 (§12.1).

### 11.5 Phase 3 — the evaluation recorder emits v1 archives

Scope:

- The eval Job's final phases change from "write six artifacts + upload" to "write the archive members into a staging directory, sweep expected outcomes, pack the directory into a DwarFS image with `mkdwarfs`, upload the image plus the standalone `manifest.json` and `complete.json` with the completion marker last" (§4.11). The expected-outcome sweep is today's campaign-side hydra-truth stage (cache.nixos.org narinfo presence, NarHash, NarSize, optional exact buildstatus map for scoped sets) executed at archive-creation time, with the same concurrency and retry discipline. The recorder Job image gains a pinned `mkdwarfs` in this phase (R2).
- The campaign engine gains the archive input path and stops performing the truth sweep when the archive carries the `expected_outcomes` capability.

Existing S3 eval sets are not migrated: per the deployment policy (§2.5), pre-v1 eval-set prefixes are simply abandoned — a scope still wanted as a campaign input is re-recorded as a v1 archive by re-running the recorder. There is no conversion tool and no permanent dual-reader. The engine's native eval-set reader is kept only until the archive input path is proven by the Phase 3 validation, and removed in Phase 5.

Validation, offline like the other phases (§11.1): the recorder's staged output round-trips through the v1 reader; the narinfo/buildstatus → expected-outcome mapping is pinned by golden tests; the offline dry-run consumes a recorder-produced archive of a small scope end-to-end (open → plan → supply resolution); and the recorder's own gates (fidelity, politeness budget accounting) are asserted unchanged by checking the same audit fields now landing in the archive's provenance block. The property the phase exists to deliver — truth frozen in the archive, so two campaigns over the same archive cannot drift on truth — holds by construction once the engine reads expected outcomes only from the archive, and is first observed live at the deploy-time smoke campaign, which runs from a recorder-produced v1 archive.

### 11.6 Phase 4 — timed scheduling and client-upload supply

Scope:

- The timeline scheduler (offset-driven dispatch, speedup, FIFO admission, dispatch-lateness accounting, cancellation/disconnect replay via channel abandon) re-homes from the branch's `timeline.rs` as the engine's timed mode (§9 Scheduling), gated on the archive's `timed` capability.
- The supply planner unifies (§8 Supply planning): the per-path source ladder, scheduler-side prefetch as the preferred delivery for paths the target's substituters cover (the existing warm machinery), client-side `AddMultipleToStore`/`AddToStoreNar` upload for embedded and relayed content, prewarm-before-clock vs inline supply, and the §8.1 step 0 measurement invariant. The branch's `supply.rs`/`prewarm.rs` (claims, topo-ordered batches, circuit breaker) re-home as the client-upload half.
- Verdict/disposition emission for the timed-only cases (cancellation, disconnect) lands with it (§7).

Validation: a small recorded archive from production traffic (an existing nxb-replay v0 archive read through the compat path, or its v1 re-record) replayed in timed mode against a dev cluster; recorded disconnects observably cancel sessions on the gateway; dispatch lateness and prewarm statistics appear in the report; the same archive replayed timeless produces the same outcome-verdicts (timing-only verdicts excepted). Engine-pod memory stays within the documented prewarm envelope (upload workers × batch byte cap).

Two measurements are required Phase 4 deliverables, recorded in the phase's report so the limits are known before anyone schedules a large run (they sit next to R1):

- **Engine-pod upload throughput**: the sustained MiB/s (and CPU cost) one engine pod pushes through the gateway ssh-ng path with `AddMultipleToStore`/`AddToStoreNar`. This number decides whether timed replays of production-scale windows require scheduler-side substituter coverage (thin archives + good target-substituter coverage) as a hard precondition.
- **Plan-time closure-graph memory**: the memory ceiling for loading and traversing a platform-slice `closures.jsonl` (~0.5 M-node adjacency graph) at plan time, recorded next to the throughput numbers.

### 11.7 Phase 5 — operator convergence, renames, retirement

Scope:

- `cargo xtask k8s replay` retires; `launch --archive`, `dev`, and `report --check` cover its roles (§10). The xtask-replay branch is closed.
- The verdict/disposition vocabulary from §7 replaces the bucket names in `results.jsonl`, `buckets/`, `progress.json`, and `summary.md`, using the rename mapping in Appendix C. This is a wire/schema change to campaign artifacts, not a code-only rename: the bucket strings are pinned by tests today, so the tests, the report renderer, and the comparability block's engine-version stamp move together. Old campaign artifacts in S3 are not rewritten.
- The legacy `parity` identifiers are renamed to the neutral subsystem name set in the same cutover: the S3 root (with the IAM/tofu grant update), the namespace/Secret/ServiceAccount/mount names, the campaign tenants, the engine crate and module paths, and the operator command family. Per the deployment policy (§2.5) this is a clean cutover on a wiped deployment — no aliasing or dual-read of the old names is carried, and no existing S3 state is migrated under the new root. The post-Phase-5 names, derived from the subsystem name **build replay**:

| Identifier | Until Phase 5 | After Phase 5 |
|---|---|---|
| Engine crate / module paths | `rio-parity` | `rio-replay` |
| Operator command family | `cargo xtask parity …` | `cargo xtask replay …` |
| S3 root + IAM grant | `parity/` (`parity/*`) | `replay/` (`replay/*`) |
| Namespace / ServiceAccount | `rio-parity` | `rio-replay` |
| Secret / mount names | `rio-parity-ssh`, `/etc/rio/parity/…`, `/etc/rio/parity-ssh/…` | `rio-replay-ssh`, `/etc/rio/replay/…`, `/etc/rio/replay-ssh/…` |
| Campaign tenants | `parity-leaf`, `parity-selfhosted`, `parity-warm` | `replay-leaf`, `replay-selfhosted`, `replay-warm` |

  After this cutover the word `parity` survives only as the name of the parity report policy (§7.3).
- Hydra-flavored identifiers inside the engine (`HydraOutcome`, `hydra.jsonl`, `hydraUnknownRatePct`, the `hydra` spec block) are renamed or retired per §6/§7. None of this touches tracey: rio-parity carries no spec markers and is not in the tracey include set; the only tracey-relevant changes in the whole migration are the Phase 1a conformance test (whose `r[verify gw.opcode.*]` markers attach to existing gateway rules) and the Phase 2 gateway per-root results change (which updates the wording of the corresponding `gw.opcode.*` rule and extends those markers); both are gated by `tracey-validate` like any other check.
- The knob dispositions of §6.8 land (retire `evidence_ttl_hours`, rename `hydra_unknown_threshold_pct` to `no_truth_threshold_pct`, keep `infra_low_confidence_pct` as a consumed low-confidence threshold). The nix-child submitter and the `--require-qds`/QDS-probe surface are not Phase 5 work — Phase 2 already deleted them when the client-ops transport replaced the nix-CLI path outright (§11.4); no fallback transport ever exists to clean up here.
- Documentation: this design document supersedes `docs/dev/2026-05-24-xtask-k8s-replay-design.md` (marked superseded, kept for history) and the parity design draft is updated to point at the converged terminology.

Validation: one campaign launched, monitored, and reported entirely through the converged surface — the first live smoke campaign (§11.1) — on a freshly wiped deployment carrying the renamed identifiers; a stored pre-rename `results.jsonl` (a fake-backend or dev-mode artifact from the earlier phases) re-rendered through the rename mapping yields identical per-bucket counts under the new names; `report --check` exercised in CI against a campaign launched with the regression-gate policy.

### 11.8 Fate of the xtask-replay branch

Absorbed (with mechanism and landing phase):

| Piece (origin/xtask-replay path) | How | Phase | Where it lands |
|---|---|---|---|
| `rio-nix/src/protocol/client.rs` additions (ClientOpError, drain_stderr_typed, KeyedBuildResult, client_query_valid_paths, client_query_path_info, client_build_paths_with_results, NarPayload, StoreEntry, client_add_to_store_nar, client_add_multiple_to_store) | cherry-pick as-is | 1a | rio-nix (library) |
| `rio-nix/src/protocol/wire/framed.rs` FramedWriter | cherry-pick as-is | 1a | rio-nix |
| `rio-gateway/tests/client_ops_conformance.rs` | cherry-pick as-is | 1a | rio-gateway tests |
| `xtask/src/k8s/replay/archive.rs` (ReplayArchive, Dir/DwarFS backends) + `dwarfs` dependency + fixtures | re-home and extend to v1 | 1 | archive library crate/module (§4, §6) |
| `xtask/src/k8s/replay/client.rs` (GatewayPool, DaemonChannel, HostKeyPolicy, channel budget) | re-home, key paths and sizing from the spec | 2 | engine transport module (§6) |
| `xtask/src/k8s/replay/substituter.rs` (narinfo probe, streaming NAR fetch, decompression) | re-home; consolidate with the engine's existing narinfo client | 4 | engine supply module (§8) |
| `xtask/src/k8s/replay/supply.rs` (workload rule, closure walk, source ladder, plan_uploads, UploadClaims) | re-home | 4 | engine supply planner (§8) |
| `xtask/src/k8s/replay/prewarm.rs` (supply context, prewarm phases, circuit breaker) | re-home | 4 | engine supply/prewarm (§8) |
| `xtask/src/k8s/replay/timeline.rs` (build_schedule, admission, disconnect replay, lateness, InFlightTracker) | re-home | 4 | engine timed scheduler (§9) |
| `compare.rs` verdict taxonomy and classification rules | absorbed as ideas, not code — the unified verdict/disposition model in §7 supersedes the enum | 5 | comparison model (§7), Appendix C carries the mapping |
| `--fail-on` exit-policy semantics | absorbed as the regression-gate report policy + `report --check` | 5 | §7, §10 |
| Fixture archives `xtask/tests/fixtures/replay/basic{,.dwarfs}` | move with the reader | 1 | archive crate test fixtures |

Dropped (retired without replacement in kind):

| Piece | Why | What covers the need |
|---|---|---|
| `xtask/src/k8s/replay/mod.rs` run_live orchestration (ReplayArgs, tunnel/endpoint selection for the run loop, console summary block, heartbeat/--watch wiring) | the laptop is not a durable place to run multi-hour measurement; the engine already owns resume, watchdog, S3 sync | in-cluster engine + `launch --archive`; dev-scale remnant in `dev` |
| `report.rs` (Summary/summary.json/console rendering/exit_code) | superseded by the engine's progress.json/report artifacts and per-campaign report policies | §6 report stage, §7 policies, `report --check` |
| `compare.rs` as code | vocabulary is replaced by the neutral verdict/disposition model | §7 |
| `--store/--ssh-key/--ssh-host-key` arbitrary-endpoint surface as a first-class measurement path | measurement runs are in-cluster against spec-named endpoints; arbitrary endpoints remain only in `dev` | §10.4 |
| `docs/dev/2026-05-24-xtask-k8s-replay-design.md` as a live design | superseded by this document | marked superseded, retained for history |

The branch is closed after Phase 5 without a wholesale merge; everything that survives arrives via the cherry-picks and re-homes above, each landing with its own tests.

### 11.9 Existing S3 artifacts

- **Eval sets** (`parity/evals/<eval>/<digest16>/`): abandoned in place at Phase 3 per the deployment policy (§2.5). They are not converted and nothing reads them after the eval-set input path is removed in Phase 5; a scope still wanted as a campaign input is re-recorded as a v1 archive (§11.5). The prefixes are not rewritten or deleted — they simply stop mattering.
- **Campaign artifacts** (`parity/campaigns/<campaign-id>/`): never rewritten. Pre-rename campaigns keep the old bucket vocabulary in their stored `results.jsonl`/`summary.md`; the comparability block's `engine_version` and the Appendix C mapping make cross-era reading unambiguous. Report tooling is not required to re-render old campaigns.
- **nxb-replay archives** (operator-held `.dwarfs` files): readable forever via the v0 ingestion path decided in §4 — opening, inspecting, and offline dry-run planning never require re-recording (and could not: they are recordings of past production windows). Live campaigns key identity, resume, and publication on the v1 `archive_id`, which v0 archives lack, so they are not live-campaign inputs today.

## 12. Risks & open questions

### 12.1 Risks

**R1 — Engine pod as a data-plane bottleneck for client-side uploads.** Today bytes never transit the engine: warm-stage substitution is scheduler-side, and submission moves only `.drv` closures. With client-side `AddMultipleToStore`/`AddToStoreNar` as a supply rung, embedded and relayed NARs flow through one pod (decompression of relayed NARs, NAR serialization of embedded trees, SSH encryption), against a platform-slice scale of ~30–100k jobs and on the order of 200 GB of uncompressed NAR per system. The prewarm worker pool's documented memory envelope (workers × per-batch byte cap, ~2 GiB at defaults) bounds memory but not wall-clock or pod network. Mitigations already in the design: the planner prefers the scheduler-side prefetch rung for anything the target's substituters cover, so client upload is the exception, not the rule; fat archives are positioned for durability and small/medium scopes, not as the default platform-slice mechanism. Residual risk: a timed replay of a large recorded window with poor target-substituter coverage may not be able to keep up with the recorded cadence from a single pod. Phase 4 validation must measure sustained MiB/s through the gateway path and record it in the report so the limit is known before anyone schedules such a run.

**R2 — mkdwarfs pinning in the recorder image.** Reading `.dwarfs` images is pure Rust (the `dwarfs` crate) and uncontroversial. Writing them requires the external `mkdwarfs` tool, and with the image as the only published S3 form (§4.1, §4.11) the evaluation recorder Job image must carry it from Phase 3 onwards (today the tool exists only on the nxb-replay side). The packaging question is settled; the residual risk is operational: keep `mkdwarfs` version-pinned through Nix in the recorder image (and available in the dev shell for `launch --archive` packing of local directories), and record the tool version in `provenance`. A version change cannot alter an archive's identity — `archive_id` is computed over the logical members, not the image bytes (§4.5) — but unpinned drift could still change image-level compression or compatibility characteristics between recordings.

**R3 — Fat-archive size envelopes.** A fat archive at platform-slice scale would reach hundreds of GiB before compression. S3 object limits are not the constraint; the recorder Job's scratch (200–400 Gi today), the engine pod's ephemeral storage (100 Gi today), and whole-archive download on campaign start are. The format work in §4 sizes the envelopes; the operational consequence is that fat archives above the engine's local headroom either need ranged/streamed reads from S3 (DwarFS is seekable; plain directories are not, as S3 prefixes they are) or are simply refused by `launch` with a clear error. Until a streaming reader exists, `launch` must enforce a size ceiling against the pod's ephemeral allocation rather than letting the kubelet evict the pod mid-campaign.

**R4 — Gateway channel-budget coupling.** The client transport multiplexes a fixed 4 channels per SSH connection and sizes connections as ceil(max_sessions/4). The 4 is a client-side fan-out choice (it bounds the blast radius of one dropped connection), not a mirror of any gateway constant: the gateway's per-connection bound is two orders of magnitude higher (512, operator-configurable, and exceeding it terminates the whole connection rather than rejecting the open), and its real exec admission is a global session cap. The residual coupling is one-directional — the client fan-out must stay below the deployed gateway's per-connection bound, since a gateway configured below it would terminate campaign connections and kill sibling in-flight builds. There is no wire-level discovery of the gateway's bound and none is planned. Mitigations: a compile-time assertion in rio-gateway pins the default bound above the engine's fan-out; the engine logs its fan-out and connection count at startup; and a burst of exec refusals (the gateway shedding load at its global session cap) is treated as a capacity/configuration problem rather than per-unit infra verdicts.

**R5 — Equivalence risk in the transport swap.** In-band `BuildPathsWithResults` results replace two independent evidence paths (relayed stderr lines and graph reconstruction), and the swap is a clean cutover: the nix-CLI submitter is never run alongside the client-ops transport, so there is no live bucket-count baseline from the old transport to compare the new one against. That absence is the accepted risk — a subtle attribution difference (especially infra-vs-genuine on flaky builders, and anything that today depends on `ListPoisoned` timing) cannot show up as an A/B mismatch and may only surface at M2 scale. Mitigations: the gateway conformance tests (including the multi-root per-root case), the fake-backend end-to-end test over the client-ops shapes, and the golden report tests pin status mapping, attribution, and bucket assignment offline (§11.4); the first live smoke campaign at deploy time (§11.1) is kept small and carries a same-day spot-audit of every failure-class verdict; and the evidence retained per failure (per-root `error_msg`, relayed reason lines, `stderr_tail`, `list_poisoned`) supports forensic re-attribution after the fact. Residual risk: an attribution difference that only manifests at M2 scale is found late; the recourse is the retained evidence and the spot-audit discipline, not a transport revert.

**R6 — Truth staleness is invisible by design.** Baking expected outcomes at archive creation means a campaign never sees upstream changes after that point (a path later garbage-collected from cache.nixos.org, a Hydra job later restarted). That is the intended trade (reproducible comparisons, no campaign-time queries), but it moves the freshness question to the operator. The report should surface archive `created_at` and age prominently in the comparability block so a stale-truth comparison is at least visibly stale.

**R7 — Timed resume degrades fidelity.** The engine's resume contract (pod restart, re-download state, skip terminal work) predates timed scheduling. §6.7 defines the continuation — terminal outcomes kept, missed requests re-anchored so recorded relative spacing is preserved, `timing_degraded: true` recorded in `progress.json` and the report — so the behavior is defined, but the lateness distribution and interruption-reproduction numbers of a resumed timed run are not comparable to an uninterrupted run's. The report flags it; nothing recovers the lost fidelity, and operators who need clean timing numbers must re-run the campaign.

**R8 — Multi-session tenancy is deferred.** Recorded sessions all replay under one campaign tenant; archives that semantically depend on tenant isolation (rio-recorded multi-tenant traffic) cannot be represented faithfully until the deferred manifest extension lands behind a format-version bump. This is an accepted non-goal (§2.5, §4.10), recorded here so nobody mistakes single-tenant replay results for a tenancy-correctness statement.

### 12.2 Open questions

None remain. The review draft carried eleven open questions (Q1–Q11); all were resolved on 2026-05-28 and the decisions are reflected in the body of this document (a later same-day decision, recorded below, supersedes the A/B-related clauses of two of them):

- engine-pod upload throughput and plan-time closure-graph memory became required Phase 4 measurements (§11.6), not design questions;
- archives at rest in S3 are DwarFS-only, with the directory form demoted to a local working/dev representation (§4.1, §4.11, §5.1);
- the legacy `parity` identifier rename is folded into the Phase 5 cutover with a fixed neutral name set (§11.7);
- per-root `BuildResult.error_msg` is the primary failure-signature and Signal-1 source, with the relayed stderr lines as evidence and fallback, and signature stability is part of the Phase 2 A/B (§6.6, §11.4);
- the dev-only full-wipe deployment policy (§2.5) removed the older-gateway detection/fallback, the eval-set conversion path, and the long-lived nix-cli reversion fallback (§6.4, §11.4, §11.5);
- prefetch shortfall above `prefetch_shortfall_pause_pct` hard-pauses the campaign before execution (§6.8, §8.4);
- the recorded per-job repro is engine-native (`xtask … repro`), with the nix CLI line kept only as a human convenience in report text (§6.4, §10.1);
- `report --check` is the single CI consumption point for the regression gate (§7.3, §10.1);
- `resource-exhausted` is a first-class expected-outcome value in v1.0 (§4.6, §4.12, §5.2, §7.1);
- `exclusions.jsonl` enters completeness accounting when present and incurs no penalty when absent (§4.7, §7.3).

**Decision (2026-05-28) — clean cutover for the engine transport.** This decision supersedes the earlier "fallback kept only for the duration of the Phase 2 A/B" wording, wherever it appeared in prior drafts and in the resolutions above. The client-ops transport is the only submission/collection transport: the nix-CLI submitter (`NixSubmitter`), the `transport: "client-ops" | "nix-cli"` spec field, the A/B validation harness, and `GetBuildGraph`'s collection role are not implemented or retained at all — Phase 2 replaces the old path outright (§6.4, §6.5, §11.4); the `--debug-graph` triage dump and `ListPoisoned` Signal-2 evidence stay. With the A/B gone, the M1-first ordering constraint is removed as well: implementation of all five phases proceeds immediately, phase validation is offline (gateway conformance, fake-backend end-to-end, golden report and format tests), and the first live validation is the smoke campaign on the converged system at deploy time (§11.1). The accepted consequence — no live bucket-count baseline from the old transport — is recorded as R5 (§12.1).

The risks in §12.1 remain live and are tracked there.

## 13. Appendices

Naming convention for these appendices: the left-hand ("today") columns use the exact identifiers that exist in the code on this branch and on origin/xtask-replay; the right-hand columns use the v1 member/field names defined in §4 Archive format v1 and the verdict/disposition names defined in §7 Comparison model.

### Appendix A — Field-level mapping: eval-set artifacts → archive v1

Today's eval set is six S3 objects under `<prefix>/evals/<hydra_eval_id>/<key_short_digest>/` plus two campaign-time inputs (the narinfo truth sweep and the optional buildstatus file) that move to archive-creation time. The v1 member names referenced below are the ones defined in §4.1 (`manifest.json`, `requests.jsonl`, `outcomes.jsonl`, `units.jsonl`, `closures.jsonl`, `impure-env.json`, `exclusions.jsonl`, `narinfo/`, `nix/store/`).

**A.1 `evalset.json` (`EvalSetMeta`) → archive `manifest.json`**

| Eval-set field | Type | v1 destination |
|---|---|---|
| `key` (`EvalSetKey`: `hydra_eval_id`, `project`, `jobset`, `systems`, `scope`, `engine_version`, `nix_version`, `nix_eval_jobs_version`, `args_expr_sha256`, `forced_at`) | object | `provenance.*` (opaque to the replayer; preserved verbatim for audit) |
| `key_digest`, `key_short_digest` | string | `provenance.*`; archive identity is `archive_id` (§4.5), not the eval-set key digest |
| `hydra_eval_id` | u64 | `provenance.*` (also surfaces in operator annotations; never read by the engine) |
| `nixpkgs_revision`, `source_store_path`, `rev_count`, `short_rev` | string/u64 | `provenance.*` |
| `project`, `jobset`, `jobset_config` | string/JSON | `provenance.*` |
| `evaluator_program`, `evaluator_argv` | string/array | `provenance.*` |
| `systems` | array | `provenance.*` (campaign-side system filtering uses per-unit labels, A.2) |
| `scope` (`Scope`: `full` \| `constituents{aggregate_job}` \| `jobs{jobs}`) | tagged object | `provenance.*` |
| `dry_run` | bool | not packaged (dry-run eval sets are never packaged as archives) |
| `fidelity_divergent` | bool | `provenance.*` (recorder quality flag; a divergent recording should normally not be packaged) |
| `stats.manifest_records` | usize | `counts.requests` and `counts.workload_units` (the recorder synthesizes one request per unit, A.2, so both equal the manifest record count) |
| `stats.in_scope_jobs`, `stats.eval_errors`, `stats.aggregates_excluded`, `stats.dep_closure_records`, `stats.ca_outputs`, `stats.hydra_requests_used`, `stats.archive_bytes` | usize/u64 | `provenance.*` |
| `created_at` | string | `created_at` |
| — | — | new in v1: `format_version`, `capabilities` (`timed`, `expected_outcomes`, `output_hashes`, `embedded_store_paths`, `impure_env`, `dependency_closures`), `substituters.relay`/`.target`, `files`, `content_digests` |

**A.2 `manifest.jsonl` (`ManifestRecord`, camelCase wire names) → `units.jsonl` + `requests.jsonl`**

| Field | Type | v1 destination |
|---|---|---|
| `job` | string | `units.jsonl` `label`; used by filters and report grouping, never by the engine's execution logic |
| `system` | string | `units.jsonl` `system`; drives the systems filter |
| `attr` | string | not carried as a dedicated v1 field (the `label` carries the job name); recorders that want it keep it in an extra ignored member or in `provenance` |
| `drvPath` | string | `units.jsonl` `drv`, plus one synthesized record in `requests.jsonl` (`session: 0`, `targets: [{drv, outputs: ["*"]}]`, no offsets — the archive is timeless) |
| `outputs` (name → store path) | map | `units.jsonl` `outputs`; joins against the expected-outcome records (A.5) |
| `requiredFeatures` | array, optional | `units.jsonl` `required_features` (feeds the feature-exclusion filter) |

**A.3 `eval-errors.jsonl` (`EvalErrorRecord`) and `fidelity.json` (`FidelityReport`)**

| Field | v1 destination |
|---|---|
| `EvalErrorRecord.attr`, `.error` | `exclusions.jsonl` records with `reason: "eval-error"` (`label` = attr, `detail` = error message); drive the `eval-error` disposition (§7.2) |
| `FidelityReport.mode`, `.checked`, `.matched`, `.mismatches[{job, local_drv, hydra_drv}]`, `.missing_locally`, `.missing_on_hydra`, `.divergent` | `provenance.*` summary, plus `fidelity.json` retained verbatim as an extra ignored member (§5.1); per-unit mismatches from an exhaustive check are additionally written as `identity_divergent: true` on the affected `units.jsonl` records (§4.7), which is what the engine's `identity-divergent` disposition reads (§7.2) |

**A.4 `dep-closure.jsonl` (`DepClosureRecord`) → `closures.jsonl`**

| Field | Type | v1 destination |
|---|---|---|
| `job` | string | not needed (`closures.jsonl` is keyed by derivation path, not by unit label) |
| `drvPath` | string | `closures.jsonl` `drv` — one record per derivation in the union closure; per-unit transitive closures are reconstructed at plan time (§4.7) |
| `deps[]` (`DepDrv.drvPath`, `.outputPaths`) | array | `closures.jsonl` `inputs` (direct adjacency) and `outputs` (declared output paths) |
| `caOutputs[]` (`CaOutput.drvPath`, `.output`) | array | `closures.jsonl` `outputs` entries with value `null` (floating content-addressed outputs) |

`closures.jsonl` is not mechanically derivable from `dep-closure.jsonl`: the eval-set member stores per-target *transitive* dependency lists with output paths and records no `inputSrcs`, while the archive member needs *direct* adjacency (`inputs`), `srcs`, and `outputs` for every derivation in the union closure. The recorder therefore derives `closures.jsonl` by parsing the `.drv` ATerms it embeds under `nix/store/`; `dep-closure.jsonl` serves only as a cross-check of per-unit output coverage. A recorder that skips the ATerm pass sets `dependency_closures = false` and the engine falls back to its own ATerm walk (§4.7).

**A.5 Campaign-time truth inputs → expected outcomes baked at creation**

| Today (campaign-time) | v1 destination |
|---|---|
| `hydra.jsonl` — the hydra-truth stage's narinfo sweep cache (per output path: present/absent, NarHash, NarSize) | `outcomes.jsonl` records (`outcome: "built"` with per-output `nar_hash_hex`/`nar_size`); sweep performed by the recorder at creation time (§5.1) |
| `spec.hydra.buildstatus_file` (JSON map job → Hydra buildstatus) | folded into the same `outcomes.jsonl` records at creation (`built` for 0, `failed` for nonzero); the raw native code retained in `detail` |
| narinfo absence (today ⇒ `HydraOutcome::Unknown`) | `outcome: "unknown"` (replays as `no-truth`) |

**A.6 `drvs.tar.zst` → embedded derivations**

| Today | v1 destination |
|---|---|
| `nix copy --derivation` export of every target's `.drv` closure into a `file://` binary-cache layout, tar+zstd | `nix/store/<hash>-<name>.drv` ATerm files in the archive (already the v0 shape); embedded source/output paths, when packaged fat, land as `nix/store/<hash>-<name>/` trees with `narinfo/` sidecars |

**A.7 S3/upload discipline**

| Eval-set discipline (today) | Archive v1 discipline |
|---|---|
| Prefix `<prefix>/evals/<hydra_eval_id>/<key_short_digest>/` | `<root>/archives/<archive_id_short>/` (§4.11) |
| `UPLOAD_ORDER` with `evalset.json` strictly last | data objects first, `manifest.json` next, `complete.json` strictly last (§4.11) |
| Completion marker PUT with `If-None-Match: *`; prefix is write-once | identical: conditional PUT of the marker; write-once prefixes |
| `--force` salts `EvalSetKey.forced_at` to fork a new prefix | re-recording produces new content and therefore a new digest; any retained force salt lives in provenance |
| Local dir mirrors the S3 layout | unchanged principle |

### Appendix B — Mapping: nxb-replay v0 archives → v1 neutral vocabulary

nxb-replay defines and writes the v0 contract today; it keeps working unchanged as a v0 producer, and the v1 adoption it eventually needs is confined to manifest additions and the outcome mapping below, applied at record time. Field names on the left are the v0 reader's view (origin/xtask-replay `archive.rs`) and the nxb-replay writer's documented fields.

**B.1 Manifest**

| v0 field | v1 |
|---|---|
| `from`, `to` (timestamps) | recorded window (unchanged) |
| `created_at` | unchanged |
| `src_substituters` | `substituters.relay` (unchanged role; https/s3 only, per the existing trust policy) |
| `target_substituters` | `substituters.target` (advisory; the authoritative target substituter set at replay time comes from the campaign spec/tenant snapshot, §8) |
| `fat` | kept as the advisory `fat` claim (§4.2); `capabilities.embedded_store_paths` states whether embedded paths are present |
| `requests`, `drvs`, `embedded_srcs` | `counts.requests`, `counts.embedded_drvs`, `counts.embedded_store_paths`, plus recomputed `counts.workload_units` |
| `src_nxb`, `nxb_replay_version` (writer-specific, ignored by the v0 reader) | `provenance.*` (opaque) |
| archive id = first 8 hex of sha256(from, to, sorted src_nxb, created_at) | superseded by `archive_id` (§4.5); the old id may be recorded in provenance |
| — | new in v1: `format_version`, `capabilities` (`timed = true` for these archives, `expected_outcomes`, `output_hashes`, `embedded_store_paths`, `impure_env`), `counts`, `files`, `content_digests`, `provenance` |

**B.2 `requests.jsonl` and `builds.jsonl` records**

| v0 field | v1 |
|---|---|
| `ReplayRequest.ssh_session_id` | `requests.jsonl` `session` (opaque grouping key; tenant mapping deliberately deferred, §4.10) |
| `ReplayRequest.offset_s` | `requests.jsonl` `offset_s` (`timed` capability) |
| `ReplayRequest.paths` = `[drv_path, [outputs]]`, `["*"]`/`[]` = all outputs | `requests.jsonl` `targets: [{drv, outputs}]`, normalized to `["*"]` for all outputs |
| `BuildRecord.ssh_session_id`, `.drv_path` | `outcomes.jsonl` `session` + `drv` (record key, unchanged semantics) |
| `BuildRecord.status` (i32 native code) | `outcomes.jsonl` `outcome` (table B.3); the native code retained in `detail` |
| `BuildRecord.status_msg` | `outcomes.jsonl` `detail` |
| `BuildRecord.duration_s` | `outcomes.jsonl` `duration_s` (sizes replay deadlines) |
| `BuildRecord.stop_offset_s` | `outcomes.jsonl` `stop_offset_s` (anchors cancellation/disconnect replay) |
| `BuildRecord.outputs` (name → `{nar_hash_hex, nar_size}`) | `outcomes.jsonl` `outputs` (drives hash comparison; `output_hashes` capability) |
| `impure-env.json` (drv → impure env var names) | unchanged member; drives the `demoted-impure` disposition |
| `narinfo/<hash>.narinfo`, `nix/store/…` | unchanged (`embedded_store_paths` capability) |

**B.3 Native status codes → neutral expected outcomes (§4.6 vocabulary; restates the normative §5.2 mapping with the v0 reader constants)**

| Native code (nixbuild.net `build.status`) | v0 constant | v1 `outcome` | Notes |
|---|---|---|---|
| 0 Built | `BUILT` | `built` | success with output hashes |
| 1 NixPermanentFailure | — (generic nonzero) | `failed` | deterministic failure |
| 4 NixOutputRejected | — (generic nonzero) | `failed` | recorder adds detail; treated as deterministic failure |
| 6 Cancelled | `CANCELLED` | `cancelled` | reproduction is timed-only behavior (§9.2) |
| 10 BuilderError | `BUILDER_ERROR` | `indeterminate` | infrastructure-dependent; yields no usable truth for outcome comparison |
| 13 ClientDisconnect | `CLIENT_DISCONNECT` | `disconnected` | timed-only disconnect replay; no outcome comparison |
| 16 OOM / resource exhaustion | `RESOURCE_EXHAUSTED` | `resource-exhausted` | failure-class expectation: compared like `failed` (§7.1) but reportable separately; the original code stays in `detail` |
| any other nonzero | — | `failed` | |
| no record for (session, drv) | — | absent (or an explicit `unknown` record) | cache hit at record time; replays as `no-truth` |

### Appendix C — Mapping: current parity buckets and replay verdicts → unified verdicts/dispositions

The vocabulary used below is the one defined in §7: verdicts `match-built`, `output-divergence`, `match-failed`, `unexpected-failure`, `unexpected-dependency-failure`, `unexpected-success`, `source-unavailable`, `infra-indeterminate`, `truth-indeterminate`, `no-truth`, plus the timed-only `interruption-replayed` and `interruption-not-reproduced`; dispositions `filtered`, `eval-error`, `identity-divergent`, `not-attemptable`, `cached-prior`, `target-substituted`, `demoted-impure`, `upload-rejected`, `supply-failed`, `not-attempted`. This appendix restates the §7.4 rename mapping in full and adds the engine-internal vocabularies; §7.3 defines which verdicts enter the parity headline numerator/denominator.

**C.1 Parity buckets (`Bucket`, kebab-case wire strings in `results.jsonl`/`buckets/`)**

| Today's bucket | Unified mapping | Notes |
|---|---|---|
| `match-built` | verdict `match-built` | records whose `narCompare` is `differs` become `output-divergence` |
| `rio-only-failure` | verdict `unexpected-failure` | target failed where truth built |
| `rio-dependency-failure` | verdict `unexpected-dependency-failure` | failing dependency drv retained via the `cascaded` attribute / failure detail |
| `rio-infra-failure` | verdict `infra-indeterminate` | |
| `upstream-source-unavailable` | verdict `source-unavailable` | stays out of the parity denominator |
| `target-substituted` | disposition `target-substituted` | |
| `cached-prior` | disposition `cached-prior` | |
| `not-attemptable` | disposition `not-attemptable` | |
| `not-attempted` | disposition `not-attempted` | deadline-partial backfill |
| `hydra-unknown` | verdict `no-truth` | |
| `eval-divergence` | disposition `identity-divergent` | recorder/scope-derived (read from `units.jsonl` `identity_divergent`, §4.7), not a run outcome |
| `hydra-only-failure` | verdict `unexpected-success` | we built where truth failed |
| `both-failed` | verdict `match-failed` | count-preserving: per §7.1's precedence rule, `match-failed` covers any replayed failure shape (own, dependency-cascaded, or fetch failure) when the expected outcome is `failed` — exactly today's both-failed membership |
| `eval-error` | disposition `eval-error` | |
| `skipped` | disposition `filtered` | filter/scope reasons carried as the disposition detail |

**C.2 Replay verdicts (origin/xtask-replay `compare.rs`)**

| Today's verdict | Unified mapping | Notes |
|---|---|---|
| `Match` (success with equal hashes) | verdict `match-built` | |
| `Match` (recorded failure that also failed) | verdict `match-failed` | the unified vocabulary splits what v0 folded into one variant |
| `Match` (recorded cancellation, timeless reading) | verdict `truth-indeterminate` | timed runs reproduce the interruption instead (`interruption-replayed`) |
| `NonReproducible {recorded, replayed}` | verdict `output-divergence` | both built, NAR hashes differ |
| `Regression {error, attempts}` | verdict `unexpected-failure` | retry count retained as the `attempts` attribute |
| `FailureNotReproduced {recorded_status}` | verdict `unexpected-success` | |
| `CancellationNotReproduced {recorded_status}` | verdict `interruption-not-reproduced` (timed only) | only possible when the archive is timed |
| `DisconnectReplayed` | verdict `interruption-replayed` (timed only) | informational; never a regression |
| `UploadRejected {error}` | disposition `upload-rejected` | supply outcome, not an outcome comparison; counted by the regression gate (§7.3) |
| `RequestError {kind, message}` | verdict `infra-indeterminate` | per-request infra noise; collapsed reporting rule (one record per all-infra request) is a report-layer concern |
| `Skip("impure environment not forwarded…")` | disposition `demoted-impure` | |
| `Skip("no recorded build (cache hit at record time)")` | verdict `no-truth` | |
| `Skip("recorded outcome was infrastructure-dependent")` | verdict `truth-indeterminate` | |
| `Skip("target already had the outputs")` | disposition `cached-prior` or `target-substituted` (split by whether validity predates the campaign) | |
| `Skip("output hashes could not be collected (infrastructure)")` | verdict `infra-indeterminate` | |
| `flaky` counter (requests with retries that settled clean) | `flaky: true` attribute on the verdict | not a verdict; survives as the retries section of the report |

**C.3 Internal vocabularies that do not change meaning**

The engine-internal warm dispositions (`not-found-upstream`, `already-present`, `no-static-producer`, `substituted`, `built-fallback`, `failed-after-retries`) become supply outcomes of the unified planner (§8 Supply planning) and feed dispositions; the collect-loop requeue reasons (`infra-auto-retry`, `dependency-failed-no-trigger`, `failfast-batch-mate`, `engine-cancelled`, `no-derivation-rows`) remain engine-internal and never appear in reports. Neither set is part of the renamed wire vocabulary, but both drop their source-flavored spellings if any appear when §6 finalizes module naming.

### Appendix D — Glossary of rio components (for readers outside the project)

| Term | What it is |
|---|---|
| rio-build | The build service under test: a Kubernetes-deployed system that accepts Nix build requests from clients and executes them on a fleet of builders. It does not evaluate Nix expressions; clients submit derivations. |
| rio-gateway | The client-facing endpoint. Speaks the Nix worker protocol over SSH ("ssh-ng"): clients open an SSH connection, run `nix-daemon --stdio` on a channel, and issue worker-protocol operations. The SSH key's comment selects the tenant. Per-tenant build policy (e.g. `keep_going`, `force_build_roots`) is configured in gateway config. Connections carry a small fixed number of channels each. |
| rio-scheduler | Owns the build DAG: merges submitted derivation graphs, decides what to substitute from upstream caches vs build, dispatches work to executors, tracks per-derivation status. Exposes an AdminService gRPC API (build graphs, poisoned-derivation lists, log tails) used for collection and triage. |
| rio-store | The content store service. Stores NARs/manifests (chunked, backed by S3) and answers path-info queries (`BatchQueryPathInfo`) used to confirm outputs and read NAR hashes. |
| executor / builder | The machines (spot instances in EKS, VMs in dev) that actually run builds dispatched by the scheduler. |
| tenant | The isolation unit: each tenant has its own substituter ("upstream") list, build policy, GC retention, and path ownership. The campaigns use dedicated tenants (`replay-leaf`, `replay-selfhosted`, `replay-warm`) so measurement traffic never mixes with other workloads. |
| upstream / substituter | A binary cache (e.g. `https://cache.nixos.org`) a tenant may fetch already-built store paths from instead of building them. |
| ssh-ng / Nix worker protocol | The wire protocol the stock `nix` client uses to talk to a remote daemon over SSH. The engine and recorders speak it as clients; rio-gateway implements the server side. |
| derivation / `.drv` / ATerm | The build recipe Nix produces at evaluation time, stored as an ATerm-format text file in the Nix store. A derivation declares inputs (other derivations and source paths) and outputs. |
| NAR / narinfo | NAR is Nix's archive serialization of a store path; a `.narinfo` is a binary cache's small metadata record for one store path (NAR hash, size, references, URL). NAR hash equality is how output equivalence is judged. |
| Hydra | The upstream NixOS CI system whose evaluations define the nixpkgs build set. One recorder reproduces a Hydra evaluation locally and packages it as an archive; the replayer itself never talks to Hydra. |
| nix-eval-jobs | The evaluator used by the evaluation recorder to enumerate a nixpkgs evaluation's jobs and derivation paths at scale. |
| eval set | The pre-archive packaging of a reproduced evaluation used by the current campaign engine (manifest, dependency closures, drv archive, metadata). Superseded by archive v1; pre-v1 eval sets are abandoned rather than converted (§2.5, §11.5). |
| campaign | One replayer run over one archive with one set of policies, executed as a Kubernetes Job, producing append-only state and a report in S3. |
| the engine | The `rio-replay` binary (the replayer) running inside the campaign Job: planning, supply, submission, collection, classification, reporting, resume. |
| xtask | The repository's operator CLI (`cargo xtask …`); creates and observes the in-cluster Jobs, never executes measurement work itself. |
| nxb-replay | An external recorder that captures nixbuild.net production build traffic into v0 archives (and packs them with DwarFS). It is one producer of archives; the replayer treats its output like any other archive. |
| DwarFS | A read-only compressed filesystem image format; published archives ship as a single `.dwarfs` file readable in-process without extraction. Writing images requires the external `mkdwarfs` tool. |
| k3s / EKS | The two deployment targets: k3s for local/dev clusters, EKS for the real measurement clusters (with IRSA-scoped S3 access for campaign artifacts). |
