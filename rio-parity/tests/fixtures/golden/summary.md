# Replay campaign c-golden-0001 — summary

Generated: 2026-05-26T12:00:00Z

## Comparability
| field | value |
|---|---|
| eval set | 8b919129046e0f60 |
| manifest sha256 | 8b919129046e0f608b919129046e0f608b919129046e0f608b919129046e0f60 |
| mode | leaf |
| build tenant | parity-leaf |
| systems | x86_64-linux |
| exclude features | kvm |
| include globs |  |
| limit | none |
| engine version | 0.1.0 |
| signature table | m1-raw-evidence |
| in scope / attemptable / attempted | 12 / 10 / 10 |
| completeness | 100.00% |

## Headline
- Build-outcome parity: **50.00%** (2 / 4)
- Output divergence (within headline): 1 jobs
- NAR-hash agreement (secondary, non-gating): 50.00% (1 / 2 compared jobs)
- Infra-indeterminate rate (excluded from headline): 20.00%
- No-truth rate: 10.00%

## Verdicts
| verdict | count | of which cascaded |
|---|---:|---:|
| match-built | 1 |  |
| output-divergence | 1 |  |
| unexpected-failure | 1 |  |
| unexpected-dependency-failure | 1 |  |
| unexpected-success | 1 |  |
| infra-indeterminate | 2 | 1 |
| no-truth | 1 |  |
| interruption-replayed | 1 |  |
| interruption-not-reproduced | 1 |  |

## Dispositions
| disposition | count |
|---|---:|
| not-attemptable | 1 |
| cached-prior | 1 |

## Top failure signatures
Signatures group byte-identical raw evidence (60-character message slugs); the same failure mode worded differently appears as separate rows, so these are NOT failure-mode counts.
- `dependency-failed`: 2
- `failed-every-worker`: 1
- `infra-retries-exhausted`: 1
- `poison-threshold`: 1

## NAR divergence top offenders
- differs.x86_64-linux

## Retries
- match-built/output-divergence on first attempt: 1 | after retries: 1

## Suspension windows
(none)

## Supply
(not recorded)

## Artifacts
- results.jsonl, supply.jsonl, dispatch.jsonl, batches.jsonl, buckets/<verdict-or-disposition>.jsonl, report/gate.json (when a regression gate was requested), logs/<job>.log.zst next to this file
