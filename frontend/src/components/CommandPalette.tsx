"use client";

/**
 * CommandPalette — Sprint 5.7.31 universal launcher.
 *
 * Cmd+K (Mac) / Ctrl+K (Windows/Linux) opens a Spotlight-style
 * palette. The user types a fuzzy query; the action list filters
 * live. Up/Down/Enter select, Esc closes.
 *
 * UX details:
 *   - Centered modal (60ch wide) — feels like Spotlight, not a
 *     side drawer.
 *   - Backdrop blur — content underneath reads as "the world I
 *     just paused", not blocking dialog.
 *   - Keyboard-first — every action is reachable without a mouse.
 *   - Each action carries: icon, label, category badge, optional
 *     keyboard-shortcut hint.
 *   - Empty input shows recent/frequent actions; queries filter.
 *   - Sticky `>` prefix marks the keyboard shortcut hint slot.
 *
 * The action set is built per-render from the `useLocale()` and
 * `useTheme()` contexts, plus the `onNavigate` function the
 * caller supplies.
 */

import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import {
  Archive,
  FolderOpen,
  Send,
  Settings,
  History,
  Sun,
  Moon,
  Languages,
  Home,
  Keyboard,
  Sparkles,
  X,
} from "lucide-react";
import { useLocale } from "@/components/LocaleProvider";
import { useTheme } from "@/components/ThemeProvider";

export interface PaletteAction {
  id: string;
  label: string;
  hint?: string;
  icon: typeof Home;
  category: "navigate" | "theme" | "lang" | "help";
  run: () => void;
}

export interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  onNavigate: (v: "landing" | "compress" | "decompress" | "share" | "settings" | "recent") => void;
}

