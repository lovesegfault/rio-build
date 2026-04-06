<script lang="ts" module>
  // Single source of truth for "what color is Failed" — every pill across
  // the list view, drawer header, and (later) DAG-node legend imports
  // this module-scope export so palette edits happen in exactly one place.
  //
  // No BuildState import here: protobuf-es enums are `const enum`-shaped
  // numeric literals, and svelte-check flags cross-file `import { Enum }`
  // as value-used-as-type under isolatedModules. The table keys on the
  // raw wire values (0..5), which are what `BuildInfo.state` actually
  // carries — the enum is purely a naming convenience.
  export const STATE_META: Record<
    number,
    { label: string; bg: string; fg: string; filter: string }
  > = {
    // UNSPECIFIED — proto3's zero-default. Scheduler never sets this; if
    // it shows up, either the backend dropped the field or there's a
    // wire-schema skew. Render neutral grey so it reads as "unknown"
    // instead of a misleading "pending".
    0: { label: 'unknown', bg: '#e5e7eb', fg: '#374151', filter: '' },
    1: { label: 'pending', bg: '#dbeafe', fg: '#1e40af', filter: 'pending' },
    2: { label: 'active', bg: '#fef3c7', fg: '#92400e', filter: 'active' },
    3: { label: 'succeeded', bg: '#d1fae5', fg: '#065f46', filter: 'succeeded' },
    4: { label: 'failed', bg: '#fee2e2', fg: '#991b1b', filter: 'failed' },
    5: { label: 'cancelled', bg: '#f3f4f6', fg: '#6b7280', filter: 'cancelled' },
  };

  // Ordered for the filter-pill row. UNSPECIFIED omitted — you can't ask
  // the scheduler to filter on the zero value (ListBuildsRequest.status_filter
  // treats "" as "all", and "unspecified" isn't a valid PG status string).
  export const FILTERABLE_STATES = [1, 2, 3, 4, 5] as const;
</script>

<script lang="ts">
  let { state }: { state: number } = $props();
  // Future-proof the lookup: if the proto grows a BUILD_STATE_POISONED
  // before this table is updated, fall back to the "unknown" styling
  // instead of rendering `undefined` into the style attr.
  let meta = $derived(STATE_META[state] ?? STATE_META[0]);
</script>

<span
  class="pill"
  data-state={meta.label}
  style="background-color: {meta.bg}; color: {meta.fg}"
>
  {meta.label}
</span>

<style>
  .pill {
    display: inline-block;
    padding: 0.125rem 0.5rem;
    border-radius: 9999px;
    font-size: 0.75rem;
    font-weight: 500;
    white-space: nowrap;
  }
</style>
