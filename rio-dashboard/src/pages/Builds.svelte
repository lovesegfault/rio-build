<script lang="ts">
  // TODO(P0278): ListBuilds → paginated table.
  // TODO(P0280): row click → /builds/:id → Graph page + node-click handler.
  // Stub exists now so App.svelte's <Route> wiring type-checks; real content
  // lands with the Builds-list plan.
  export let id: string | undefined = undefined;
</script>

<p>builds{id ? ` — ${id}` : ''} (placeholder)</p>
