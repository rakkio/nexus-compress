"use client";

/**
 * DropOverlay — Sprint 5.7.31 global drag-drop.
 *
 * Renders a full-screen overlay whenever the user drags a file over
 * the app window. The overlay:
 *   1. Detects file types via the OS-native `tauri://drag-drop` event
 *      (when running under the Tauri shell) OR via standard HTML
 *      `dragenter`/`drop` (when running under `next dev` / browser).
 *   2. Auto-routes to the correct view based on extension:
 *        - archives (.tar, .nxs, .nxs6, .zip, .7z, .rar, .gz, .bz2,
 *          .xz, .lz4, .zst, .nxar) → Decompress
 *        - everything else              → Compress
 *   3. Hands the path off to the active view via a callback so
 *      CompressView/DecompressView can pre-populate the file
 *      picker and skip the manual selection step.
 *
 * UX details:
 *   - Overlay fades in/out (200 ms) so it doesn't flash on every
 *     tiny mouse movement through the window.
 *   - Single large icon + action verb ("Suelta para comprimir")
 *     instead of tiny text — drag-drop state is glanceable.
 *   - The overlay never blocks keyboard input (pointer-events: none
 *     except on the visual frame) so we don't trap focus.
 */

import { useEffect, useRef, useState, useCallback } from "react";
import {
  Archive,
  FolderOpen,
  FileText,
  ChevronDown,
} from "lucide-react";

// ── File-type detection ─────────────────────────────────────────
// Single source of truth: anything listed here is treated as an
// archive and routed to DecompressView; anything else goes to
// CompressView. Add new archive formats here.
const ARCHIVE_EXTS = new Set([
  "tar",
  "nxs",
  "nxs6",
  "nx6r",
  "nxar",
  "nxe",
  "nxr",
  "zip",
  "7z",
  "rar",
  "gz",
  "tgz",
  "bz2",
  "xz",
  "txz",
  "lz",
  "lz4",
  "zst",
  "lzma",
  "br",
]);

export function classifyPath(path: string): "archive" | "compress" {
  const lower = path.toLowerCase();
  // strip any trailing query/fragment
  const clean = lower.split("?")[0].split("#")[0];
  // double-extension handling: foo.tar.gz → "gz" is the real ext
  const dot = clean.lastIndexOf(".");
  if (dot < 0) return "compress";
  const ext = clean.slice(dot + 1);
  return ARCHIVE_EXTS.has(ext) ? "archive" : "compress";
}

type DropAction = "compress" | "archive";

interface DropOverlayProps {
  /** Called when the user drops a file. The path comes from
   *  Tauri's `tauri://drag-drop` (preferred) or the browser's
   *  DataTransfer (fallback). `action` is "archive" if the file
   *  looks like an archive (should be extracted) and "compress"
   *  otherwise. */
  onDropPath: (path: string, action: DropAction) => void;
}

/**
 * Just inside the window so it disappears immediately when the user
 * drags out of bounds.
 */
