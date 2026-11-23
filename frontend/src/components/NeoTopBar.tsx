"use client";

import { useLocale } from "@/components/LocaleProvider";
import { useTheme } from "@/components/ThemeProvider";
import {
  Home,
  History,
  Share2,
  Settings as SettingsIcon,
  Sun,
  Moon,
} from "lucide-react";

/**
 * Brand mark used in the top bar. Matches the desktop app icon
 * (src-tauri/icons/icon.png) — three "compressed data" bars on
 * a cyan→blue gradient rounded square with a small chevron-down
 * accent. Sprint 5.7.31.
 */
function BrandMark() {
  return (
    <svg viewBox="0 0 32 32" className="w-8 h-8 drop-shadow-[0_0_14px_-2px_rgba(34,211,238,0.55)]">
      <defs>
        <linearGradient id="nx-bg" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0%" stopColor="#0891b2" />
          <stop offset="100%" stopColor="#1d4ed8" />
        </linearGradient>
      </defs>
      <rect
        x="1"
        y="1"
        width="30"
        height="30"
        rx="6"
        fill="url(#nx-bg)"
        stroke="rgba(255,255,255,0.18)"
        strokeWidth="0.5"
      />
      {/* Chevron (compression indicator) */}
      <path
        d="M22 7 L19 10 L25 10 Z"
        fill="rgba(255,255,255,0.85)"
      />
      {/* 3 compressed bars */}
      <rect x="9"  y="11" width="14" height="2.2" rx="1.1" fill="#ecfeff" />
      <rect x="9"  y="14.5" width="14" height="2.2" rx="1.1" fill="#ecfeff" />
      <rect x="9"  y="18" width="14" height="2.2" rx="1.1" fill="#ecfeff" />
      {/* subtle accent line */}
      <rect x="9" y="8" width="14" height="0.7" rx="0.35" fill="rgba(34,211,238,0.7)" />
      {/* bottom baseline */}
      <rect x="9" y="22" width="14" height="0.7" rx="0.35" fill="rgba(34,211,238,0.45)" />
    </svg>
  );
}

export type View =
  | "landing"
  | "compress"
  | "decompress"
  | "share"
  | "settings"
  | "recent";

export function NeoTopBar({
  view,
  onNavigate,
  onOpenPalette,
}: {
  view: View;
  onNavigate: (v: View) => void;
  onOpenPalette?: () => void;
}) {
  const { t } = useLocale();
  const { theme, toggle } = useTheme();

  const navItems: { id: View; label: string; Icon: typeof Home }[] = [
    { id: "landing",  label: t("nav.home"),     Icon: Home },
    { id: "recent",   label: t("nav.recent"),   Icon: History },
    { id: "share",    label: t("nav.share"),    Icon: Share2 },
    { id: "settings", label: t("nav.settings"), Icon: SettingsIcon },
  ];

  return (
    <header
      data-tauri-drag-region
      className="h-14 flex items-center justify-between px-8 shrink-0 select-none border-b border-white/[0.06] glass-strong"
    >
      {/* Brand */}
      <div className="flex items-center gap-2.5 w-[120px]">
        <div className="transition-transform duration-300 hover:scale-105 hover:rotate-3">
          <BrandMark />
        </div>
        <span className="text-white font-semibold text-[14px] tracking-tight">
          NexusRAR
        </span>
      </div>

      {/* Nav */}
      <nav className="flex items-center gap-1">
        {navItems.map((item) => {
          const active = view === item.id;
          const Icon = item.Icon;
          return (
            <button
              key={item.id}
              onClick={() => onNavigate(item.id)}
              className={
                "group relative flex items-center gap-1.5 px-3.5 py-1.5 text-[12.5px] font-medium rounded-lg transition-all duration-200 " +
                (active
                  ? "text-white bg-white/[0.08] shadow-[inset_0_0_0_1px_rgba(255,255,255,0.08)]"
                  : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.04] hover:-translate-y-px")
              }
            >
              <Icon
                size={14}
                strokeWidth={active ? 2.4 : 2}
                className={"transition-transform duration-200 " + (active ? "" : "group-hover:scale-110")}
              />
              {item.label}
              {/* Active underline indicator — animates in/out */}
              {active && (
                <span className="absolute -bottom-px left-2 right-2 h-px bg-gradient-to-r from-transparent via-cyan-400 to-transparent animate-fade-in" />
              )}
            </button>
          );
        })}
      </nav>

      {/* Version + theme toggle */}
      <div className="w-[120px] flex justify-end items-center gap-2">
        <span className="text-zinc-500 text-[11px] font-mono tracking-wide tabular-nums px-2 py-0.5 rounded-md border border-white/[0.06]">
          v0.1.2
        </span>
        <button
          onClick={onOpenPalette}
          title={t("palette.open_hint")}
          aria-label={t("palette.open_hint")}
          className="w-8 h-8 rounded-lg flex items-center justify-center text-zinc-400 hover:text-cyan-400 hover:bg-white/[0.05] border border-white/[0.06] hover:border-cyan-500/30 transition-all duration-200"
        >
          <kbd className="text-[10px] font-mono tracking-tighter">⌘K</kbd>
        </button>
        <button
          onClick={toggle}
          className="w-8 h-8 rounded-lg flex items-center justify-center text-zinc-400 hover:text-amber-400 hover:bg-white/[0.05] border border-white/[0.06] hover:border-amber-500/30 transition-all duration-200"
          aria-label={theme === "dark" ? "Switch to light" : "Switch to dark"}
          title={t("palette.theme.hint")}
        >
          {theme === "dark" ? <Sun size={14} /> : <Moon size={14} />}
        </button>
      </div>
    </header>
  );
}
