<script lang="ts">
  // Generic rounded badge. The shape (padding/radius/weight) lives here
  // once; callers supply the colour pair from their own meta-table.
  // BuildStatePill and the Executors status column both render through
  // this so a palette/shape tweak is a one-file edit.
  let { label, bg, fg }: { label: string; bg: string; fg: string } = $props();
</script>

<span
  class="pill"
  data-state={label}
  style:background-color={bg}
  style:color={fg}
>
  {label}
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
