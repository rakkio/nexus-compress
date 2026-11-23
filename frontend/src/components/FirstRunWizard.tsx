"use client";

/**
 * FirstRunWizard â€” Sprint 5.7.31 onboarding.
 *
 * Shows a 3-step modal on the very first launch (detected via
 * `localStorage.nexus.firstRun.done`). Steps:
 *   1. Language
 *   2. Theme
 *   3. Default output directory
 * Then dismisses and sets the done flag.
 *
 * Designed for first impressions, not for re-use â€” once dismissed
 * it never appears again unless the user explicitly triggers it
 * from the command palette ("Show welcome").
 *
 * Sprint 5.7.31 keyboard nav: â†/â†’ to go back/forward, Enter to
 * advance, Esc to dismiss (writes the done flag either way so it
 * doesn't nag on subsequent launches).
 */

import { useEffect, useMemo, useState, useCallback } from "react";
import {
  Check,
  Sun,
  Moon,
  FolderOpen,
  Sparkles,
  ChevronLeft,
  ChevronRight,
  X,
  Globe,
} from "lucide-react";
import { useLocale } from "@/components/LocaleProvider";
import type { Locale, TranslationKey } from "@/lib/i18n";
import { useTheme } from "@/components/ThemeProvider";
import { useLocalStorage } from "@/lib/useLocalStorage";

const STORAGE_DONE = "nexus.firstRun.done";
const STORAGE_VERSION = "nexus.firstRun.version";
const WIZARD_VERSION = 1;

interface FirstRunWizardProps {
  /** Caller decides when to render the wizard (driven by localStorage
   *  check on mount + the custom `nexus:show-welcome` event). */
  open: boolean;
  onClose: () => void;
}

const LANGS: { id: Locale; label: string; sub: string }[] = [
  { id: "es", label: "EspaÃ±ol", sub: "Idioma por defecto" },
  { id: "en", label: "English", sub: "Default language" },
  { id: "it", label: "Italiano", sub: "Lingua predefinita" },
];

export function FirstRunWizard({ open, onClose }: FirstRunWizardProps) {
  const { t, locale, setLocale } = useLocale();
  const { theme, setTheme } = useTheme();
  const [destDir, setDestDir] = useLocalStorage<string>("nexus.compress.destDir", "");
  const [step, setStep] = useState<0 | 1 | 2>(0);
  const isTauri =
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

  // Reset step on open.
  useEffect(() => {
    if (open) setStep(0);
  }, [open]);

  const finish = useCallback(() => {
    try {
      window.localStorage.setItem(STORAGE_DONE, "1");
      window.localStorage.setItem(STORAGE_VERSION, String(WIZARD_VERSION));
    } catch {}
    onClose();
  }, [onClose]);

  // Keyboard nav.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        finish();
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (step < 2) setStep((s) => ((s + 1) as 0 | 1 | 2));
        else finish();
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        if (step > 0) setStep((s) => ((s - 1) as 0 | 1 | 2));
      } else if (e.key === "ArrowRight" && step < 2) {
        e.preventDefault();
        setStep((s) => ((s + 1) as 0 | 1 | 2));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, step, finish]);

  if (!open) return null;

  const pickDir = async () => {
    if (!isTauri) return;
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const r = await open({ multiple: false, directory: true });
      if (typeof r === "string") setDestDir(r);
    } catch {}
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={t("welcome.title")}
      className="fixed inset-0 z-[105] flex items-center justify-center animate-fade-in"
    >
      {/* Backdrop with strong blur to focus the modal. */}
      <div
        className="absolute inset-0 bg-black/60"
        style={{ backdropFilter: "blur(10px)", WebkitBackdropFilter: "blur(10px)" }}
        onClick={finish}
      />
      {/* Modal */}
      <div
        className="relative w-[640px] max-w-[92vw] rounded-3xl overflow-hidden border border-white/[0.10] shadow-2xl"
        style={{
          background:
            "linear-gradient(180deg, rgba(20,20,24,0.97) 0%, rgba(10,10,12,0.97) 100%)",
          boxShadow:
            "0 30px 100px -16px rgba(0,0,0,0.7), 0 0 0 1px rgba(255,255,255,0.05) inset",
        }}
      >
        {/* Top bar: progress dots + close */}
        <div className="flex items-center justify-between px-6 pt-5 pb-3">
          <div className="flex items-center gap-2">
            <Sparkles size={14} className="text-cyan-400" />
            <span className="text-[11px] tracking-[0.2em] uppercase text-zinc-500 font-semibold">
              {t("welcome.label")}
            </span>
          </div>
          <button
            onClick={finish}
            className="text-zinc-500 hover:text-white p-1.5 rounded-md hover:bg-white/[0.05] transition-colors"
            aria-label="Skip"
            title="Skip"
          >
            <X size={14} />
          </button>
        </div>
        <div className="flex items-center justify-center gap-1.5 pb-5">
          {[0, 1, 2].map((i) => (
            <span
              key={i}
              className={
                "h-1 rounded-full transition-all duration-300 " +
                (i === step
                  ? "w-8 bg-cyan-400"
                  : i < step
                    ? "w-4 bg-cyan-400/50"
                    : "w-4 bg-white/[0.10]")
              }
            />
          ))}
        </div>
        {/* Step content */}
        <div className="px-7 pb-7 min-h-[280px]">
          {step === 0 ? (
            <Step1Language
              current={locale}
              onPick={(id) => setLocale(id)}
              t={t}
            />
          ) : step === 1 ? (
            <Step2Theme current={theme} onPick={(tt) => setTheme(tt)} t={t} />
          ) : (
            <Step3Dest
              current={destDir}
              onPick={pickDir}
              onChange={(v) => setDestDir(v)}
              t={t}
            />
          )}
        </div>
        {/* Footer nav */}
        <div className="flex items-center justify-between px-7 py-4 border-t border-white/[0.06] bg-white/[0.02]">
          <button
            onClick={() => setStep(0)}
            disabled={step === 0}
            className="flex items-center gap-1 text-[12px] text-zinc-400 hover:text-white transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
          >
            <ChevronLeft size={14} />
            {t("welcome.back")}
          </button>
          <div className="text-[11px] text-zinc-500 tabular-nums">
            {step + 1} / 3
          </div>
          <button
            onClick={() => (step < 2 ? setStep((s) => ((s + 1) as 0 | 1 | 2)) : finish())}
            className="flex items-center gap-1.5 text-[12.5px] font-medium px-3.5 py-2 rounded-lg bg-cyan-400 hover:bg-cyan-300 text-cyan-950 transition-colors"
          >
            {step < 2 ? t("welcome.next") : t("welcome.done")}
            {step < 2 ? <ChevronRight size={14} /> : <Check size={14} />}
          </button>
        </div>
      </div>
    </div>
  );
}

