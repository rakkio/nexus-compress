"use client";

import { useEffect, useState } from "react";
import { useLocale } from "@/components/LocaleProvider";

// ─── Types (still exported so SaveTarget / CompressView can import) ───

// Sprint 5.7.x — removed the Strength type and the MODES / ModeMeta
// metadata that drove the now-deleted "default compression mode"
// cards in Settings. The Compress screen is where the user picks
// the per-compression mode (Rapido/Balanceado/Ultra) — duplicating
// the choice in Settings was dead surface area.
export type Mode = "v4" | "v5-min" | "v6-solid";

// ─── Tauri invoke helper ─────────────────────────────────────────

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

// ─── Tunnel mode metadata ────────────────────────────────────────

interface TunnelModeMeta {
  id: "quick" | "named" | "direct";
  icon: string;
  titleKey: "settings.tunnel.mode.quick" | "settings.tunnel.mode.named" | "settings.tunnel.mode.direct";
  descKey: "settings.tunnel.mode.quick.desc" | "settings.tunnel.mode.named.desc" | "settings.tunnel.mode.direct.desc";
  badgeKey: "settings.tunnel.mode.quick.badge" | "settings.tunnel.mode.named.badge" | "settings.tunnel.mode.direct.badge";
  badgeColor: string;
}

const TUNNEL_MODES: TunnelModeMeta[] = [
  {
    id: "quick",
    icon: "🚀",
    titleKey: "settings.tunnel.mode.quick",
    descKey: "settings.tunnel.mode.quick.desc",
    badgeKey: "settings.tunnel.mode.quick.badge",
    badgeColor: "text-amber-300 bg-amber-500/10 border-amber-500/30",
  },
  {
    id: "named",
    icon: "☁️",
    titleKey: "settings.tunnel.mode.named",
    descKey: "settings.tunnel.mode.named.desc",
    badgeKey: "settings.tunnel.mode.named.badge",
    badgeColor: "text-violet-300 bg-violet-500/10 border-violet-500/30",
  },
  {
    id: "direct",
    icon: "📡",
    titleKey: "settings.tunnel.mode.direct",
    descKey: "settings.tunnel.mode.direct.desc",
    badgeKey: "settings.tunnel.mode.direct.badge",
    badgeColor: "text-emerald-300 bg-emerald-500/10 border-emerald-500/30",
  },
];

// ─── Section wrapper ─────────────────────────────────────────────

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div className="flex items-baseline justify-between mb-3 px-1 gap-4">
        <p className="text-zinc-500 text-[11px] tracking-[0.2em] uppercase">
          {title}
        </p>
        {hint && (
          <p className="text-zinc-600 text-[11px] leading-snug text-right flex-1 max-w-md">
            {hint}
          </p>
        )}
      </div>
      <div className="rounded-2xl bg-white/[0.03] border border-white/[0.06] overflow-hidden divide-y divide-white/[0.04]">
        {children}
      </div>
    </div>
  );
}

// ─── Language panel ─────────────────────────────────────────────

export function LanguagePanel() {
  const { locale, setLocale, t } = useLocale();
  return (
    <Section
      title={t("settings.section.language")}
      hint="Cambia el idioma de toda la interfaz."
    >
      <div className="px-5 py-4 flex items-center gap-3 flex-wrap">
        {(["es", "en", "it"] as const).map((lang) => {
          const active = locale === lang;
          const flag = lang === "es" ? "🇪🇸" : lang === "en" ? "🇬🇧" : "🇮🇹";
          const langLabel =
            lang === "es"
              ? t("settings.language.es")
              : lang === "en"
                ? t("settings.language.en")
                : t("settings.language.it");
          return (
            <button
              key={lang}
              onClick={() => setLocale(lang)}
              className={`px-6 py-2.5 rounded-xl text-[13px] font-medium transition-all ${
                active
                  ? "bg-cyan-500/20 border border-cyan-500/40 text-cyan-300 shadow-sm shadow-cyan-500/10"
                  : "bg-white/[0.04] border border-white/[0.08] text-zinc-400 hover:text-white hover:border-white/[0.18]"
              }`}
            >
              {flag}  {langLabel}
            </button>
          );
        })}
      </div>
    </Section>
  );
}

