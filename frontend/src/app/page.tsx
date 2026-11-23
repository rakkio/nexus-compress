"use client";

/**
 * Home — Sprint 5.6.1 "Neo Terminal" + recientes persistence +
 *         Sprint 5.7.31 global UX layer (DropOverlay + CommandPalette
 *         + FirstRunWizard).
 *
 * The page itself stays a thin router: it owns the current view
 * state, the recentOps log, and dispatches user gestures to the
 * right component. Global UX layers (drop overlay, command
 * palette, first-run wizard) are mounted unconditionally so they
 * remain reachable from anywhere in the app.
 */

import { useState, useCallback, useEffect } from "react";
import { NeoTopBar, type View } from "@/components/NeoTopBar";
import { LandingPage } from "@/components/LandingPage";
import { CompressView } from "@/components/CompressView";
import { DecompressView } from "@/components/DecompressView";
import { ShareView } from "@/components/ShareView";
import { NeoDashboard, type LastOp } from "@/components/NeoDashboard";
import { RecentView, type RecentOp } from "@/components/RecentView";
import { SettingsView } from "@/components/SettingsView";
import { useLocalStorage } from "@/lib/useLocalStorage";
import { formatBytes } from "@/lib/format";
import { DropOverlay } from "@/components/DropOverlay";
import {
  CommandPalette,
  useCommandPaletteHotkey,
} from "@/components/CommandPalette";
import { FirstRunWizard, shouldShowFirstRun } from "@/components/FirstRunWizard";
import { Toaster, toast } from "@/components/Toaster";

