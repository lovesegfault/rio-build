# Plan 0104: Observability baseline — describe!() + instrument spans

## Design

Three commits closed the gap between `observability.md` (which documented 47 metrics and a trace structure diagram) and the actual `/metrics` output (which had zero `# HELP` text) and trace spans (worker had zero).

`3167946` added `pub fn describe_metrics()` to each binary crate, calling `describe_counter!`/`describe_gauge!`/`describe_histogram!` for every metric with descriptions sourced verbatim from the `observability.md` tables. 47 metrics described, 47 emitted — verified by diff of sorted name extraction. Called from each `main.rs` immediately after `init_metrics()`; `describe!()` is fire-and-forget metadata, safe before or after first emit (exporter merges at scrape time, first description wins on double-call). Per-crate placement rather than `rio-common`: `rio-common` lacks the `metrics` crate dep (only `metrics-exporter-prometheus`), and keeping descriptions next to emitters catches drift at grep time. Two phantom metrics deleted from docs: `rio_store_cache_hit_ratio` and `rio_worker_fuse_cache_hit_ratio` were documented but never implemented (the only reference was a stale doc-comment on `test_cache_len()` in `cas.rs`, also fixed).

`484c6d3` added `#[instrument]` spans to the worker build hot path — worker had **zero** instrument attributes; gateway had 22, scheduler 11, store 27. A black box in distributed traces. 11 spans added covering the full build lifecycle per `observability.md`'s trace structure diagram: root `execute_build` (`drv_path`, `worker_id`, `is_fod` fields), children `fetch_drv_from_store`, `compute_input_closure`, `fetch_input_metadata` (`input_count`), `generate_db` (`path_count`, `drv_output_count`), `spawn_daemon_in_namespace`, `run_daemon_build` (`build_timeout_secs` — this span ≈ actual build wall time), `upload_all_outputs` → `upload_output` (per-path) → `do_upload_streaming` (`retry` attempt). FUSE `fetch_and_extract` at `level=debug` — the slow path (cache miss → gRPC + NAR extract), at most once per path per worker lifetime. Not on `lookup`/`read` — those are hot (kernel caches attr for 1h TTL but still high-volume). House style: `skip_all` + explicit fields.

`cad6c3a` added the `rio_scheduler_log_forward_dropped_total` metric — a counter for logs dropped at the 1024-cap broadcast channel. Previously silent; now visible in dashboards.

## Files

```json files
[
  {"path": "rio-gateway/src/lib.rs", "action": "MODIFY", "note": "describe_metrics() fn; called from main after init_metrics"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "describe_metrics() fn"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "describe_metrics() fn"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "describe_metrics() fn"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "#[instrument] on execute_build root span; drv_path/worker_id/is_fod fields"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "#[instrument] on compute_input_closure, fetch_input_metadata"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "#[instrument] on spawn_daemon_in_namespace, run_daemon_build"},
  {"path": "rio-worker/src/executor/upload.rs", "action": "MODIFY", "note": "#[instrument] on upload_all_outputs, upload_output, do_upload_streaming"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "#[instrument(level=debug)] on fetch_and_extract"},
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "#[instrument] on generate_db; path_count/drv_output_count fields"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "log_forward_dropped_total counter"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "phantom rio_*_cache_hit_ratio metrics deleted; ratio note rewritten prescriptive"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "stale test_cache_len doc-comment rewritten"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[obs.metric.*]` and `r[obs.span.*]` annotations were placed on these files in the retroactive sweep; P0127 later removed several false `r[verify obs.*]` annotations (honest untested reporting — these are hard to unit-test).

## Entry

- Depends on P0101: the `runtime.rs` extraction made the `#[instrument]` additions land on stable file paths.

## Exit

Merged as `484c6d3..cad6c3a` (3 commits, non-contiguous). `.#ci` green at merge. 47 described == 47 emitted (diff-verified). Worker spans visible in Tempo after deploy (phase2b VM test exercises the OTLP exporter).
