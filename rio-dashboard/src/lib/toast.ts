// Toast queue: a tiny writable store so any component can push a toast
// without prop-drilling. The consumer is the single <Toast/> portal
// mounted in App.svelte. No library — the whole thing is ~40 lines
// between this store and the component.
//
// svelte/store (not runes-in-module) because the queue is shared
// process-wide, read from exactly one portal, and a plain writable<T[]>
// is the smallest surface that does that.
import { writable } from 'svelte/store';

export type ToastKind = 'info' | 'error';

export type ToastMsg = {
  id: number;
  kind: ToastKind;
  text: string;
};

const { subscribe, update } = writable<ToastMsg[]>([]);
let seq = 0;

function push(kind: ToastKind, text: string, ttlMs = 4000): void {
  const id = ++seq;
  update((xs) => [...xs, { id, kind, text }]);
  // Auto-dismiss. If the component unmounts first the update is a no-op
  // (filters an id that's already gone).
  setTimeout(() => dismiss(id), ttlMs);
}

export function dismiss(id: number): void {
  update((xs) => xs.filter((t) => t.id !== id));
}

export const toasts = { subscribe };
export const toast = {
  info: (text: string) => push('info', text),
  error: (text: string) => push('error', text),
};
