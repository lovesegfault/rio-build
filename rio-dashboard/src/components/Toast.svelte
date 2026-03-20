<script lang="ts">
  // Toast portal: the one place ToastMsg[] renders. App.svelte mounts
  // exactly one instance at the top level. Auto-dismiss is handled by
  // the store's push() — the component only renders and exposes a
  // manual-close affordance.
  //
  // position: fixed + high z-index is the "portal" — Svelte needs no
  // createPortal/teleport for a fixed-positioned stack that's mounted
  // once at the root. Keeps the component dependency-free.
  import { dismiss, toasts } from '../lib/toast';
</script>

<ul class="toast-stack" role="status" aria-live="polite">
  {#each $toasts as t (t.id)}
    <li class="toast toast-{t.kind}" data-testid="toast">
      <span>{t.text}</span>
      <button type="button" aria-label="dismiss" onclick={() => dismiss(t.id)}
        >×</button
      >
    </li>
  {/each}
</ul>

<style>
  .toast-stack {
    position: fixed;
    bottom: 1rem;
    right: 1rem;
    z-index: 1000;
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .toast {
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    background: #333;
    color: #fff;
    display: flex;
    gap: 0.5rem;
    align-items: center;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
  }
  .toast-error {
    background: #a33;
  }
  .toast button {
    background: transparent;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 1.1rem;
    line-height: 1;
    padding: 0 0.25rem;
  }
</style>