export default function Home() {
  const [view, setView] = useState<View>("landing");
  const [lastOp, setLastOp] = useState<LastOp | null>(null);
  // Sprint 5.7.30: recentOps now persists to localStorage so the
  // history survives reloads (previously lived in useState only).
  const [recentOps, setRecentOps] = useLocalStorage<RecentOp[]>(
    "nexus.recentOps.v1",
    []
  );

  // Sprint 5.7.31 — pending drop from DropOverlay. When a file is
  // dropped anywhere in the app we route to the right view and
  // pre-populate the file picker. The view reads this on mount
  // (see CompressView / DecompressView `pendingDropPath` prop).
  const [pendingDrop, setPendingDrop] = useState<
    { path: string; action: "compress" | "decompress" } | null
  >(null);
  const handleDropPath = useCallback(
    (path: string, action: "compress" | "archive") => {
      // DropOverlay returns "compress" | "archive"; "archive" routes
      // to DecompressView, anything else to CompressView.
      const viewAction = action === "archive" ? "decompress" : "compress";
      setPendingDrop({ path, action: viewAction });
      setView(viewAction);
      // Quick feedback so the user knows the drop registered.
      const filename = path.split(/[\\/]/).pop() || path;
      toast.info(
        viewAction === "decompress"
          ? `Extrayendo "${filename}"…`
          : `Comprimiendo "${filename}"…`
      );
    },
    []
  );

  // Cmd/Ctrl+K command palette.
  const [paletteOpen, setPaletteOpen] = useState(false);
  useCommandPaletteHotkey(() => setPaletteOpen(true));

  // First-run wizard (Sprint 5.7.31). Auto-opens if the user hasn't
  // completed the wizard yet; re-triggerable via the command palette
  // ("Show welcome") which dispatches a custom event.
  const [wizardOpen, setWizardOpen] = useState(false);
  useEffect(() => {
    if (shouldShowFirstRun()) setWizardOpen(true);
  }, []);
  useEffect(() => {
    const onShow = () => setWizardOpen(true);
    window.addEventListener("nexus:show-welcome", onShow);
    return () => window.removeEventListener("nexus:show-welcome", onShow);
  }, []);

  // Sprint 5.7.x+ — cold-start warmup. swc_core's parser/codegen
  // are lazy-initialized on the first call (cost ~1-3 s). We
  // pre-compile them once on app mount, in the background, so
  // the FIRST real compression starts instantly instead of
  // freezing for 2 s after the user clicks "Comprimir".
  // Cheap to call repeatedly so we don't bother memoizing.
  useEffect(() => {
    if (typeof window === "undefined") return;
    const w = window as any;
    if (!w.__TAURI_INTERNALS__?.invoke) return; // not running in Tauri
    // Fire-and-forget: we don't await the warmup response. If
    // it fails the user just gets a slow first compress, which
    // is the same behavior as before this fix.
    w.__TAURI_INTERNALS__.invoke("warmup_cmd", { req: {} }).catch(
      () => {
        // Warmup failures are non-fatal — the real compression
        // path will compile swc_core on demand anyway.
      },
    );
  }, []);

  const onNavigate = useCallback((v: View) => {
    setView(v);
  }, []);

  const onOpComplete = useCallback(
    (op: {
      kind: "compress" | "decompress" | "share";
      filename: string;
      originalBytes: number;
      compressedBytes?: number;
      restoredBytes?: number;
      durationMs: number;
    }) => {
      const fullOp: RecentOp = {
        ...op,
        id:
          typeof crypto !== "undefined" && "randomUUID" in crypto
            ? crypto.randomUUID()
            : `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        timestamp: Date.now(),
        status: "ok",
      };
      setLastOp(fullOp);
      setRecentOps((prev) => [fullOp, ...prev].slice(0, 30));
      // Sprint 5.7.32 — toast the success so the user gets feedback
      // even if they're looking at a different part of the UI.
      // Note: toast messages are intentionally hardcoded Spanish
      // here for v1 — they're the only place we still inline text
      // outside i18n, because toasts are transient and the
      // already-existing LocaleProvider hook is the natural place
      // to add `locale` later. Keeping the surface small.
      const verb =
        op.kind === "compress"
          ? "Comprimido"
          : op.kind === "decompress"
            ? "Extraído"
            : "Compartido";
      const filename = op.filename.split(/[\\/]/).pop() || op.filename;
      let body = `${verb} ${filename}`;
      if (op.kind === "compress" && op.compressedBytes && op.originalBytes) {
        const saved = op.originalBytes - op.compressedBytes;
        if (saved > 0) {
          body += ` · ahorraste ${formatBytes(saved, "es")}`;
        }
      }
      toast.success(body, 4500);
    },
    []
  );

  const onClearRecent = useCallback(() => {
    setRecentOps([]);
  }, []);

  // Clear pendingDrop after the view has consumed it.
  const consumeDrop = useCallback(() => setPendingDrop(null), []);

  return (
    <main className="h-screen flex flex-col bg-[#0a0a0a] text-white">
      {/* Ambient gradient background */}
      <div className="fixed inset-0 pointer-events-none">
        <div className="absolute top-0 left-1/4 w-[600px] h-[600px] rounded-full bg-cyan-500/[0.03] blur-[120px]" />
        <div className="absolute bottom-0 right-1/4 w-[600px] h-[600px] rounded-full bg-emerald-500/[0.02] blur-[120px]" />
      </div>

      <NeoTopBar view={view} onNavigate={onNavigate} onOpenPalette={() => setPaletteOpen(true)} />

      {/* Sprint 5.6.29: keyed wrapper re-mounts on view change so the
          `animate-fade-in` keyframe plays on every navigation. */}
      <div
        key={view}
        className="relative flex-1 flex flex-col min-h-0 overflow-hidden animate-fade-in"
      >
        {view === "landing" && <LandingPage onNavigate={onNavigate} ops={recentOps} />}
        {view === "compress" && (
          <CompressView
            onComplete={onOpComplete}
            onNavigate={onNavigate}
            pendingDropPath={pendingDrop?.action === "compress" ? pendingDrop.path : null}
            onConsumedDrop={consumeDrop}
          />
        )}
        {view === "decompress" && (
          <DecompressView
            onComplete={onOpComplete}
            onNavigate={onNavigate}
            pendingDropPath={pendingDrop?.action === "decompress" ? pendingDrop.path : null}
            onConsumedDrop={consumeDrop}
          />
        )}
        {view === "share" && (
          <ShareView
            onComplete={onOpComplete}
            onNavigate={onNavigate}
          />
        )}
        {view === "settings" && <SettingsView />}
        {view === "recent" && (
          <RecentView ops={recentOps} onNavigate={onNavigate} onClear={onClearRecent} />
        )}
      </div>

      <NeoDashboard
        op={lastOp}
        onNavigate={onNavigate}
        onDismiss={() => setLastOp(null)}
      />

      {/* Global UX layers (Sprint 5.7.31). All use `fixed`
          positioning and high z-index so they sit on top of the
          active view. */}
      <DropOverlay onDropPath={handleDropPath} />
      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        onNavigate={(v) => setView(v)}
      />
      <FirstRunWizard
        open={wizardOpen}
        onClose={() => setWizardOpen(false)}
      />
      <Toaster />
    </main>
  );
}

// ============================================================
//  Settings — Sprint 5.7 rewrite
//
//  Sprint 5.7: extracted to its own component file with tabbed
//  sidebar navigation (Interfaz / Compresión / Transporte /
//  Almacenamiento / Acerca de). The page.tsx <Home> component
//  just imports the new view and drops it in.
// ============================================================