export function CommandPalette({ open, onClose, onNavigate }: CommandPaletteProps) {
  const { t, locale, setLocale } = useLocale();
  const { theme, toggle } = useTheme();
  const [query, setQuery] = useState("");
  const [highlightIndex, setHighlightIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);

  // Build actions dynamically because labels come from the locale.
  const actions: PaletteAction[] = useMemo(
    () => [
      {
        id: "nav.landing",
        label: t("nav.home"),
        hint: "go home",
        icon: Home,
        category: "navigate",
        run: () => {
          onNavigate("landing");
          onClose();
        },
      },
      {
        id: "nav.compress",
        label: t("nav.compress"),
        hint: "C",
        icon: Archive,
        category: "navigate",
        run: () => {
          onNavigate("compress");
          onClose();
        },
      },
      {
        id: "nav.decompress",
        label: t("nav.decompress"),
        hint: "D",
        icon: FolderOpen,
        category: "navigate",
        run: () => {
          onNavigate("decompress");
          onClose();
        },
      },
      {
        id: "nav.share",
        label: t("nav.share"),
        hint: "S",
        icon: Send,
        category: "navigate",
        run: () => {
          onNavigate("share");
          onClose();
        },
      },
      {
        id: "nav.recent",
        label: t("nav.recent"),
        hint: "R",
        icon: History,
        category: "navigate",
        run: () => {
          onNavigate("recent");
          onClose();
        },
      },
      {
        id: "nav.settings",
        label: t("nav.settings"),
        hint: ",",
        icon: Settings,
        category: "navigate",
        run: () => {
          onNavigate("settings");
          onClose();
        },
      },
      {
        id: "theme.toggle",
        label:
          theme === "dark"
            ? t("palette.theme.to_light")
            : t("palette.theme.to_dark"),
        hint: "T",
        icon: theme === "dark" ? Sun : Moon,
        category: "theme",
        run: () => {
          toggle();
          onClose();
        },
      },
      {
        id: "lang.es",
        label: "Español",
        icon: Languages,
        category: "lang",
        run: () => {
          setLocale("es");
          onClose();
        },
      },
      {
        id: "lang.en",
        label: "English",
        icon: Languages,
        category: "lang",
        run: () => {
          setLocale("en");
          onClose();
        },
      },
      {
        id: "lang.it",
        label: "Italiano",
        icon: Languages,
        category: "lang",
        run: () => {
          setLocale("it");
          onClose();
        },
      },
      {
        id: "help.shortcuts",
        label: t("palette.help.shortcuts"),
        icon: Keyboard,
        category: "help",
        run: () => {
          onClose();
          // dispatch a custom event the help dialog listens for
          window.dispatchEvent(new CustomEvent("nexus:show-shortcuts"));
        },
      },
      {
        id: "help.welcome",
        label: t("palette.help.welcome"),
        icon: Sparkles,
        category: "help",
        run: () => {
          // reset the first-run flag so the wizard re-appears
          try {
            window.localStorage.removeItem("nexus.firstRun.done");
          } catch {}
          onClose();
          window.dispatchEvent(new CustomEvent("nexus:show-welcome"));
        },
      },
    ],
    [t, theme, toggle, setLocale, onNavigate, onClose]
  );

  // Filter by query (case-insensitive substring match).
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return actions;
    return actions.filter((a) => a.label.toLowerCase().includes(q));
  }, [actions, query]);

  // Reset highlight when filtered list changes.
  useEffect(() => {
    setHighlightIndex(0);
  }, [query]);

  // Focus input on open; clear query on close.
  useEffect(() => {
    if (open) {
      setQuery("");
      setHighlightIndex(0);
      // RAF because the input is rendered conditionally.
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  // Keyboard navigation.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      } else if (e.key === "ArrowDown") {
        e.preventDefault();
        setHighlightIndex((i) => Math.min(i + 1, filtered.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setHighlightIndex((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter") {
        e.preventDefault();
        const a = filtered[highlightIndex];
        if (a) a.run();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, filtered, highlightIndex, onClose]);

  if (!open) return null;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={t("palette.title")}
      className="fixed inset-0 z-[110] flex items-start justify-center pt-[14vh] animate-fade-in"
    >
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/55"
        style={{ backdropFilter: "blur(8px)", WebkitBackdropFilter: "blur(8px)" }}
        onClick={onClose}
      />
      {/* The palette itself */}
      <div
        className="relative w-[640px] max-w-[92vw] rounded-2xl overflow-hidden border border-white/[0.10] shadow-2xl"
        style={{
          background: "rgba(15, 15, 18, 0.94)",
          backdropFilter: "blur(20px) saturate(160%)",
          WebkitBackdropFilter: "blur(20px) saturate(160%)",
          boxShadow:
            "0 24px 80px -12px rgba(0,0,0,0.7), 0 0 0 1px rgba(255,255,255,0.04) inset",
        }}
      >
        {/* Search input */}
        <div className="flex items-center gap-3 px-5 border-b border-white/[0.06]">
          <Sparkles size={16} className="text-cyan-400 shrink-0" />
          <input
            ref={inputRef}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("palette.placeholder")}
            className="flex-1 bg-transparent py-4 text-[14.5px] text-white placeholder:text-zinc-500 outline-none"
          />
          <kbd className="px-1.5 py-0.5 rounded-md border border-white/[0.10] text-zinc-500 text-[10.5px] font-mono">
            Esc
          </kbd>
          <button
            onClick={onClose}
            className="text-zinc-500 hover:text-white p-1 rounded-md hover:bg-white/[0.05] transition-colors"
            aria-label="Close"
          >
            <X size={14} />
          </button>
        </div>
        {/* Results */}
        <div className="max-h-[60vh] overflow-y-auto p-2">
          {filtered.length === 0 ? (
            <div className="px-4 py-10 text-center text-zinc-500 text-[13px]">
              {t("palette.empty")}
            </div>
          ) : (
            <ul role="listbox">
              {filtered.map((a, i) => {
                const Icon = a.icon;
                const active = i === highlightIndex;
                return (
                  <li key={a.id}>
                    <button
                      role="option"
                      aria-selected={active}
                      onMouseEnter={() => setHighlightIndex(i)}
                      onClick={() => a.run()}
                      className={
                        "w-full flex items-center gap-3 px-3 py-2 rounded-lg text-left transition-colors " +
                        (active
                          ? "bg-white/[0.07] text-white"
                          : "text-zinc-300 hover:bg-white/[0.03]")
                      }
                    >
                      <Icon
                        size={15}
                        className={
                          active
                            ? "text-cyan-300"
                            : "text-zinc-500"
                        }
                      />
                      <span className="flex-1 text-[13px]">{a.label}</span>
                      <span className="text-[10.5px] uppercase tracking-wider text-zinc-500 px-1.5 py-0.5 rounded border border-white/[0.06]">
                        {a.category}
                      </span>
                      {a.hint ? (
                        <kbd className="px-1.5 py-0.5 rounded-md border border-white/[0.10] text-zinc-500 text-[10.5px] font-mono">
                          {a.hint}
                        </kbd>
                      ) : null}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
        {/* Footer hint */}
        <div className="px-4 py-2 border-t border-white/[0.06] text-zinc-500 text-[10.5px] flex items-center justify-between">
          <span>{t("palette.footer.count", { n: filtered.length })}</span>
          <span className="flex items-center gap-2">
            <kbd className="px-1.5 py-0.5 rounded-md border border-white/[0.10] font-mono">↑↓</kbd>
            <span>{t("palette.navigation")}</span>
            <span className="mx-1">·</span>
            <kbd className="px-1.5 py-0.5 rounded-md border border-white/[0.10] font-mono">↵</kbd>
            <span>{t("palette.open")}</span>
          </span>
        </div>
      </div>
    </div>
  );
}

/**
 * Hook that binds the Cmd/Ctrl+K shortcut globally. Mount this
 * from the page root so it fires wherever the user is.
 */
export function useCommandPaletteHotkey(onOpen: () => void) {
  const cb = useCallback(
    (e: KeyboardEvent) => {
      const isMac =
        typeof navigator !== "undefined" &&
        /Mac|iPhone|iPad/.test(navigator.platform);
      const trigger = isMac ? e.metaKey : e.ctrlKey;
      if (trigger && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        onOpen();
      }
    },
    [onOpen]
  );
  useEffect(() => {
    window.addEventListener("keydown", cb);
    return () => window.removeEventListener("keydown", cb);
  }, [cb]);
}

// Re-export locale strings needed by the palette so i18n stays
// in one place. Caller of the palette imports these keys into the
// `useLocale().t(...)` map.
export const PALETTE_I18N_KEYS = [
  "palette.title",
  "palette.placeholder",
  "palette.empty",
  "palette.footer.count",
  "palette.navigation",
  "palette.open",
  "palette.theme.to_light",
  "palette.theme.to_dark",
  "palette.help.shortcuts",
  "palette.help.welcome",
  "nav.compress",
  "nav.decompress",
  "nav.recent",
  "nav.settings",
  "nav.home",
  "nav.share",
] as const;