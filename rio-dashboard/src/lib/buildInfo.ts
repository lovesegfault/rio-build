// Shared progress / timestamp formatters for BuildInfo rendering.
//
// EXTRACTED from Builds.svelte + BuildDrawer.svelte + Workers.svelte
// (consolidator-mc210: progress() was byte-identical 2×, proto-Timestamp
// → ms hand-rolled 4× with inconsistent nanos handling). Standardized on
// @bufbuild/protobuf/wkt's timestampMs — the canonical converter, which
// Builds.svelte's hand-roll predated.
//
// timestampMs(ts) = Number(ts.seconds)*1000 + Math.round(ts.nanos/1e6).
// The prior Builds.svelte code dropped nanos entirely; BuildDrawer.svelte
// included them via Math.floor. Sub-second precision is irrelevant for
// "5s ago" / "2m30s" display but the inconsistency was a latent
// assertion-churn trap for any future test pinning exact ms output.

import { timestampMs, type Timestamp } from '@bufbuild/protobuf/wkt';
import type { BuildInfo } from '../api/types';

/**
 * Percentage completion 0–100. completed + cached both count as "done" —
 * cached derivations short-circuit at merge time (scheduler's
 * row_to_proto: cached is "completed with no assignment row"). Clamped
 * at 100: during the scheduler's count-bump window it's possible to
 * briefly observe completed+cached > total.
 */
export function progress(b: BuildInfo): number {
  if (b.totalDerivations === 0) return 0;
  const done = b.completedDerivations + b.cachedDerivations;
  return Math.min(100, Math.round((done / b.totalDerivations) * 100));
}

/**
 * proto Timestamp → ms since epoch. Returns undefined for absent
 * timestamp (all BuildInfo timestamp fields are proto3-optional).
 */
export function tsToMs(ts: Timestamp | undefined): number | undefined {
  return ts ? timestampMs(ts) : undefined;
}

/**
 * Absolute ISO-8601 string, or "—" for absent timestamp. Scheduler
 * doesn't always populate startedAt/finishedAt yet (deferred to the
 * sqlx-chrono feature) — "—" keeps the drawer from reading as broken.
 */
export function fmtTsAbs(ts: Timestamp | undefined): string {
  const ms = tsToMs(ts);
  return ms !== undefined ? new Date(ms).toISOString() : '—';
}

/**
 * Relative "now" / "Xs ago" / "Xm ago" / "Xh ago" / "Xd ago", or "—"
 * for absent timestamp. `now` is injected for deterministic testing
 * and for the Workers page's re-render-on-poll pattern (bump a
 * `now = $state(Date.now())` signal, the {#each} body re-evaluates).
 */
export function fmtTsRel(ts: Timestamp | undefined, now = Date.now()): string {
  const then = tsToMs(ts);
  if (then === undefined) return '—';
  const delta = now - then;
  if (delta < 1000) return 'now';
  const s = Math.floor(delta / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/**
 * Human duration "15s" / "2m30s" / "1h12m" from startedAt→finishedAt
 * (or startedAt→now for in-flight builds). "—" when startedAt absent.
 */
export function fmtDuration(b: BuildInfo, now = Date.now()): string {
  const start = tsToMs(b.startedAt);
  if (start === undefined) return '—';
  const end = tsToMs(b.finishedAt) ?? now;
  const s = Math.round((end - start) / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${s % 60}s`;
  return `${Math.floor(s / 3600)}h${Math.floor((s % 3600) / 60)}m`;
}
