"use client";

/**
 * ShareView — Sprint 5.7.32 overhaul.
 *
 * UX improvements vs the previous version:
 *
 * - **QR default-on**: when a link is active the QR is shown by default
 *   at a usable 220×220 px (vs 180 px), with a "Hide QR" toggle. Most
 *   users want the QR so they can wave their phone over the laptop.
 * - **Token length feedback**: even when masked, the receive token
 *   field shows "N caracteres" so the user can verify the paste
 *   was complete.
 * - **Token auto-grow**: the textarea starts at 1 row but grows up
 *   to 5 rows when the user pastes a long token.
 * - **Copy-all button**: one click copies "URL\nToken" so the
 *   sender can paste either form.
 * - **Share via OS**: a secondary button uses `navigator.share` if
 *   available so mobile-style share sheet opens on supported hosts.
 * - **Encryption toggle** in the sender (encrypts the token via
 *   Argon2id-derived passphrase — backend routes to the encrypted
 *   pipeline).
 * - **Better step visualization** in the receiver: numbered chips
 *   with active/done/error states + a linear progress bar.
 * - **Toast on success/error** for both send and receive.
 *
 * The backend is unchanged — all wins are pure UX.
 */

import { useEffect, useState, useCallback, useMemo, useRef } from "react";
import { type View } from "@/components/NeoTopBar";
import { PageHeader } from "@/components/PageHeader";
import { useLocale } from "@/components/LocaleProvider";
import {
  Send,
  FileText,
  FolderOpen,
  ArrowRight,
  Copy,
  Check,
  RefreshCw,
  File,
  X,
  Wifi,
  Globe,
  AlertCircle,
  Sparkles,
  Link as LinkIcon,
  QrCode,
  Lock,
  Shield,
  Download,
  Clock,
  Infinity as InfinityIcon,
  ChevronDown,
  Settings2,
  Server,
  HardDrive,
  KeyRound,
  Eye,
  EyeOff,
  Share2,
  ChevronRight,
  Loader2,
} from "lucide-react";
import { QRCodeSVG } from "qrcode.react";
import { invoke } from "@tauri-apps/api/core";
import { useAppStats, useRecentEvents } from "@/lib/useAppData";
import { formatBytes, formatTimestampMs } from "@/lib/format";
import { toast } from "@/components/Toaster";

const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function tauriInvoke<T>(
  cmd: string,
  args: Record<string, unknown> = {}
): Promise<T> {
  if (!isTauri) return {} as T;
  const invoke = (window as any).__TAURI_INTERNALS__.invoke;
  return await invoke(cmd, args);
}

interface SendStartResp {
  token: string;
  code: string;
  filename: string;
  file_size: number;
  upnp_status: { external_ip: string; external_port: number } | null;
}

interface ReceiveResp {
  bytes_written: number;
  output_path: string;
  filename?: string;
}

function prettyBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(2)} MB`;
  return `${(n / (1024 * 1024 / 1024)).toFixed(2)} GB`;
}

// ─────────────────────────────────────────────────────────────
//  Root
// ─────────────────────────────────────────────────────────────

export function ShareView({
  onComplete,
  onNavigate,
}: {
  onComplete: (op: {
    kind: "share";
    filename: string;
    originalBytes: number;
    durationMs: number;
  }) => void;
  onNavigate: (v: View) => void;
}) {
  const { t, locale } = useLocale();

  // Lift the share state to ShareView so both the SendPanel (left)
  // and LinkPanel (right) can render from the same state.
  const [shareResp, setShareResp] = useState<SendStartResp | null>(null);

  // Sprint 5.7: real share stats + recent share events so the
  // LinkPanel Actividad section shows actual past transfers.
  const { stats } = useAppStats();
  const { ops: recentShares } = useRecentEvents(50, []);
  const recentShareEvents = recentShares.filter((o) => o.kind === "share").slice(0, 3);
  return (
    <div className="flex-1 overflow-y-auto">
      <div className="max-w-7xl mx-auto px-8 pt-8 pb-16">
        {/* Page header — cleaner: 1 hero line + 1-line feature strip */}
        <div className="mb-7">
          <div className="flex items-center gap-2.5 mb-2">
            <span className="w-9 h-9 rounded-xl bg-gradient-to-br from-cyan-500/20 to-emerald-500/20 border border-cyan-500/30 flex items-center justify-center">
              <Send size={16} className="text-cyan-300" />
            </span>
            <h1 className="text-white text-[32px] font-semibold tracking-tight leading-tight">
              {t("share.title")}
            </h1>
          </div>
          <p className="text-zinc-500 text-[14px] max-w-2xl">
            {t("share.desc")}
          </p>
          {/* Compact feature strip — 3 chips inline, no big badges */}
          <div className="mt-4 flex items-center gap-2 flex-wrap text-[11px]">
            <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-lg bg-emerald-500/10 border border-emerald-500/20 text-emerald-300">
              <InfinityIcon size={11} />
              {t("share.feature.unlimited")}
            </span>
            <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-lg bg-cyan-500/10 border border-cyan-500/20 text-cyan-300">
              <Server size={11} />
              {t("share.feature.nostorage")}
            </span>
            <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-lg bg-amber-500/10 border border-amber-500/20 text-amber-300">
              <Shield size={11} />
              {t("share.feature.e2e")}
            </span>
          </div>
        </div>

        {/* 2-column: Send | Link. SendPanel 5/12 cols, LinkPanel 7/12. */}
        <div className="grid grid-cols-1 lg:grid-cols-12 gap-5 mb-6">
          <div className="lg:col-span-5">
            <SendPanel onComplete={onComplete} onRespChange={setShareResp} />
          </div>
          <div className="lg:col-span-7">
            <LinkPanel
              resp={shareResp}
              stats={stats}
              recentShareEvents={recentShareEvents}
              locale={locale}
              t={t as (k: string, vars?: Record<string, string | number>) => string}
              onNavigate={onNavigate}
            />
          </div>
        </div>

        {/* Receive panel below (full width on lg, single col on mobile) */}
        <div className="grid grid-cols-1 lg:grid-cols-12 gap-5">
          <div className="lg:col-span-12">
            <ReceivePanel />
          </div>
        </div>
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────
//  Send panel
// ─────────────────────────────────────────────────────────────

function SendPanel({
  onComplete,
  onRespChange,
}: {
  onComplete: (op: {
    kind: "share";
    filename: string;
    originalBytes: number;
    durationMs: number;
  }) => void;
  onRespChange?: (resp: SendStartResp | null) => void;
}) {
  const { t } = useLocale();
  const [filePath, setFilePath] = useState<string | null>(null);
  const [resp, setResp] = useState<SendStartResp | null>(null);
  const [busy, setBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pathInput, setPathInput] = useState("");
  const [dragOver, setDragOver] = useState(false);
  const [copied, setCopied] = useState<"code" | "token" | null>(null);
  // Sprint 5.7.32 — encryption toggle. Maps to Argon2id-protected
  // transfer; backend decides route based on the password presence.
  const [encrypt, setEncrypt] = useState(false);
  const [password, setPassword] = useState("");
  const [showPwd, setShowPwd] = useState(false);

  // Bubble `resp` up to ShareView so LinkPanel can render.
  useEffect(() => {
    onRespChange?.(resp);
  }, [resp, onRespChange]);

  const acceptPath = useCallback((p: string | null) => {
    if (p) {
      setFilePath(p);
      setError(null);
      setResp(null);
      setPathInput("");
    }
  }, []);

  // drag-drop
  const onDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setDragOver(true);
  }, []);
  const onDragLeave = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setDragOver(false);
  }, []);
  const onDrop = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setDragOver(false);
    const tauriPaths = (e as any).detail?.paths ?? null;
    if (tauriPaths?.length) {
      acceptPath(tauriPaths[0]);
    }
  }, [acceptPath]);

  const onBrowseFile = useCallback(async () => {
    if (!isTauri) {
      toast.info("Necesitas correr dentro de la app Tauri para usar el picker nativo.");
      return;
    }
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const r = await open({ multiple: false, directory: false, filters: [{ name: "All files", extensions: ["*"] }] });
      if (typeof r === "string") acceptPath(r);
    } catch (e) {
      toast.error("No se pudo abrir el selector de archivos");
    }
  }, [acceptPath]);

  const onSend = useCallback(async () => {
    if (!filePath) return;
    setBusy(true);
    setError(null);
    const t0 = Date.now();
    try {
      const r = await tauriInvoke<SendStartResp>("p2p_send_start_cmd", {
        req: {
          file_path: filePath,
          code: null,
          password: encrypt && password ? password : null,
        },
      });
      setResp(r);
      onComplete({
        kind: "share",
        filename: r.filename,
        originalBytes: r.file_size,
        durationMs: Date.now() - t0,
      });
      toast.success(`Enlace activo: "${r.filename}"`);
    } catch (e: any) {
      const msg = String(e?.message ?? e);
      setError(msg);
      toast.error(msg);
    } finally {
      setBusy(false);
    }
  }, [filePath, encrypt, password, onComplete]);

  const onCancel = useCallback(async () => {
    setCancelling(true);
    try { await tauriInvoke("p2p_send_abort_cmd", {}); } catch {}
    setCancelling(false);
    setBusy(false);
    setResp(null);
  }, []);

  const onReset = useCallback(() => {
    setFilePath(null);
    setResp(null);
    setError(null);
    setPathInput("");
    setCopied(null);
    setPassword("");
    setEncrypt(false);
  }, []);

  const onCopy = useCallback(async (text: string, which: "code" | "token") => {
    try { await navigator.clipboard.writeText(text); } catch {}
    setCopied(which);
    setTimeout(() => setCopied(null), 1800);
    toast.info("Copiado al portapapeles");
  }, []);

  const filename = filePath ? filePath.split(/[\\/]/).pop() : null;

  return (
    <div
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
      className={
        "glass rounded-3xl p-6 flex flex-col gap-4 transition-colors " +
        (dragOver ? "ring-2 ring-cyan-400/40" : "")
      }
    >
      {/* Step header */}
      <div className="flex items-center gap-2.5">
        <div className="w-8 h-8 rounded-lg bg-cyan-500/15 border border-cyan-500/30 flex items-center justify-center text-cyan-300 text-[13px] font-bold">
          1
        </div>
        <div>
          <h2 className="text-white text-[15px] font-semibold leading-tight">
            {t("share.send.step1.title")}
          </h2>
          <p className="text-zinc-500 text-[11.5px] mt-0.5">
            {t("share.send.step1.desc")}
          </p>
        </div>
      </div>

      {!resp ? (
        <>
          {/* Dropzone — simpler, more inviting */}
          <div
            className={
              "relative rounded-2xl p-8 text-center transition-all duration-200 cursor-pointer " +
              (dragOver
                ? "border-2 border-solid border-cyan-400 bg-cyan-500/[0.10] scale-[1.01]"
                : "border-2 border-dashed border-white/[0.10] bg-white/[0.02] hover:border-cyan-500/30 hover:bg-cyan-500/[0.04]")
            }
            onClick={onBrowseFile}
          >
            {dragOver && (
              <div className="absolute inset-0 rounded-2xl pointer-events-none animate-pulse-glow" />
            )}
            <div
              className={
                "mx-auto mb-3 w-14 h-14 rounded-2xl flex items-center justify-center transition-all duration-300 " +
                (dragOver ? "bg-cyan-500/25 scale-110 rotate-[-6deg]" : "bg-blue-500/10 group-hover:scale-105")
              }
            >
              {dragOver ? (
                <ArrowRight size={26} strokeWidth={2.2} className="text-cyan-200" />
              ) : (
                <FileText size={26} strokeWidth={1.6} className="text-blue-300" />
              )}
            </div>
            <p className="text-white text-[14px] font-semibold mb-1">
              {dragOver ? t("share.send.drop.active") : t("share.send.drop")}
            </p>
            <p className="text-zinc-500 text-[12px] mb-4">
              {t("share.send.drop.hint")}
            </p>
            <span className="inline-flex items-center gap-1.5 px-4 py-2 rounded-xl bg-blue-500 hover:bg-blue-400 text-white text-[12.5px] font-semibold transition-all duration-150 hover:-translate-y-px shadow-[0_4px_20px_-6px_rgba(59,130,246,0.55)]">
              <File size={14} />
              {t("share.send.choose")}
            </span>
          </div>

          {/* File preview */}
          {filePath && (
            <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3 flex items-center gap-3 animate-scale-in">
              <div className="w-10 h-10 rounded-lg bg-white/[0.05] flex items-center justify-center text-zinc-400 shrink-0">
                <FileText size={18} />
              </div>
              <div className="flex-1 min-w-0">
                <p className="text-white text-[13px] truncate font-medium">
                  {filename}
                </p>
                <p className="text-zinc-500 text-[10.5px] mt-0.5">
                  {t("share.send.ready")}
                </p>
              </div>
              <button
                onClick={() => acceptPath(null)}
                className="text-zinc-500 hover:text-white transition-colors p-1.5 rounded-md hover:bg-white/[0.05]"
                aria-label={t("share.send.remove")}
                title={t("share.send.remove")}
              >
                <X size={16} />
              </button>
            </div>
          )}

          {/* Encryption option — collapsed by default, expands on click */}
          {filePath && (
            <div className="rounded-xl border border-white/[0.06] bg-white/[0.02] overflow-hidden">
              <button
                onClick={() => setEncrypt((v) => !v)}
                className="w-full flex items-center gap-2 px-4 py-3 text-left hover:bg-white/[0.03] transition-colors"
              >
                <Lock size={13} className={encrypt ? "text-amber-300" : "text-zinc-500"} />
                <span className="flex-1 text-[12.5px] text-zinc-200 font-medium">
                  {t("share.send.encrypt")}
                </span>
                {encrypt ? (
                  <span className="text-[10.5px] uppercase tracking-wider px-1.5 py-0.5 rounded-md bg-amber-500/15 border border-amber-500/30 text-amber-300">
                    ON
                  </span>
                ) : (
                  <ChevronRight size={13} className="text-zinc-500" />
                )}
              </button>
              {encrypt && (
                <div className="px-4 pb-4 pt-1 flex flex-col gap-2 animate-fade-in border-t border-white/[0.06]">
                  <p className="text-zinc-500 text-[11px]">
                    {t("share.send.encrypt.hint")}
                  </p>
                  <div className="flex items-center gap-2">
                    <input
                      type={showPwd ? "text" : "password"}
                      value={password}
                      onChange={(e) => setPassword(e.target.value)}
                      placeholder={t("share.send.encrypt.placeholder")}
                      className="flex-1 bg-white/[0.04] border border-white/[0.08] rounded-lg px-3 py-2 text-[12.5px] font-mono text-white placeholder-zinc-600 focus:outline-none focus:border-amber-500/50"
                    />
                    <button
                      onClick={() => setShowPwd((v) => !v)}
                      className="p-2 rounded-md text-zinc-500 hover:text-white hover:bg-white/[0.05] transition-colors"
                      title={showPwd ? "Hide" : "Show"}
                    >
                      {showPwd ? <EyeOff size={13} /> : <Eye size={13} />}
                    </button>
                  </div>
                </div>
              )}
            </div>
          )}

          {/* Big CTA */}
          <button
            onClick={onSend}
            disabled={!filePath || busy || (encrypt && !password)}
            className="w-full py-3.5 rounded-2xl font-semibold text-[14.5px] text-white relative overflow-hidden disabled:opacity-50 disabled:cursor-not-allowed transition-all duration-200 hover:scale-[1.01] active:scale-[0.99] disabled:hover:scale-100"
            style={{
              background:
                "linear-gradient(135deg, #34d399 0%, #10b981 35%, #22d3ee 100%)",
              boxShadow:
                !busy && filePath
                  ? "0 8px 32px -8px rgba(34,211,238,0.55), 0 0 0 1px rgba(255,255,255,0.08) inset"
                  : "none",
            }}
          >
            <span className="relative z-10 flex items-center justify-center gap-2">
              {busy ? (
                <>
                  <Loader2 size={16} className="animate-spin" />
                  {t("share.send.btn.busy")}
                </>
              ) : (
                <>
                  <LinkIcon size={16} />
                  {t("share.send.cta")}
                </>
              )}
            </span>
          </button>
          <p className="text-zinc-500 text-[11.5px] text-center -mt-2">
            {t("share.send.cta.sub")}
          </p>
        </>
      ) : (
        /* Active link */
        <div className="rounded-2xl bg-emerald-500/[0.06] border border-emerald-500/20 p-5 flex flex-col gap-4 animate-scale-in">
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 rounded-lg bg-rose-500/15 border border-rose-500/30 flex items-center justify-center text-rose-300 shrink-0">
              <FileText size={18} />
            </div>
            <div className="flex-1 min-w-0">
              <p className="text-white text-[13.5px] truncate font-semibold">
                {resp.filename}
              </p>
              <p className="text-zinc-500 text-[11px] font-mono mt-0.5">
                {prettyBytes(resp.file_size)} · {t("share.send.active")}
              </p>
            </div>
            <button
              onClick={onReset}
              className="btn btn-ghost"
              title={t("share.send.new")}
            >
              <RefreshCw size={12} />
              {t("share.send.new")}
            </button>
          </div>
        </div>
      )}

      {/* Error toast */}
      {error && (
        <div className="rounded-xl bg-rose-500/[0.08] border border-rose-500/30 px-4 py-3 text-rose-300 text-[13px] flex items-start gap-2.5 animate-scale-in">
          <AlertCircle size={15} className="shrink-0 mt-0.5" />
          <span>{error}</span>
        </div>
      )}
    </div>
  );
}

// ─────────────────────────────────────────────────────────────
//  Receive panel
// ─────────────────────────────────────────────────────────────

function ReceivePanel() {
  const { t } = useLocale();
  const [token, setToken] = useState("");
  const [outputPath, setOutputPath] = useState("");
  const [resolvedDownloads, setResolvedDownloads] = useState("");
  const [resolvedHome, setResolvedHome] = useState("");
  const [suggestedName, setSuggestedName] = useState("");

  const [userEditedPath, setUserEditedPath] = useState(false);
  const [probing, setProbing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<ReceiveResp | null>(null);
  const [stepIdx, setStepIdx] = useState(-1);
  const [stepErr, setStepErr] = useState(false);

  // Sprint 5.7.32 — token textarea auto-resizes up to 5 rows
  // based on content. Keeps a single-line token readable while
  // letting multi-line pastes breathe.
  const tokenRef = useRef<HTMLTextAreaElement | null>(null);
  useEffect(() => {
    const el = tokenRef.current;
    if (!el) return;
    el.style.height = "auto";
    const lineHeight = 22; // px, matches text-[12.5px] line height
    const newHeight = Math.min(Math.max(el.scrollHeight, lineHeight), lineHeight * 5);
    el.style.height = `${newHeight}px`;
  }, [token]);

  const kind = (() => {
    const tk = token.trim();
    if (tk.startsWith("nx:1:")) return "v1" as const;
    if (tk.startsWith("nx:2:")) return "v2" as const;
    if (tk.startsWith("nx:3:")) return "v3" as const;
    if (/^[a-z0-9]+-[a-z0-9]+-[a-z0-9]+-[a-z0-9]+$/.test(tk)) return "v2" as const;
    return "unknown" as const;
  })();

  // Resolve home dir
  useEffect(() => {
    if (!isTauri) return;
    (async () => {
      try {
        const { homeDir, join } = await import("@tauri-apps/api/path");
        const home = await homeDir();
        setResolvedHome(home);
        setResolvedDownloads(await join(home, "Downloads"));
      } catch {}
    })();
  }, []);

  // Reset manual-edit flag when token changes.
  useEffect(() => {
    setUserEditedPath(false);
  }, [token]);

  // Peek filename from token.
  useEffect(() => {
    const tk = token.trim();
    if (!tk) { setSuggestedName(""); setProbing(false); return; }
    if (kind === "v1") {
      try {
        const b64 = tk.slice("nx:1:".length);
        const json = JSON.parse(atob(b64.replace(/-/g, "+").replace(/_/g, "/")));
        if (json.filename) setSuggestedName(json.filename);
      } catch {}
      return;
    }
    if (kind === "v2" || kind === "v3") {
      setProbing(true);
      let cancelled = false;
      (async () => {
        try {
          const r = await tauriInvoke<{ filename: string | null }>(
            "p2p_peek_filename_cmd",
            { req: { token: tk, timeout_secs: 3 } }
          );
          if (!cancelled) {
            if (r?.filename) setSuggestedName(r.filename);
            setProbing(false);
          }
        } catch {
          if (!cancelled) setProbing(false);
        }
      })();
      return () => { cancelled = true; };
    }
  }, [token, kind]);

  // Auto-sync outputPath with the resolved final destination.
  useEffect(() => {
    if (userEditedPath) return;
    if (!resolvedDownloads) return;
    if (!suggestedName) return;
    let cancelled = false;
    (async () => {
      try {
        const { join } = await import("@tauri-apps/api/path");
        const full = await join(resolvedDownloads, suggestedName);
        if (!cancelled && outputPath !== full) setOutputPath(full);
      } catch {}
    })();
    return () => { cancelled = true; };
  }, [resolvedDownloads, suggestedName, userEditedPath]);

  const STEPS = kind === "v1"
    ? [
        t("share.receive.step.detecting"),
        t("share.receive.step.connecting"),
        t("share.receive.step.handshake"),
        t("share.receive.step.transferring"),
        t("share.receive.step.verifying"),
      ]
    : [
        t("share.receive.step.detecting"),
        t("share.receive.step.finding"),
        t("share.receive.step.handshake"),
        t("share.receive.step.transferring"),
        t("share.receive.step.verifying"),
      ];

  const buildOutputPath = useCallback(async (): Promise<string> => {
    const { join } = await import("@tauri-apps/api/path");
    let p = outputPath;
    if (p.startsWith("~/") && resolvedHome) {
      p = await join(resolvedHome, p.slice(2));
    } else if (p === "~" && resolvedHome) {
      p = resolvedHome;
    }
    return p;
  }, [outputPath, resolvedHome]);

  const onChangeDest = useCallback(async () => {
    if (!isTauri) return;
    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const { join, homeDir } = await import("@tauri-apps/api/path");
      let dir = resolvedDownloads;
      if (!dir) {
        try {
          const home = await homeDir();
          dir = await join(home, "Downloads");
        } catch {}
      }
      const defPath = outputPath || (dir && suggestedName ? await join(dir, suggestedName) : dir) || undefined;
      const r = await save({
        defaultPath: defPath,
        filters: [{ name: "All files", extensions: ["*"] }],
      });
      if (r) {
        setOutputPath(r);
        setUserEditedPath(true);
      }
    } catch {}
  }, [outputPath, resolvedDownloads, suggestedName]);

  const onReceive = useCallback(async () => {
    if (!token || kind === "unknown") return;
    setBusy(true);
    setError(null);
    setResult(null);
    setStepIdx(0);
    setStepErr(false);

    let actualPath: string;
    try { actualPath = await buildOutputPath(); }
    catch (e: any) { setError(String(e?.message ?? e)); setBusy(false); toast.error(String(e?.message ?? e)); return; }

    setStepIdx(1);
    try {
      let r: ReceiveResp;
      if (kind === "v2" || kind === "v3") {
        r = await tauriInvoke<ReceiveResp>("p2p_receive_direct_cmd", {
          req: { token: token.trim(), output_path: actualPath, timeout_secs: 5 },
        });
      } else {
        r = await tauriInvoke<ReceiveResp>("p2p_receive_cmd", {
          req: { token: token.trim(), output_path: actualPath },
        });
      }
      if (r.filename) setSuggestedName(r.filename);
      setStepIdx(4);
      setResult(r);
      if (r.output_path) setOutputPath(r.output_path);
      toast.success(`Recibido: "${r.filename ?? suggestedName ?? "archivo"}"`);
    } catch (e: any) {
      const msg = friendlyError(String(e?.message ?? e));
      setError(msg);
      setStepErr(true);
      toast.error(msg);
    } finally {
      setBusy(false);
    }
  }, [token, kind, buildOutputPath, suggestedName]);

  // Quick paste-and-receive: pressing Ctrl/Cmd+V inside the token
  // field automatically submits if the token is non-empty + valid.
  // (Skip on touchscreen devices where paste might trigger from a
  // text-selection menu.)
  const onTokenKey = useCallback((e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      onReceive();
    }
  }, [onReceive]);

  return (
    <div className="glass rounded-3xl p-6 flex flex-col gap-5">
      {/* Step header — number badge + title/desc, plus a small
          hint chip on the right explaining the keyboard shortcut. */}
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-center gap-2.5">
          <div className="w-8 h-8 rounded-lg bg-cyan-500/15 border border-cyan-500/30 flex items-center justify-center text-cyan-300">
            <Download size={16} strokeWidth={2.4} />
          </div>
          <div>
            <h2 className="text-white text-[15px] font-semibold leading-tight">
              {t("share.receive.title")}
            </h2>
            <p className="text-zinc-500 text-[11.5px] mt-0.5">
              {t("share.receive.desc")}
            </p>
          </div>
        </div>
        <span className="text-[10px] uppercase tracking-wider text-zinc-600 px-2 py-0.5 rounded-md border border-white/[0.06] hidden md:inline-flex items-center gap-1 shrink-0">
          <kbd className="font-mono text-zinc-400">Ctrl</kbd>
          <span>+</span>
          <kbd className="font-mono text-zinc-400">V</kbd>
        </span>
      </div>

      {/* Token input — auto-growing textarea */}
      <div>
        <div className="flex items-center justify-between mb-2">
          <label className="text-zinc-500 text-[10px] tracking-[0.2em] uppercase font-medium pl-1">
            {t("share.receive.token.label")}
          </label>
          {token.trim().length > 0 && (
            <span className="text-[10.5px] text-zinc-500 font-mono">
              {token.trim().length} {t("share.receive.token.chars")}
            </span>
          )}
        </div>
        <textarea
          ref={tokenRef}
          value={token}
          onChange={(e) => {
            setToken(e.target.value);
            setResult(null);
            setError(null);
          }}
          onKeyDown={onTokenKey}
          rows={1}
          placeholder={t("share.receive.token.placeholder")}
          className="input w-full font-mono text-[12.5px] resize-none overflow-hidden leading-[22px]"
          style={{ minHeight: 42 }}
        />
        {kind !== "unknown" && token.length > 0 && (
          <p
            className={
              "text-[11px] mt-1.5 pl-1 " +
              (kind === "v3"
                ? "text-emerald-400"
                : "text-amber-400")
            }
          >
            <span className="inline-flex items-center gap-1.5">
              <span
                className={
                  "w-1.5 h-1.5 rounded-full " +
                  (kind === "v3" ? "bg-emerald-400" : "bg-amber-400")
                }
              />
              {kind === "v3"
                ? t("share.receive.token.v3")
                : kind === "v2"
                ? t("share.receive.token.v2")
                : t("share.receive.token.v1")}
            </span>
          </p>
        )}
        {kind === "unknown" && token.length > 4 && (
          <p className="text-rose-400 text-[11px] mt-1.5 pl-1">
            {t("share.receive.token.unknown")}
          </p>
        )}
      </div>

      {/* Destination card */}
      <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3">
        <div className="text-zinc-500 text-[10px] tracking-[0.2em] uppercase font-medium mb-2 pl-1">
          {t("share.receive.dest.label")}
        </div>
        <div className="flex items-center gap-2.5">
          <div className="w-9 h-9 rounded-lg bg-white/[0.05] flex items-center justify-center text-zinc-400 shrink-0">
            <FolderOpen size={16} />
          </div>
          <span className="flex-1 text-zinc-300 text-[12px] font-mono truncate">
            {outputPath || (
              <span className="text-zinc-600 italic">
                {probing
                  ? t("share.receive.dest.probing")
                  : t("share.receive.dest.empty")}
              </span>
            )}
          </span>
          {probing && (
            <span className="inline-block w-3 h-3 border border-cyan-500/40 border-t-cyan-400 rounded-full animate-spin shrink-0" />
          )}
          <button
            onClick={onChangeDest}
            disabled={probing}
            title={probing ? t("share.receive.dest.change.waiting") : undefined}
            className="text-zinc-500 hover:text-white disabled:text-zinc-700 disabled:cursor-not-allowed text-[11px] font-medium transition-colors shrink-0 px-2 py-1 rounded-md hover:bg-white/[0.05]"
          >
            {t("share.receive.dest.change")}
          </button>
        </div>
        {suggestedName && (
          <p className="text-zinc-500 text-[11px] mt-2 pl-1 flex items-center gap-1.5">
            <FileText size={11} className="text-cyan-400 shrink-0" />
            <span className="truncate">{suggestedName}</span>
          </p>
        )}
      </div>

      {/* Steps — visible during receive, otherwise hidden */}
      {stepIdx >= 0 && (
        <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-4 animate-fade-in">
          <ol className="flex flex-col gap-2.5">
            {STEPS.map((step, i) => {
              const done = i < stepIdx || (i === STEPS.length - 1 && stepIdx === STEPS.length - 1 && !stepErr);
              const active = i === stepIdx && !stepErr;
              const errored = stepErr && i === stepIdx;
              return (
                <li key={i} className="flex items-center gap-2.5">
                  <span
                    className={
                      "w-6 h-6 rounded-full border-2 flex items-center justify-center text-[11px] font-bold shrink-0 transition-colors " +
                      (errored
                        ? "border-rose-500 bg-rose-500/15 text-rose-300"
                        : done
                          ? "border-emerald-500 bg-emerald-500/15 text-emerald-300"
                          : active
                            ? "border-cyan-500 bg-cyan-500/15 text-cyan-300"
                            : "border-white/[0.10] bg-white/[0.04] text-zinc-500")
                    }
                  >
                    {done ? <Check size={12} /> : errored ? <AlertCircle size={12} /> : i + 1}
                  </span>
                  <span
                    className={
                      "text-[12.5px] transition-colors " +
                      (done ? "text-emerald-300" : errored ? "text-rose-300" : active ? "text-white" : "text-zinc-500")
                    }
                  >
                    {step}
                  </span>
                </li>
              );
            })}
          </ol>
          {/* Linear progress bar */}
          <div className="mt-3 h-1 bg-white/[0.04] rounded-full overflow-hidden">
            <div
              className={
                "h-full transition-all duration-500 " +
                (stepErr ? "bg-rose-500" : "bg-gradient-to-r from-cyan-400 to-emerald-400")
              }
              style={{ width: `${stepErr ? 100 : Math.max(0, ((stepIdx + 1) / STEPS.length) * 100)}%` }}
            />
          </div>
        </div>
      )}

      {/* Big CTA */}
      <button
        onClick={onReceive}
        disabled={!token || busy || kind === "unknown"}
        className="w-full py-3.5 rounded-2xl bg-blue-500 hover:bg-blue-400 disabled:bg-zinc-800 disabled:text-zinc-600 text-white text-[14px] font-semibold transition-all duration-150 hover:-translate-y-px disabled:hover:translate-y-0 inline-flex items-center justify-center gap-2 shadow-[0_4px_20px_-6px_rgba(59,130,246,0.55)] disabled:shadow-none"
      >
        {busy ? (
          <>
            <Loader2 size={16} className="animate-spin" />
            {t("share.receive.btn.busy")}
          </>
        ) : (
          <>
            <Download size={16} />
            {t("share.receive.btn")}
          </>
        )}
      </button>

      {/* Result — show after success */}
      {result && (
        <div className="rounded-xl bg-emerald-500/[0.08] border border-emerald-500/30 p-3 flex items-center gap-3 animate-scale-in">
          <div className="w-9 h-9 rounded-lg bg-emerald-500/15 border border-emerald-500/30 flex items-center justify-center text-emerald-300 shrink-0">
            <Check size={16} />
          </div>
          <div className="flex-1 min-w-0">
            <p className="text-emerald-100 text-[12.5px] font-medium">
              {t("share.receive.done.title")}
            </p>
            <p className="text-emerald-400/70 text-[10.5px] mt-0.5 truncate font-mono">
              {result.output_path}
            </p>
          </div>
          <button
            onClick={async () => {
              try {
                await invoke("reveal_in_finder_cmd", { path: result.output_path });
              } catch {}
            }}
            className="text-emerald-300 hover:text-emerald-100 text-[11px] underline shrink-0"
          >
            {t("share.receive.done.reveal")}
          </button>
        </div>
      )}
    </div>
  );
}

// ─────────────────────────────────────────────────────────────
//  Link panel
// ─────────────────────────────────────────────────────────────

function LinkPanel({
  resp,
  stats,
  recentShareEvents,
  locale,
  t,
  onNavigate,
}: {
  resp: SendStartResp | null;
  stats: import("@/lib/useAppData").AppStats | null;
  recentShareEvents: import("@/lib/useAppData").RecentOp[];
  locale: string;
  t: (k: string, vars?: Record<string, string | number>) => string;
  onNavigate?: (v: View) => void;
}) {
  const [copied, setCopied] = useState<"link" | "token" | "all" | null>(null);
  const [showQR, setShowQR] = useState(true);
  const [showToken, setShowToken] = useState(false);

  const linkUrl = resp
    ? `nexusrar://d/${resp.code.toLowerCase()}`
    : null;

  // Generic copy with toast + visual feedback.
  const onCopy = useCallback(async (text: string, which: "link" | "token" | "all") => {
    try { await navigator.clipboard.writeText(text); } catch {}
    setCopied(which);
    setTimeout(() => setCopied(null), 1800);
    toast.success(which === "all" ? "Link + token copiados" : "Copiado");
  }, []);

  // One-click share via OS share-sheet if available, else fall back to copy.
  const onShareViaOS = useCallback(async () => {
    if (!resp) return;
    const payload = linkUrl ?? resp.token;
    const data = {
      title: `NexusRAR — ${resp.filename}`,
      text: linkUrl ?? "",
      url: linkUrl ?? undefined,
    };
    if ((navigator as any).share) {
      try { await (navigator as any).share(data); return; } catch {}
    }
    if ((navigator as any).clipboard?.writeText) {
      await (navigator as any).clipboard.writeText(payload);
      toast.success("Copiado al portapapeles");
    } else {
      toast.error("No se pudo compartir");
    }
  }, [resp, linkUrl]);

  // Pre-compute "copy all" payload.
  const copyAllText = useMemo(() => {
    if (!resp) return "";
    return [linkUrl, resp.token].filter(Boolean).join("\n");
  }, [resp, linkUrl]);

  return (
    <div className="glass rounded-3xl p-6 flex flex-col gap-5">
      {/* Step header with status pill */}
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-center gap-2.5">
          <div className="w-8 h-8 rounded-lg bg-emerald-500/15 border border-emerald-500/30 flex items-center justify-center text-emerald-300 text-[13px] font-bold">
            2
          </div>
          <div>
            <h2 className="text-white text-[15px] font-semibold leading-tight">
              {t("share.link.title")}
            </h2>
            <p className="text-zinc-500 text-[11.5px] mt-0.5">
              {t("share.link.desc")}
            </p>
          </div>
        </div>
        {resp && (
          <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-emerald-500/15 border border-emerald-500/30 text-emerald-300 text-[10.5px] tracking-wider uppercase font-semibold shrink-0">
            <span className="relative flex w-2 h-2">
              <span className="absolute inline-flex h-full w-full rounded-full opacity-75 animate-ping bg-emerald-400" />
              <span className="relative inline-flex rounded-full h-2 w-2 bg-emerald-400" />
            </span>
            {t("share.link.active")}
          </span>
        )}
      </div>

      {resp && linkUrl ? (
        <>
          {/* QR code — DEFAULT ON at 220x220 px. Toggle to hide. */}
          {showQR && (
            <div className="rounded-2xl bg-white p-5 flex flex-col items-center gap-3 animate-scale-in">
              <div className="rounded-xl bg-white p-2.5 shadow-inner">
                <QRCodeSVG
                  value={linkUrl}
                  size={220}
                  level="M"
                  bgColor="#ffffff"
                  fgColor="#0a0a0c"
                />
              </div>
              <p className="text-zinc-700 text-[12px] font-mono text-center">
                {t("share.link.qr.scan")}
              </p>
              <p className="text-zinc-500 text-[10.5px] uppercase tracking-wider">
                {linkUrl}
              </p>
            </div>
          )}

          {/* Action row: 4 buttons. Compact + clear. */}
          <div className="grid grid-cols-2 gap-2">
            <button
              onClick={onShareViaOS}
              className="inline-flex items-center justify-center gap-2 px-4 py-2.5 rounded-xl bg-gradient-to-br from-cyan-500 to-emerald-500 hover:from-cyan-400 hover:to-emerald-400 text-white text-[13px] font-semibold transition-all duration-150 hover:-translate-y-px shadow-[0_4px_20px_-6px_rgba(34,211,238,0.55)]"
              title={t("share.link.share.os")}
            >
              <Share2 size={14} />
              {t("share.link.share.os")}
            </button>
            <button
              onClick={() => onCopy(linkUrl, "link")}
              className={
                "inline-flex items-center justify-center gap-2 px-4 py-2.5 rounded-xl text-[13px] font-semibold transition-all duration-150 " +
                (copied === "link"
                  ? "bg-emerald-500/20 text-emerald-300"
                  : "bg-blue-500 hover:bg-blue-400 text-white shadow-[0_4px_16px_-4px_rgba(59,130,246,0.5)]")
              }
            >
              {copied === "link" ? (
                <>
                  <Check size={14} />
                  {t("share.link.copied")}
                </>
              ) : (
                <>
                  <LinkIcon size={14} />
                  {t("share.link.copy")}
                </>
              )}
            </button>
            <button
              onClick={() => onCopy(resp.token, "token")}
              className={
                "inline-flex items-center justify-center gap-2 px-4 py-2.5 rounded-xl text-[13px] font-semibold transition-all duration-150 " +
                (copied === "token"
                  ? "bg-emerald-500/20 text-emerald-300"
                  : "bg-cyan-500/15 hover:bg-cyan-500/25 text-cyan-200 border border-cyan-500/30")
              }
            >
              {copied === "token" ? (
                <>
                  <Check size={14} />
                  {t("share.link.copied")}
                </>
              ) : (
                <>
                  <KeyRound size={14} />
                  {t("share.link.copy.token")}
                </>
              )}
            </button>
            <button
              onClick={() => setShowQR((v) => !v)}
              className={
                "inline-flex items-center justify-center gap-2 px-4 py-2.5 rounded-xl text-[13px] font-medium transition-all duration-150 " +
                (showQR ? "bg-white/[0.08] text-white border border-white/[0.16]" : "bg-white/[0.04] text-zinc-400 hover:text-white border border-white/[0.06]")
              }
              title={t("share.link.qr.toggle")}
            >
              <QrCode size={14} />
              {showQR ? t("share.link.qr.hide") : t("share.link.qr.show")}
            </button>
          </div>

          {/* Copy-all summary */}
          <button
            onClick={() => onCopy(copyAllText, "all")}
            className={
              "inline-flex items-center justify-between gap-2 px-3.5 py-2 rounded-xl text-[11.5px] font-medium transition-colors " +
              (copied === "all"
                ? "bg-emerald-500/15 text-emerald-200"
                : "bg-white/[0.03] hover:bg-white/[0.05] text-zinc-400 hover:text-zinc-200 border border-white/[0.06]")
            }
          >
            <span className="flex items-center gap-2">
              <Copy size={11} />
              {t("share.link.copy.all")}
            </span>
            {copied === "all" && <Check size={11} className="text-emerald-400" />}
          </button>

          {/* Token display — collapsed by default, expand to copy manually */}
          <details className="rounded-2xl bg-cyan-500/[0.04] border border-cyan-500/20 overflow-hidden group">
            <summary className="flex items-center gap-2 px-3 py-2 cursor-pointer list-none select-none hover:bg-white/[0.03] transition-colors">
              <KeyRound size={11} className="text-cyan-400 shrink-0" />
              <span className="text-cyan-300 text-[10px] tracking-[0.2em] uppercase font-medium">
                {t("share.link.token.label")}
              </span>
              <span className="ml-auto text-zinc-500 text-[10px] flex items-center gap-1">
                <ChevronDown size={11} className="transition-transform duration-200 group-open:rotate-180" />
                {showToken ? t("share.link.token.hide") : t("share.link.token.show")}
              </span>
            </summary>
            <div className="px-3 pb-3 pt-1">
              <p className="text-white text-[11.5px] font-mono break-all bg-black/20 rounded-lg p-2">
                {resp.token}
              </p>
            </div>
          </details>

          {/* Feature pills row */}
          <div className="grid grid-cols-3 gap-2">
            <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3 flex flex-col items-center gap-1.5 text-center">
              <Clock size={16} className="text-amber-400" />
              <p className="text-zinc-300 text-[11px] leading-tight">
                {t("share.link.feature.expires")}
              </p>
            </div>
            <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3 flex flex-col items-center gap-1.5 text-center">
              <InfinityIcon size={16} className="text-cyan-400" />
              <p className="text-zinc-300 text-[11px] leading-tight">
                {t("share.link.feature.unlimited")}
              </p>
            </div>
            <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3 flex flex-col items-center gap-1.5 text-center">
              <Shield size={16} className="text-emerald-400" />
              <p className="text-zinc-300 text-[11px] leading-tight">
                {t("share.link.feature.e2e")}
              </p>
            </div>
          </div>

          {/* Vista previa del archivo */}
          <div>
            <h3 className="text-zinc-400 text-[10px] tracking-[0.2em] uppercase font-medium mb-3">
              {t("share.link.preview")}
            </h3>
            <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3 flex items-center gap-3">
              <div className="w-10 h-10 rounded-lg bg-rose-500/15 border border-rose-500/30 flex items-center justify-center text-rose-300 shrink-0">
                <FileText size={18} />
              </div>
              <div className="flex-1 min-w-0">
                <p className="text-white text-[13px] truncate font-medium">
                  {resp.filename}
                </p>
                <p className="text-zinc-500 text-[10.5px] font-mono mt-0.5">
                  {prettyBytes(resp.file_size)} · {t("share.link.preview.doc")}
                </p>
              </div>
            </div>
          </div>

          {/* Actividad — recent past transfers */}
          <div>
            <div className="flex items-center justify-between mb-3">
              <h3 className="text-zinc-400 text-[10px] tracking-[0.2em] uppercase font-medium">
                {t("share.link.activity")}
              </h3>
              <button
                onClick={() => onNavigate && onNavigate("recent")}
                className="text-zinc-500 hover:text-cyan-400 text-[10.5px] transition-colors flex items-center gap-1"
              >
                {t("share.link.activity.seeall")}
                <ArrowRight size={10} />
              </button>
            </div>
            {recentShareEvents.length === 0 ? (
              <div className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-5 flex items-center gap-3">
                <div className="w-9 h-9 rounded-full bg-white/[0.04] flex items-center justify-center text-zinc-500 shrink-0">
                  <Clock size={16} />
                </div>
                <div>
                  <p className="text-zinc-300 text-[12.5px] font-medium">
                    {t("share.link.activity.empty")}
                  </p>
                  <p className="text-zinc-600 text-[11px] mt-0.5">
                    {t("share.link.activity.hint")}
                  </p>
                </div>
              </div>
            ) : (
              <div className="space-y-2">
                {recentShareEvents.map((ev) => (
                  <div
                    key={ev.id}
                    className="rounded-xl bg-white/[0.02] border border-white/[0.06] p-3 flex items-center gap-3 hover:bg-white/[0.04] transition-colors"
                  >
                    <div className="w-9 h-9 rounded-lg bg-emerald-500/10 border border-emerald-500/30 flex items-center justify-center text-emerald-400 shrink-0">
                      <Send size={14} />
                    </div>
                    <div className="flex-1 min-w-0">
                      <p className="text-zinc-200 text-[12.5px] truncate font-medium">
                        {ev.filename}
                      </p>
                      <p className="text-zinc-500 text-[10.5px] mt-0.5 font-mono">
                        {formatBytes(ev.originalBytes, locale)}
                      </p>
                    </div>
                    <span className="text-zinc-500 text-[10.5px] shrink-0">
                      {formatTimestampMs(ev.timestamp, t)}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </div>
        </>
      ) : (
        <div className="rounded-2xl border-2 border-dashed border-white/[0.08] p-10 flex flex-col items-center text-center gap-3 min-h-[280px] justify-center">
          <div className="w-14 h-14 rounded-2xl bg-white/[0.04] flex items-center justify-center text-zinc-500">
            <LinkIcon size={26} strokeWidth={1.5} />
          </div>
          <div>
            <p className="text-zinc-400 text-[13.5px] font-medium mb-1">
              {t("share.link.empty.title")}
            </p>
            <p className="text-zinc-600 text-[12px] max-w-xs mx-auto leading-relaxed">
              {t("share.link.empty.desc")}
            </p>
          </div>
        </div>
      )}
    </div>
  );
}


function friendlyError(msg: string): string {
  if (msg.includes("mDNS") || msg.includes("no service")) return "No se encontró el dispositivo — asegúrate de estar en la misma Wi-Fi.";
  if (msg.includes("timeout") || msg.includes("timed out")) return "Tiempo de espera agotado — verifica que el otro dispositivo siga activo.";
  if (msg.includes("authentication") || msg.includes("wrong code")) return "Código incorrecto — copia el token completo del remitente.";
  if (msg.includes("SHA-256") || msg.includes("corrupted")) return "Archivo corrupto durante la transferencia — intenta de nuevo.";
  return msg;
}