// â”€â”€ Steps â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

function Step1Language({
  current,
  onPick,
  t,
}: {
  current: Locale;
  onPick: (id: Locale) => void;
  t: (k: TranslationKey, vars?: Record<string, string | number>) => string;
}) {
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-col items-center gap-3">
        <div className="w-14 h-14 rounded-2xl bg-cyan-500/15 border border-cyan-500/30 flex items-center justify-center text-cyan-300">
          <Globe size={26} />
        </div>
        <h2 className="text-white text-[22px] font-semibold tracking-tight text-center">
          {t("welcome.lang.title")}
        </h2>
        <p className="text-zinc-500 text-[13px] text-center max-w-md">
          {t("welcome.lang.desc")}
        </p>
      </div>
      <div className="grid grid-cols-1 gap-2">
        {LANGS.map((l) => {
          const active = current === l.id;
          return (
            <button
              key={l.id}
              onClick={() => onPick(l.id)}
              className={
                "flex items-center gap-3 px-4 py-3 rounded-xl border text-left transition-all " +
                (active
                  ? "border-cyan-500/50 bg-cyan-500/[0.06] text-white"
                  : "border-white/[0.08] bg-white/[0.02] hover:border-white/[0.16] hover:bg-white/[0.04] text-zinc-200")
              }
            >
              <span
                className={
                  "w-5 h-5 rounded-full border-2 flex items-center justify-center shrink-0 " +
                  (active ? "border-cyan-400" : "border-white/[0.16]")
                }
              >
                {active && <span className="w-2.5 h-2.5 rounded-full bg-cyan-400" />}
              </span>
              <div className="flex-1 min-w-0">
                <p className="text-[14px] font-medium">{l.label}</p>
                <p className="text-zinc-500 text-[11.5px]">{l.sub}</p>
              </div>
              {active && <Check size={16} className="text-cyan-400 shrink-0" />}
            </button>
          );
        })}
      </div>
    </div>
  );
}

function Step2Theme({
  current,
  onPick,
  t,
}: {
  current: "dark" | "light";
  onPick: (tt: "dark" | "light") => void;
  t: (k: TranslationKey, vars?: Record<string, string | number>) => string;
}) {
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-col items-center gap-3">
        <div
          className={
            "w-14 h-14 rounded-2xl border flex items-center justify-center " +
            (current === "dark"
              ? "bg-amber-500/15 border-amber-500/30 text-amber-300"
              : "bg-cyan-500/15 border-cyan-500/30 text-cyan-300")
          }
        >
          {current === "dark" ? <Moon size={26} /> : <Sun size={26} />}
        </div>
        <h2 className="text-white text-[22px] font-semibold tracking-tight text-center">
          {t("welcome.theme.title")}
        </h2>
        <p className="text-zinc-500 text-[13px] text-center max-w-md">
          {t("welcome.theme.desc")}
        </p>
      </div>
      <div className="grid grid-cols-2 gap-3">
        <ThemeCard
          tone="dark"
          active={current === "dark"}
          onClick={() => onPick("dark")}
        />
        <ThemeCard
          tone="light"
          active={current === "light"}
          onClick={() => onPick("light")}
        />
      </div>
    </div>
  );
}

