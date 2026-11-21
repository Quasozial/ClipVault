import { useSyncExternalStore } from "react";

export interface Toast {
  id: number;
  kind: "error" | "info";
  text: string;
}

let toasts: Toast[] = [];
let nextId = 1;
const subs = new Set<() => void>();
const emit = () => subs.forEach((s) => s());

export function dismissToast(id: number) {
  toasts = toasts.filter((t) => t.id !== id);
  emit();
}

function push(kind: Toast["kind"], text: string) {
  const id = nextId++;
  toasts = [...toasts.slice(-3), { id, kind, text }];
  emit();
  // Errors stay until dismissed or replaced; info fades after a few seconds.
  if (kind === "info") setTimeout(() => dismissToast(id), 4000);
}

export const toast = {
  info: (text: string) => push("info", text),
  error: (text: string) => push("error", text),
};

/** Shows a failed action as "<what>: <backend message>" and logs it. */
export function reportError(what: string, e: unknown) {
  const msg = e instanceof Error ? e.message : String(e);
  console.error(what, e);
  toast.error(`${what}: ${msg}`);
}

export function useToasts(): Toast[] {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => toasts,
  );
}
