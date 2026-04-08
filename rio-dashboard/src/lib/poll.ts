// Shared poll-loop building block: fire `fn` immediately, then every `ms`,
// skipping ticks while the tab is hidden (Page Visibility API — true on
// tab-switch or window-minimize). Returns the teardown to hand back from a
// component `$effect`:
//
//   $effect(() => startPoll(refresh));
//
// Compose extra reactive deps / early-stop above the call (Graph.svelte
// does this for its `allTerminal` flag):
//
//   $effect(() => {
//     void reactiveDep;
//     if (done) { void fn(); return; }
//     return startPoll(fn);
//   });
//
// Kept as a non-rune helper (plain .ts, $effect lives in the caller) so
// callers retain control over which signals the surrounding effect tracks.
// Before this helper existed three components hand-rolled the loop and
// only one remembered the `document.hidden` gate.

export const POLL_MS = 5000;

export function startPoll(fn: () => unknown, ms = POLL_MS): () => void {
  void fn();
  const id = setInterval(() => {
    // jsdom defaults document.hidden to false, so unit tests that
    // advance fake timers see every tick.
    if (document.hidden) return;
    void fn();
  }, ms);
  return () => clearInterval(id);
}
