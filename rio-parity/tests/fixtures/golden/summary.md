# Parity campaign c-golden-0001 — summary

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
| in scope / attemptable / attempted | 10 / 8 / 8 |
| completeness | 100.00% |

## Headline
- Build-outcome parity: **50.00%** (2 / 4)
- NAR-hash agreement (secondary, non-gating): 50.00% (1 / 2 compared jobs)
- Infra-failure rate (excluded from headline): 25.00%
- Hydra-unknown rate: 12.50%

## Buckets
| bucket | count | of which cascaded |
|---|---:|---:|
| match-built | 2 |  |
| rio-only-failure | 1 |  |
| rio-dependency-failure | 1 |  |
| rio-infra-failure | 2 | 1 |
| cached-prior | 1 |  |
| not-attemptable | 1 |  |
| hydra-unknown | 1 |  |
| hydra-only-failure | 1 |  |

## Top failure signatures
Signatures group byte-identical raw evidence (60-character message slugs); the same failure mode worded differently appears as separate rows, so these are NOT failure-mode counts.
- `dependency-failed`: 2
- `failed-every-worker`: 1
- `infra-retries-exhausted`: 1
- `poison-threshold`: 1

## NAR divergence top offenders
- differs.x86_64-linux

## Retries
- match-built on first attempt: 1 | after retries: 1

## Suspension windows
(none)

## Artifacts
- results.jsonl, hydra.jsonl, warm.jsonl, batches.jsonl, buckets/<bucket>.jsonl, logs/<job>.log.zst next to this file