export function DropOverlay({ onDropPath }: DropOverlayProps) {
  const [visible, setVisible] = useState(false);
  // Set by Tauri's `tauri://drag-drop` (real OS path) OR by the
  // HTML drop event (filename only when in browser).
  const pendingRef = useRef<string | null>(null);
  const counterRef = useRef(0);
  // The action we plan to take — used for the overlay label.
  const [plannedAction, setPlannedAction] = useState<DropAction>("compress");
  const [plannedName, setPlannedName] = useState<string>("");

  const show = useCallback(() => {
    counterRef.current += 1;
    setVisible(true);
  }, []);

  const hideIfNoDrag = useCallback(() => {
    counterRef.current -= 1;
    if (counterRef.current <= 0) {
      counterRef.current = 0;
      setVisible(false);
    }
  }, []);

  useEffect(() => {
    // ── Native Tauri drag-drop (preferred — has real paths) ──
    const tauri = (window as any).__TAURI__?.event;
    let unlistenEnter: (() => void) | undefined;
    let unlistenOver: (() => void) | undefined;
    let unlistenLeave: (() => void) | undefined;
    let unlistenDrop: (() => void) | undefined;
    if (tauri?.listen) {
      (async () => {
        try {
          unlistenEnter = await tauri.listen("tauri://drag-enter", () => show());
          unlistenOver = await tauri.listen("tauri://drag-over", () => show());
          unlistenLeave = await tauri.listen("tauri://drag-leave", () => hideIfNoDrag());
          unlistenDrop = await tauri.listen("tauri://drag-drop", (e: any) => {
            const paths: string[] = e?.payload?.paths ?? [];
            if (!paths.length) return;
            const p = paths[0];
            pendingRef.current = p;
            const action = classifyPath(p);
            setPlannedAction(action);
            setPlannedName(p.split(/[/\\]/).pop() || p);
            setVisible(true);
            // hand off and hide
            counterRef.current = 0;
            setVisible(false);
            onDropPath(p, action);
          });
        } catch {
          // ignore — fall back to HTML drag-drop below
        }
      })();
    }

    // ── HTML drag-drop (fallback for `next dev`) ──
    //
    // Sprint 5.7.32: these listeners attach in **capture phase**
    // (`{ capture: true }`) so they fire BEFORE any React onDrop
    // listener on a child element. When we handle the drop here, we
    // call `e.stopImmediatePropagation()` to prevent BOTH the bubble
    // phase AND any sibling listeners from running — this avoids the
    // double-drop bug where the global DropOverlay and the local
    // per-view dropzone both consume the same file.
    const onDragEnter = (e: DragEvent) => {
      if (!e.dataTransfer || !Array.from(e.dataTransfer.types).includes("Files")) return;
      e.preventDefault();
      show();
    };
    const onDragOver = (e: DragEvent) => {
      if (!e.dataTransfer || !Array.from(e.dataTransfer.types).includes("Files")) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = "copy";
    };
    const onDragLeave = (e: DragEvent) => {
      // relatedTarget = where the cursor went. If it's null or
      // outside the window, hide. We use the simple counter pattern
      // to avoid flicker as the cursor crosses nested elements.
      if (e.relatedTarget === null) hideIfNoDrag();
    };
    const onDrop = (e: DragEvent) => {
      const file = e.dataTransfer?.files?.[0];
      if (!file) {
        e.preventDefault();
        e.stopImmediatePropagation();
        setVisible(false);
        return;
      }
      // Stop the event so the local per-view dropzone never sees it.
      e.preventDefault();
      e.stopImmediatePropagation();
      // In `next dev` we only have the filename, not a path.
      const name = file.name;
      const action = classifyPath(name);
      pendingRef.current = null; // no real path in browser mode
      setPlannedAction(action);
      setPlannedName(name);
      counterRef.current = 0;
      setVisible(false);
      onDropPath(name, action);
    };
    window.addEventListener("dragenter", onDragEnter, { capture: true });
    window.addEventListener("dragover", onDragOver, { capture: true });
    window.addEventListener("dragleave", onDragLeave, { capture: true });
    window.addEventListener("drop", onDrop, { capture: true });

    return () => {
      unlistenEnter?.();
      unlistenOver?.();
      unlistenLeave?.();
      unlistenDrop?.();
      window.removeEventListener("dragenter", onDragEnter, { capture: true } as any);
      window.removeEventListener("dragover", onDragOver, { capture: true } as any);
      window.removeEventListener("dragleave", onDragLeave, { capture: true } as any);
      window.removeEventListener("drop", onDrop, { capture: true } as any);
    };
  }, [show, hideIfNoDrag, onDropPath]);

  // ── Visual ──
  return (
    <div
      aria-hidden={!visible}
      className={
        "fixed inset-0 z-[100] flex items-center justify-center " +
        "transition-opacity duration-200 pointer-events-none " +
        (visible ? "opacity-100" : "opacity-0")
      }
    >
      {/* Backdrop: full-screen tint that "punches a hole" for the
          app underneath. We avoid blurring so the user can still
          see the content beneath. */}
      <div
        className="absolute inset-0"
        style={{
          background:
            "radial-gradient(ellipse at center, rgba(34,211,238,0.18) 0%, rgba(0,0,0,0.55) 70%)",
          backdropFilter: "blur(2px)",
          WebkitBackdropFilter: "blur(2px)",
        }}
      />
      {/* The "drop card" — a centred rounded frame with the action
          verb and icon. The whole stack is pointer-events: none so
          we don't capture mouse events from the OS drag operation. */}
      <div
        className={
          "relative rounded-3xl px-14 py-10 border-2 border-dashed " +
          "transition-transform duration-200 " +
          (visible ? "scale-100" : "scale-95") +
          " " +
          (plannedAction === "archive"
            ? "border-amber-400"
            : "border-cyan-400")
        }
        style={{
          background:
            "linear-gradient(180deg, rgba(20,20,24,0.96) 0%, rgba(10,10,12,0.96) 100%)",
          boxShadow:
            "0 0 80px -8px " +
            (plannedAction === "archive"
              ? "rgba(251,191,36,0.55)"
              : "rgba(34,211,238,0.55)"),
        }}
      >
        <div className="flex flex-col items-center gap-4 text-center">
          <div
            className={
              "w-20 h-20 rounded-2xl flex items-center justify-center " +
              (plannedAction === "archive"
                ? "bg-amber-500/15 text-amber-300"
                : "bg-cyan-500/15 text-cyan-300")
            }
          >
            {plannedAction === "archive" ? (
              <FolderOpen size={36} strokeWidth={1.6} />
            ) : (
              <Archive size={36} strokeWidth={1.6} />
            )}
          </div>
          <div>
            <p
              className={
                "text-[11px] tracking-[0.3em] uppercase font-semibold mb-1 " +
                (plannedAction === "archive" ? "text-amber-300" : "text-cyan-300")
              }
            >
              {plannedAction === "archive" ? "Detectado" : "Detectado"}
            </p>
            <h2 className="text-white text-[28px] font-semibold tracking-tight">
              {plannedAction === "archive"
                ? "Suelta para extraer"
                : "Suelta para comprimir"}
            </h2>
            {plannedName ? (
              <p className="text-zinc-400 text-[12.5px] mt-2 font-mono flex items-center gap-2 justify-center">
                <FileText size={12} className="text-zinc-500" />
                {plannedName}
              </p>
            ) : null}
          </div>
          <div className="flex items-center gap-2 mt-2 text-zinc-500 text-[11px]">
            <ChevronDown size={12} className="animate-bounce" />
            <span>Suelta el archivo en cualquier parte de la ventana</span>
          </div>
        </div>
      </div>
    </div>
  );
}