// ─── Compression panel ──────────────────────────────────────────
//
// Sprint 5.7.x — the per-compression mode/strength selectors used
// to live here too, but CompressView already exposes them on the
// compression screen itself (where the choice is actually made per-
// file). Two settings on the same choice was duplicate surface
// area; we removed it.
//
// What's left here is the engine **self-test** — a one-click
// diagnostic that exercises every backend end-to-end and reports
// "the engine works on your machine, with the codecs we ship".
// Useful when something looks off (mysterious crashes, weird
// ratio numbers) but not a default-toggle.

export function CompressionPanel({
  onSelfTest,
}: {
  onSelfTest?: () => void;
} = {}) {
  const { t } = useLocale();

  const [selfTestStatus, setSelfTestStatus] = useState<"idle" | "busy" | "ok" | "err">("idle");
  const [selfTestMsg, setSelfTestMsg] = useState("");

  const handleSelfTest = async () => {
    setSelfTestStatus("busy");
    setSelfTestMsg("");
    try {
      if (onSelfTest) await onSelfTest();
      else await tauriInvoke("self_test_cmd", {});
      setSelfTestStatus("ok");
      setSelfTestMsg(t("settings.compression.selftest.ok"));
    } catch (e: any) {
      setSelfTestStatus("err");
      setSelfTestMsg(e?.message ?? String(e));
    }
  };

  return (
    <Section
      title={t("settings.section.compression")}
      hint={t("settings.compression.engineHint")}
    >
      {/* Engine info card — replaces the mode/strength cards that
          used to live here. Shows what the engine is doing per-
          compression without letting the user toggle a "default"
          (which is meaningless — CompressView has the live picker). */}
      <div className="p-5 space-y-3">
        <div className="flex items-baseline justify-between px-1">
          <span className="text-zinc-500 text-[11px] tracking-[0.15em] uppercase">
            {t("settings.compression.default")}
          </span>
          <span className="text-white text-[13px] font-mono font-medium">
            {t("settings.compression.default.value")}
          </span>
        </div>
        <div className="flex items-baseline justify-between px-1">
          <span className="text-zinc-500 text-[11px] tracking-[0.15em] uppercase">
            {t("settings.compression.dict")}
          </span>
          <span className="text-white text-[13px] font-mono font-medium">
            {t("settings.compression.dict.value")}
          </span>
        </div>
        <p className="text-zinc-500 text-[11.5px] leading-relaxed pt-2 border-t border-white/[0.04] mt-2">
          {t("settings.compression.perFileHint")}
        </p>
      </div>

      {/* Self-test */}
      <div className="px-5 pb-5 pt-2">
        <button
          onClick={handleSelfTest}
          disabled={selfTestStatus === "busy"}
          className={`w-full py-2.5 rounded-xl text-[13px] font-medium transition-all border ${
            selfTestStatus === "ok"
              ? "bg-emerald-500/10 border-emerald-500/30 text-emerald-300"
              : selfTestStatus === "err"
                ? "bg-red-500/10 border-red-500/30 text-red-400"
                : "bg-white/[0.04] border-white/[0.08] text-zinc-300 hover:text-white hover:border-white/[0.18]"
          } disabled:opacity-50`}
        >
          {selfTestStatus === "busy"
            ? t("settings.compression.selftest.busy")
            : selfTestMsg || t("settings.compression.selftest")}
        </button>
      </div>
    </Section>
  );
}

// ─── Tunnel panel ───────────────────────────────────────────────