function ThemeCard({
  tone,
  active,
  onClick,
}: {
  tone: "dark" | "light";
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className={
        "flex flex-col gap-3 p-3 rounded-2xl border text-left transition-all " +
        (active
          ? "border-cyan-500/50 bg-cyan-500/[0.06]"
          : "border-white/[0.08] bg-white/[0.02] hover:border-white/[0.16]")
      }
    >
      <div
        className={
          "rounded-xl h-24 border overflow-hidden relative " +
          (tone === "dark"
            ? "bg-gradient-to-br from-[#16161a] to-[#0a0a0c] border-white/[0.06]"
            : "bg-gradient-to-br from-white to-[#f1f3f6] border-black/10")
        }
      >
        <div className="absolute inset-3 flex flex-col gap-1.5">
          <div
            className={
              "h-2 rounded-full w-3/4 " +
              (tone === "dark" ? "bg-cyan-400/60" : "bg-cyan-600/60")
            }
          />
          <div
            className={
              "h-2 rounded-full w-1/2 " +
              (tone === "dark" ? "bg-zinc-600/60" : "bg-slate-300/80")
            }
          />
          <div
            className={
              "h-2 rounded-full w-2/3 " +
              (tone === "dark" ? "bg-zinc-700/60" : "bg-slate-200")
            }
          />
        </div>
        {tone === "dark" ? (
          <Moon size={12} className="absolute top-2 right-2 text-amber-400/70" />
        ) : (
          <Sun size={12} className="absolute top-2 right-2 text-cyan-500/80" />
        )}
      </div>
      <div className="flex items-center gap-2">
        <span
          className={
            "w-4 h-4 rounded-full border-2 flex items-center justify-center " +
            (active ? "border-cyan-400" : "border-white/[0.16]")
          }
        >
          {active && <span className="w-2 h-2 rounded-full bg-cyan-400" />}
        </span>
        <span className="text-white text-[13px] font-medium">
          {tone === "dark" ? "Oscuro" : "Claro"}
        </span>
      </div>
    </button>
  );
}

function Step3Dest({
  current,
  onPick,
  onChange,
  t,
}: {
  current: string;
  onPick: () => void;
  onChange: (v: string) => void;
  t: (k: TranslationKey, vars?: Record<string, string | number>) => string;
}) {
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-col items-center gap-3">
        <div className="w-14 h-14 rounded-2xl bg-emerald-500/15 border border-emerald-500/30 flex items-center justify-center text-emerald-300">
          <FolderOpen size={26} />
        </div>
        <h2 className="text-white text-[22px] font-semibold tracking-tight text-center">
          {t("welcome.dest.title")}
        </h2>
        <p className="text-zinc-500 text-[13px] text-center max-w-md">
          {t("welcome.dest.desc")}
        </p>
      </div>
      <div className="flex items-center gap-2">
        <input
          value={current}
          onChange={(e) => onChange(e.target.value)}
          placeholder={t("welcome.dest.placeholder")}
          className="flex-1 bg-white/[0.04] border border-white/[0.08] rounded-xl px-4 py-2.5 text-[13px] font-mono text-white placeholder:text-zinc-500 outline-none focus:border-cyan-500/50"
        />
        <button
          onClick={onPick}
          className="px-4 py-2.5 rounded-xl bg-white/[0.06] hover:bg-white/[0.10] text-white text-[13px] font-medium border border-white/[0.08] inline-flex items-center gap-2 transition-colors"
        >
          <FolderOpen size={14} />
          {t("welcome.dest.browse")}
        </button>
      </div>
      <p className="text-[11.5px] text-zinc-500 text-center -mt-2">
        {t("welcome.dest.hint")}
      </p>
    </div>
  );
}

/**
 * Helper that decides whether the wizard should auto-show on mount.
 * Returns true if first-run is incomplete (no `done` flag, or
 * flag version < WIZARD_VERSION).
 */
export function shouldShowFirstRun(): boolean {
  if (typeof window === "undefined") return false;
  try {
    const done = window.localStorage.getItem(STORAGE_DONE);
    const version = Number(window.localStorage.getItem(STORAGE_VERSION) ?? "0");
    if (!done) return true;
    if (version < WIZARD_VERSION) return true;
    return false;
  } catch {
    return false;
  }
}