/**
 * useLocalStorage — Sprint 5.7.30
 *
 * Lightweight SSR-safe `useState` wrapper that mirrors the value
 * into `window.localStorage` under a stable key. Used by
 * `page.tsx` to persist the recent-ops history across reloads.
 *
 * Design notes:
 *   - Hydration is lazy on first render (after mount), so server-
 *     rendered output always matches the SSR fallback (the
 *     `initialValue`). This keeps Next.js' static export from
 *   warning on a date-driven flicker.
 *   - Writes are synchronous to localStorage but we wrap them in
 *     a try/catch — Safari (private mode) and WebView2 (sandboxed)
 *     throw on `setItem` if storage is full or denied.
 *   - We emit `storage` events from other tabs to keep multiple
 *     windows of the app in sync (e.g. user reopens the app
 *     while it's still running).
 *   - The hook accepts a generic so it can be reused for any
 *     JSON-serialisable state (theme, locale, settings drafts…).
 */

import { useCallback, useEffect, useRef, useState } from "react";

type Setter<T> = (value: T | ((prev: T) => T)) => void;

export function useLocalStorage<T>(
  key: string,
  initialValue: T
): [T, Setter<T>] {
  // Always start with `initialValue` so SSR + first client render
  // match. Hydration happens in the effect below.
  const [value, setValue] = useState<T>(initialValue);
  const hydrated = useRef(false);

  // Hydrate from storage on mount.
  useEffect(() => {
    if (typeof window === "undefined") return;
    try {
      const raw = window.localStorage.getItem(key);
      if (raw !== null) {
        setValue(JSON.parse(raw) as T);
      }
    } catch (err) {
      // Corrupt JSON or storage access denied — keep initial value
      // and surface a console warning so we can spot it in dev.
      console.warn(`[useLocalStorage] failed to read ${key}:`, err);
    } finally {
      hydrated.current = true;
    }
    // We intentionally only run this once on mount; `key` is
    // expected to be a stable literal at every call site.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Persist on every change (after hydration, so we don't
  // immediately overwrite stored data with `initialValue`).
  useEffect(() => {
    if (typeof window === "undefined") return;
    if (!hydrated.current) return;
    try {
      window.localStorage.setItem(key, JSON.stringify(value));
    } catch (err) {
      console.warn(`[useLocalStorage] failed to write ${key}:`, err);
    }
  }, [key, value]);

  // Cross-tab sync: if another tab updates the same key, mirror it.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const onStorage = (e: StorageEvent) => {
      if (e.key !== key || e.newValue === null) return;
      try {
        setValue(JSON.parse(e.newValue) as T);
      } catch {
        /* ignore corrupt payload from other tab */
      }
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, [key]);

  // Wrap setValue to also support functional updates (matching
  // the signature of React's useState setter).
  const setValueAndPersist: Setter<T> = useCallback((next) => {
    setValue((prev) =>
      typeof next === "function"
        ? (next as (p: T) => T)(prev)
        : next
    );
  }, []);

  return [value, setValueAndPersist];
}