export function TunnelPanel() {
  const { t } = useLocale();
  const [mode, setMode] = useState<"quick" | "named" | "direct">("quick");
  const [hostname, setHostname] = useState("");
  const [token, setToken] = useState("");
  const [hasToken, setHasToken] = useState(false);
  const [status, setStatus] = useState<"" | "ok" | "err">("");
  const [statusMsg, setStatusMsg] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    tauriInvoke<{ mode: string; hostname: string | null; has_token: boolean }>(
      "p2p_get_tunnel_config_cmd"
    )
      .then((r) => {
        setMode(r.mode as "quick" | "named" | "direct");
        setHostname(r.hostname ?? "");
        setHasToken(r.has_token);
      })
      .catch(() => {});
  }, []);

  const onSave = async () => {
    setBusy(true);
    setStatus("");
    setStatusMsg("");
    try {
      await tauriInvoke("p2p_save_tunnel_config_cmd", {
        req: {
          mode,
          hostname: mode === "named" ? hostname : null,
          token: token || null,
        },
      });
      const r = await tauriInvoke<{ has_token: boolean }>("p2p_get_tunnel_config_cmd");
      setHasToken(r.has_token);
      setToken("");
      setStatus("ok");
      setStatusMsg("✓ " + t("settings.tunnel.save"));
      setTimeout(() => {
        setStatus("");
        setStatusMsg("");
      }, 3000);
    } catch (e: any) {
      setStatus("err");
      setStatusMsg(`✗ ${e?.message ?? e}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Section
      title={t("settings.section.tunnel")}
      hint="Cómo se establece la conexión entre dispositivos al enviar un archivo."
    >
      <div className="px-5 py-4">
        <label className="text-zinc-500 text-[11px] tracking-[0.15em] uppercase block mb-3">
          {t("settings.tunnel.mode")}
        </label>
        <div className="flex flex-col gap-2">
          {TUNNEL_MODES.map((m) => {
            const active = mode === m.id;
            const disabled = m.id === "direct";
            return (
              <button
                key={m.id}
                onClick={() => !disabled && setMode(m.id)}
                disabled={disabled}
                className={`flex items-start gap-3 px-4 py-3 rounded-xl border text-left text-[13px] transition-all ${
                  active && !disabled
                    ? "bg-cyan-500/10 border-cyan-500/40 text-white"
                    : disabled
                      ? "bg-white/[0.02] border-white/[0.04] text-zinc-700 cursor-not-allowed"
                      : "bg-white/[0.03] border-white/[0.06] text-zinc-400 hover:text-white hover:border-white/[0.14]"
                }`}
              >
                <span className="text-2xl shrink-0 mt-0.5">{m.icon}</span>
                <div className="flex-1 min-w-0">
                  <div className="flex items-center gap-2 mb-1">
                    <span className="font-semibold">{t(m.titleKey)}</span>
                    <span
                      className={`px-1.5 py-0.5 rounded text-[9.5px] font-mono tracking-wider border ${m.badgeColor}`}
                    >
                      {t(m.badgeKey)}
                    </span>
                  </div>
                  <p className="text-zinc-500 text-[11.5px] leading-relaxed">
                    {t(m.descKey)}
                  </p>
                </div>
                {active && !disabled && (
                  <span className="w-2 h-2 rounded-full shrink-0 mt-1.5 bg-cyan-400" />
                )}
              </button>
            );
          })}
        </div>
      </div>

      {mode === "named" && (
        <>
          <div className="px-5 py-4">
            <label className="text-zinc-500 text-[11px] tracking-[0.15em] uppercase block mb-2">
              {t("settings.tunnel.hostname")}
            </label>
            <input
              type="text"
              value={hostname}
              onChange={(e) => setHostname(e.target.value)}
              placeholder="p2p.example.com"
              disabled={busy}
              className="w-full bg-white/[0.04] border border-white/[0.08] rounded-xl px-4 py-2.5 text-[13px] text-white placeholder-zinc-600 focus:outline-none focus:border-cyan-500/40 font-mono"
            />
          </div>
          <div className="px-5 py-4">
            <label className="text-zinc-500 text-[11px] tracking-[0.15em] uppercase block mb-2 flex items-center gap-2">
              {t("settings.tunnel.token")}
              {hasToken && (
                <span className="ml-2 inline-flex items-center gap-1 text-emerald-400 text-[10px] normal-case tracking-normal font-medium">
                  <span className="w-1.5 h-1.5 rounded-full bg-emerald-400" />
                  {t("settings.tunnel.token.saved")}
                </span>
              )}
            </label>
            <input
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder={
                hasToken ? t("settings.tunnel.token.replace") : t("settings.tunnel.token.new")
              }
              disabled={busy}
              autoComplete="off"
              className="w-full bg-white/[0.04] border border-white/[0.08] rounded-xl px-4 py-2.5 text-[13px] text-white placeholder-zinc-600 focus:outline-none focus:border-cyan-500/40 font-mono"
            />
          </div>
        </>
      )}

      <div className="px-5 py-4 flex items-center gap-3">
        <button
          onClick={onSave}
          disabled={busy || (mode === "named" && !hostname)}
          className={`flex-1 py-2.5 rounded-xl text-[13px] font-medium transition-all border ${
            status === "ok"
              ? "bg-emerald-500/20 border-emerald-500/40 text-emerald-300"
              : "bg-cyan-500/20 border border-cyan-500/30 text-cyan-300 hover:bg-cyan-500/30 hover:border-cyan-500/50"
          } disabled:opacity-40 disabled:cursor-not-allowed`}
        >
          {busy
            ? t("settings.tunnel.save.busy")
            : status === "ok"
              ? statusMsg
              : t("settings.tunnel.save")}
        </button>
        {status === "err" && statusMsg && (
          <span className="text-[12px] font-mono shrink-0 text-red-400">{statusMsg}</span>
        )}
      </div>

      <div className="px-5 py-4 bg-white/[0.01]">
        <p className="text-zinc-600 text-[12px] leading-relaxed">
          {t("settings.tunnel.note")}
        </p>
      </div>
    </Section>
  );
}

// ─── About panel ────────────────────────────────────────────────

function InfoRow({
  label,
  value,
  mono,
  href,
}: {
  label: string;
  value: string;
  mono?: boolean;
  href?: string;
}) {
  const content = (
    <>
      <span className="text-zinc-400 text-[13px]">{label}</span>
      <span
        className={`text-white text-[13px] ${mono ? "font-mono" : "font-medium"} ${
          href ? "hover:text-cyan-300 transition-colors" : ""
        }`}
      >
        {value}
      </span>
    </>
  );
  return (
    <div className="px-5 py-3.5 flex items-center justify-between">
      {href ? (
        <a
          href={href}
          target="_blank"
          rel="noopener noreferrer"
          className="flex items-center justify-between w-full"
        >
          {content}
        </a>
      ) : (
        <div className="flex items-center justify-between w-full">{content}</div>
      )}
    </div>
  );
}

export function AboutPanel() {
  const { t, locale } = useLocale();
  return (
    <Section
      title={t("settings.section.about")}
      hint="Información técnica y enlaces."
    >
      <InfoRow label={t("settings.about.version")} value="0.1.2" mono />
      <InfoRow label={t("settings.about.engine")} value="NexusCompress v6 Solid-AST" mono />
      <InfoRow
        label="GitHub"
        value="github.com/rakkio/nexus-compress"
        mono
        href="https://github.com/rakkio/nexus-compress"
      />
      <InfoRow label="Localización" value={locale.toUpperCase()} mono />
    </Section>
  );
}

// ─── Composite (legacy) ─────────────────────────────────────────
// Used by anywhere that still wants all settings in one column.
// The new SettingsView uses the individual panels above instead.

export function ConfigPanel({
  onSelfTest,
}: {
  onSelfTest?: () => void;
} = {}) {
  return (
    <div className="space-y-7">
      <LanguagePanel />
      <CompressionPanel onSelfTest={onSelfTest} />
      <TunnelPanel />
      <AboutPanel />
    </div>
  );
}
