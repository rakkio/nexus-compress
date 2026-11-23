"use client";

/**
 * Toaster — Sprint 5.7.32 lightweight toast notification system.
 *
 * A single toaster mounted in `page.tsx` shows ephemeral messages
 * (success / error / info) in the top-right corner. Auto-dismiss
 * after 4 seconds; the user can dismiss early by clicking the X.
 *
 * We deliberately use a global imperative API (`toast.success(...)`)
 * instead of putting toast state into every view, so components
 * don't need to re-render just to surface a message. The singleton
 * is registered as `window.__toast` on mount.
 *
 * UX details:
 *   - Slides in from the right + fades opacity (`animate-fade-in`).
 *   - Each toast has its own timeout so they don't clear together.
 *   - The progress component in CompressView is *separate* from
 *     this toaster — we don't want a 4-second auto-dismiss for the
 *     progress bar; that's a different visual primitive.
 */

import { useEffect, useState, useCallback } from "react";
import { Check, AlertCircle, Info, X } from "lucide-react";

type ToastKind = "ok" | "err" | "info";

interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
  /** Override default timeout in ms. 0 = sticky (user must close). */
  ttl?: number;
}

let nextId = 1;
const subscribers = new Set<(t: Toast) => void>();

/** Imperative API — call from anywhere. */
export const toast = {
  success(message: string, ttl = 4000) {
    emit({ id: nextId++, kind: "ok", message, ttl });
  },
  error(message: string, ttl = 6000) {
    emit({ id: nextId++, kind: "err", message, ttl });
  },
  info(message: string, ttl = 4000) {
    emit({ id: nextId++, kind: "info", message, ttl });
  },
};

function emit(t: Toast) {
  subscribers.forEach((cb) => cb(t));
}

/** Mounts once near the app root (see page.tsx). */
export function Toaster() {
  const [items, setItems] = useState<Toast[]>([]);

  // Listener: append to local state, then schedule auto-dismiss.
  const onPush = useCallback((t: Toast) => {
    setItems((prev) => [...prev, t]);
    if (t.ttl !== 0) {
      window.setTimeout(() => {
        setItems((prev) => prev.filter((x) => x.id !== t.id));
      }, t.ttl ?? 4000);
    }
  }, []);

  useEffect(() => {
    subscribers.add(onPush);
    // Expose imperative API globally for non-React callers (rare;
    // only used by event listeners inside Tauri).
    (window as any).__toast = toast;
    return () => {
      subscribers.delete(onPush);
      delete (window as any).__toast;
    };
  }, [onPush]);

  const dismiss = (id: number) =>
    setItems((prev) => prev.filter((x) => x.id !== id));

  return (
    <div
      aria-live="polite"
      aria-atomic="true"
      className="fixed top-3 right-3 z-[120] flex flex-col gap-2 pointer-events-none"
    >
      {items.map((t) => {
        const tone = TONE[t.kind];
        const Icon = ICON[t.kind];
        return (
          <div
            key={t.id}
            role="status"
            className={
              "pointer-events-auto min-w-[260px] max-w-[420px] rounded-xl border backdrop-blur " +
              "shadow-2xl px-3.5 py-2.5 flex items-start gap-2.5 animate-scale-in " +
              tone.box
            }
          >
            <Icon size={15} className={"shrink-0 mt-0.5 " + tone.icon} />
            <p className="text-[12.5px] flex-1 leading-tight">{t.message}</p>
            <button
              onClick={() => dismiss(t.id)}
              className="text-zinc-500 hover:text-white transition-colors p-0.5 rounded"
              aria-label="Dismiss"
            >
              <X size={12} />
            </button>
          </div>
        );
      })}
    </div>
  );
}

const TONE: Record<ToastKind, { box: string; icon: string }> = {
  ok: {
    box: "bg-emerald-500/[0.12] border-emerald-500/40 text-emerald-100",
    icon: "text-emerald-300",
  },
  err: {
    box: "bg-rose-500/[0.12] border-rose-500/40 text-rose-100",
    icon: "text-rose-300",
  },
  info: {
    box: "bg-cyan-500/[0.12] border-cyan-500/40 text-cyan-100",
    icon: "text-cyan-300",
  },
};

const ICON = { ok: Check, err: AlertCircle, info: Info };