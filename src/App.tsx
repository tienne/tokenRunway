import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import { openUrl } from "@tauri-apps/plugin-opener";
import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
import { t, resolveLang, type Lang } from "./i18n";
import "./App.css";

interface RunwayStatus {
  tool: string;
  available: boolean;
  unit: string; // "tokens" | "requests"
  windowHours: number;
  windowUsage: number;
  dailyUsage: number;
  dailyCount: number;
  dailyCost: number;
  cacheHitRate: number | null;
  insight: string | null;
  sparkline: number[];
  limit: number | null;
  percentRemaining: number | null;
  burnRatePerMin: number;
  burnTrend: string; // "up" | "down" | "flat"
  etaMinutes: number | null;
  resetsAt: string | null;
  sevenDayRemaining: number | null;
  plan: string | null;
  note: string | null;
}

interface Settings {
  alertThreshold: number;
  etaAlertMinutes: number;
  resetAlertMinutes: number;
  pollSeconds: number;
  disabledTools: string[];
  trayTool: string | null;
  notificationsEnabled: boolean;
  language: string | null;
  quietEnabled: boolean;
  quietStartHour: number;
  quietEndHour: number;
  analyticsEnabled: boolean;
  hideInactive: boolean;
}

interface ToolInfo {
  tool: string;
  available: boolean;
}

function formatAmount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return `${n}`;
}

function unitLabel(unit: string): string {
  return unit === "requests" ? "req" : "tok";
}

function windowLabel(hours: number): string {
  return Number.isInteger(hours) ? `${hours}h` : `${hours.toFixed(1)}h`;
}

function formatEta(min: number | null, lang: Lang): string {
  if (min == null) return "—";
  if (min < 1) return t(lang, "lessThanMin");
  const h = Math.floor(min / 60);
  const m = Math.round(min % 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** 구간별 사용량 미니 막대 스파크라인 */
function Sparkline({ data }: { data: number[] }) {
  if (!data.length || data.every((v) => v === 0)) return null;
  const max = Math.max(...data, 1);
  return (
    <div className="spark" aria-hidden>
      {data.map((v, i) => (
        <span key={i} style={{ height: `${Math.max(6, (v / max) * 100)}%` }} />
      ))}
    </div>
  );
}

function formatUpdated(ts: number, lang: Lang): string {
  const diffSec = Math.round((Date.now() - ts) / 1000);
  if (diffSec < 60) return t(lang, "updatedJustNow");
  const m = Math.floor(diffSec / 60);
  return t(lang, "updatedAgo", { t: `${m}m` });
}

function formatResetsAt(iso: string | null, lang: Lang): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (isNaN(d.getTime())) return null;
  const diffMin = Math.round((d.getTime() - Date.now()) / 60000);
  if (diffMin <= 0) return t(lang, "resetSoon");
  const h = Math.floor(diffMin / 60);
  const m = diffMin % 60;
  const time = h > 0 ? `${h}h ${m}m` : `${m}m`;
  return t(lang, "afterReset", { t: time });
}

/** 트레이 팝오버 — 런웨이 대시보드 */
function Dashboard() {
  const [statuses, setStatuses] = useState<RunwayStatus[]>([]);
  const [loading, setLoading] = useState(true);
  const [pollSeconds, setPollSeconds] = useState(30);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);

  async function refresh() {
    try {
      const data = await invoke<RunwayStatus[]>("get_runway");
      setStatuses(data);
      setLastUpdated(Date.now());
    } finally {
      setLoading(false);
    }
  }

  async function reloadSettings() {
    try {
      const s = await invoke<Settings>("get_settings");
      setPollSeconds(s.pollSeconds);
      setLang(resolveLang(s.language));
    } catch {
      /* noop */
    }
  }

  useEffect(() => {
    refresh();
    reloadSettings();
    // 설정 창에서 변경하면 즉시 반영 (폴링 기다리지 않음)
    const unlisten = listen("settings-changed", () => {
      reloadSettings();
      refresh();
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 콘텐츠 너비에 맞춰 팝오버 창 너비 자동 조정 (텍스트가 줄바꿈되지 않게)
  useEffect(() => {
    const ro = new ResizeObserver(() => {
      // 줄바꿈 없이 필요한 너비(scrollWidth) + 높이
      const w = Math.ceil(document.documentElement.scrollWidth);
      const h = Math.ceil(document.documentElement.scrollHeight);
      const win = getCurrentWindow();
      win
        .setSize(
          new LogicalSize(
            Math.min(Math.max(w, 360), 560), // 360~560 범위
            Math.min(Math.max(h, 160), 760)
          )
        )
        .catch(() => {});
    });
    ro.observe(document.body);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const id = setInterval(async () => {
      refresh();
      try {
        const s = await invoke<Settings>("get_settings");
        if (s.pollSeconds !== pollSeconds) setPollSeconds(s.pollSeconds);
        setLang(resolveLang(s.language));
      } catch {
        /* noop */
      }
    }, Math.max(5, pollSeconds) * 1000);
    return () => clearInterval(id);
  }, [pollSeconds]);

  return (
    <main className="container">
      <header>
        <h1>🛬 Token Runway</h1>
        <div className="header-actions">
          <button onClick={refresh} className="refresh">
            {t(lang, "refresh")}
          </button>
          <button
            onClick={() => invoke("open_history_window")}
            className="icon-btn"
            title={t(lang, "history")}
          >
            📅
          </button>
          <button
            onClick={() => invoke("open_settings_window")}
            className="icon-btn"
            title={t(lang, "settings")}
          >
            ⚙️
          </button>
        </div>
      </header>

      {loading && <p className="muted">{t(lang, "loading")}</p>}
      {!loading && statuses.length === 0 && (
        <p className="muted">{t(lang, "noTools")}</p>
      )}

      {statuses.map((s) => (
        <section className="card" key={s.tool}>
          <div className="card-head">
            <span className="tool">
              {s.tool}
              {s.plan && <span className="plan-badge">{s.plan}</span>}
            </span>
            <span className="pct">
              {s.percentRemaining != null ? (
                <>
                  {s.percentRemaining.toFixed(0)}%
                  <span className="pct-label"> {t(lang, "remaining")}</span>
                </>
              ) : (
                t(lang, "noLimit")
              )}
            </span>
          </div>

          {s.percentRemaining != null && (
            <div className="bar">
              <div
                className="bar-fill"
                style={{
                  width: `${Math.max(0, Math.min(100, s.percentRemaining))}%`,
                }}
              />
            </div>
          )}

          {formatResetsAt(s.resetsAt, lang) && (
            <p className="resets">⏱ {formatResetsAt(s.resetsAt, lang)}</p>
          )}

          <div className="today-section">
            <span className="today-title">
              {t(lang, "session")} ({windowLabel(s.windowHours)})
            </span>
            <div className="today">
              <div className="today-item">
                <span className="today-val">{formatAmount(s.windowUsage)}</span>
                <span className="today-label">{t(lang, "used")}</span>
              </div>
              <div className="today-item">
                <span className="today-val">
                  {formatAmount(Math.round(s.burnRatePerMin))}/min
                </span>
                <span className="today-label">
                  {t(lang, "burnRate")}
                  {s.burnTrend === "up" && <span className="trend-up"> ↑</span>}
                  {s.burnTrend === "down" && (
                    <span className="trend-down"> ↓</span>
                  )}
                  {s.burnTrend === "flat" && (
                    <span className="trend-flat"> →</span>
                  )}
                </span>
              </div>
              <div className="today-item">
                <span className="today-val">{formatEta(s.etaMinutes, lang)}</span>
                <span className="today-label">{t(lang, "eta")}</span>
              </div>
            </div>
          </div>

          <div className="today-section">
            <span className="today-title">{t(lang, "today")}</span>
            <div className="today">
              {s.unit === "requests" ? (
                <div className="today-item">
                  <span className="today-val">{s.dailyCount}</span>
                  <span className="today-label">{t(lang, "todayRequests")}</span>
                </div>
              ) : (
                <>
                  <div className="today-item">
                    <span className="today-val">
                      {formatAmount(s.dailyUsage)}
                    </span>
                    <span className="today-label">{t(lang, "todayTokens")}</span>
                  </div>
                  <div className="today-item">
                    <span className="today-val">{s.dailyCount}</span>
                    <span className="today-label">{t(lang, "todayMessages")}</span>
                  </div>
                  {s.dailyCost > 0 && (
                    <div className="today-item">
                      <span className="today-val">${s.dailyCost.toFixed(2)}</span>
                      <span className="today-label">{t(lang, "apiValue")}</span>
                    </div>
                  )}
                </>
              )}
            </div>
          </div>

          <Sparkline data={s.sparkline} />

          {s.cacheHitRate != null && (
            <p className="cache-hit">
              ⚡ {t(lang, "cacheHit", { n: s.cacheHitRate.toFixed(0) })}
            </p>
          )}

          {s.insight && <p className="insight">{t(lang, s.insight)}</p>}

          {s.sevenDayRemaining != null && (
            <p className="weekly">
              {t(lang, "weekly", { n: s.sevenDayRemaining.toFixed(0) })}
            </p>
          )}

          {s.note && (
            <p className={s.note.startsWith("error.") ? "note-error" : "note"}>
              {t(lang, s.note)}
            </p>
          )}
        </section>
      ))}

      {lastUpdated && (
        <p className="updated">{formatUpdated(lastUpdated, lang)}</p>
      )}
    </main>
  );
}

/** 설정 전용 창 */
function SettingsView() {
  const [settings, setSettings] = useState<Settings>({
    alertThreshold: 20,
    etaAlertMinutes: 30,
    resetAlertMinutes: 0,
    pollSeconds: 30,
    disabledTools: [],
    trayTool: null,
    notificationsEnabled: true,
    language: null,
    quietEnabled: false,
    quietStartHour: 22,
    quietEndHour: 8,
    analyticsEnabled: false,
    hideInactive: false,
  });
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [permGranted, setPermGranted] = useState<boolean | null>(null);
  const [autostart, setAutostart] = useState(false);

  const lang = resolveLang(settings.language);
  const tr = (k: string, p?: Record<string, string | number>) => t(lang, k, p);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings).catch(() => {});
    invoke<ToolInfo[]>("get_available_tools").then(setTools).catch(() => {});
    isPermissionGranted().then(setPermGranted).catch(() => {});
    isEnabled().then(setAutostart).catch(() => {});
    invoke("track_event", { event: "settings_opened" }).catch(() => {});
  }, []);

  async function toggleAutostart(value: boolean) {
    try {
      if (value) await enable();
      else await disable();
      setAutostart(value);
    } catch {
      /* noop */
    }
  }

  async function update(patch: Partial<Settings>) {
    const next = { ...settings, ...patch };
    setSettings(next);
    await invoke("set_settings", { settings: next });
  }

  function toggleTool(tool: string, enabled: boolean) {
    const disabled = enabled
      ? settings.disabledTools.filter((x) => x !== tool)
      : [...settings.disabledTools, tool];
    update({ disabledTools: disabled });
  }

  async function askPermission() {
    const result = await requestPermission();
    setPermGranted(result === "granted");
  }

  async function setNotifications(enabled: boolean) {
    update({ notificationsEnabled: enabled });
    if (enabled) {
      let granted = await isPermissionGranted();
      if (!granted) {
        const res = await requestPermission();
        granted = res === "granted";
      }
      setPermGranted(granted);
    }
  }

  async function openNotifSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.notifications"
    ).catch(() => {});
  }

  return (
    <main className="container settings-window">
      <section className="settings">
        <label className="setting-row toggle-row">
          <span>{tr("osNotif")}</span>
          <input
            type="checkbox"
            checked={settings.notificationsEnabled}
            onChange={(e) => setNotifications(e.target.checked)}
          />
        </label>
        {settings.notificationsEnabled && permGranted === false && (
          <p className="perm-warn">
            ⚠️ {tr("permWarn")}{" "}
            <button className="link-btn" onClick={askPermission}>
              {tr("reqPerm")}
            </button>
            {" · "}
            <button className="link-btn" onClick={openNotifSettings}>
              {tr("openSysSettings")}
            </button>
          </p>
        )}
        {settings.notificationsEnabled && permGranted === true && (
          <p className="setting-hint">{tr("permOk")}</p>
        )}

        <label className="setting-row toggle-row">
          <span>{tr("quietHours")}</span>
          <input
            type="checkbox"
            checked={settings.quietEnabled}
            onChange={(e) => update({ quietEnabled: e.target.checked })}
          />
        </label>
        {settings.quietEnabled && (
          <div className="quiet-range">
            <label>
              {tr("quietFrom")}
              <select
                value={settings.quietStartHour}
                onChange={(e) =>
                  update({ quietStartHour: Number(e.target.value) })
                }
              >
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>
                    {String(h).padStart(2, "0")}:00
                  </option>
                ))}
              </select>
            </label>
            <label>
              {tr("quietTo")}
              <select
                value={settings.quietEndHour}
                onChange={(e) =>
                  update({ quietEndHour: Number(e.target.value) })
                }
              >
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>
                    {String(h).padStart(2, "0")}:00
                  </option>
                ))}
              </select>
            </label>
          </div>
        )}

        <hr className="setting-divider" />

        <label className="setting-row">
          <span>{tr("alertThreshold")}</span>
          <span className="setting-value">
            {tr("pctRemaining", { n: settings.alertThreshold })}
          </span>
        </label>
        <input
          type="range"
          min={5}
          max={90}
          step={5}
          value={settings.alertThreshold}
          onChange={(e) => update({ alertThreshold: Number(e.target.value) })}
        />

        <label className="setting-row">
          <span>{tr("etaAlert")}</span>
          <span className="setting-value">
            {settings.etaAlertMinutes === 0
              ? tr("off")
              : tr("minBefore", { n: settings.etaAlertMinutes })}
          </span>
        </label>
        <input
          type="range"
          min={0}
          max={120}
          step={10}
          value={settings.etaAlertMinutes}
          onChange={(e) => update({ etaAlertMinutes: Number(e.target.value) })}
        />

        <label className="setting-row">
          <span>{tr("resetAlert")}</span>
          <span className="setting-value">
            {settings.resetAlertMinutes === 0
              ? tr("off")
              : tr("minBefore", { n: settings.resetAlertMinutes })}
          </span>
        </label>
        <input
          type="range"
          min={0}
          max={60}
          step={5}
          value={settings.resetAlertMinutes}
          onChange={(e) => update({ resetAlertMinutes: Number(e.target.value) })}
        />

        <label className="setting-row">
          <span>{tr("pollInterval")}</span>
          <span className="setting-value">
            {tr("sec", { n: settings.pollSeconds })}
          </span>
        </label>
        <input
          type="range"
          min={10}
          max={120}
          step={10}
          value={settings.pollSeconds}
          onChange={(e) => update({ pollSeconds: Number(e.target.value) })}
        />

        <label className="setting-row">
          <span>{tr("trayDisplay")}</span>
        </label>
        <select
          className="setting-select"
          value={settings.trayTool ?? ""}
          onChange={(e) => update({ trayTool: e.target.value || null })}
        >
          <option value="">{tr("trayAuto")}</option>
          {tools
            .filter((x) => x.available)
            .map((x) => (
              <option key={x.tool} value={x.tool}>
                {x.tool}
              </option>
            ))}
        </select>

        <label className="setting-row">
          <span>{tr("language")}</span>
        </label>
        <select
          className="setting-select"
          value={settings.language ?? ""}
          onChange={(e) => update({ language: e.target.value || null })}
        >
          <option value="">{tr("langAuto")}</option>
          <option value="ko">한국어</option>
          <option value="en">English</option>
        </select>

        <label className="setting-row toggle-row">
          <span>{tr("autostart")}</span>
          <input
            type="checkbox"
            checked={autostart}
            onChange={(e) => toggleAutostart(e.target.checked)}
          />
        </label>

        <label className="setting-row toggle-row">
          <span>{tr("analytics")}</span>
          <input
            type="checkbox"
            checked={settings.analyticsEnabled}
            onChange={(e) => update({ analyticsEnabled: e.target.checked })}
          />
        </label>
        <p className="setting-hint">{tr("analyticsHint")}</p>

        <label className="setting-row toggle-row">
          <span>{tr("hideInactive")}</span>
          <input
            type="checkbox"
            checked={settings.hideInactive}
            onChange={(e) => update({ hideInactive: e.target.checked })}
          />
        </label>

        {tools.length > 0 && (
          <div className="setting-tools">
            <span className="setting-row">{tr("monitorTools")}</span>
            {tools.map((x) => (
              <label key={x.tool} className="tool-toggle">
                <input
                  type="checkbox"
                  checked={!settings.disabledTools.includes(x.tool)}
                  disabled={!x.available}
                  onChange={(e) => toggleTool(x.tool, e.target.checked)}
                />
                <span>
                  {x.tool}
                  {!x.available && <span className="muted">{tr("notDetected")}</span>}
                </span>
              </label>
            ))}
          </div>
        )}

        <p className="setting-hint">{tr("settingsHint")}</p>
      </section>
    </main>
  );
}

interface DayUsage {
  date: string;
  usage: number;
  count: number;
}
interface ToolHistory {
  tool: string;
  unit: string;
  days: DayUsage[];
}

/** 히스토리 전용 창 — 도구별 일별 사용량 막대 */
function HistoryView() {
  const [history, setHistory] = useState<ToolHistory[]>([]);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    invoke<Settings>("get_settings")
      .then((s) => setLang(resolveLang(s.language)))
      .catch(() => {});
    invoke<ToolHistory[]>("get_history", { days: 7 })
      .then(setHistory)
      .finally(() => setLoading(false));
    invoke("track_event", { event: "history_opened" }).catch(() => {});
  }, []);

  return (
    <main className="container">
      {loading && <p className="muted">{t(lang, "loading")}</p>}
      {!loading && history.every((h) => h.days.length === 0) && (
        <p className="muted">{t(lang, "noHistory")}</p>
      )}

      {history
        .filter((h) => h.days.length > 0)
        .map((h) => {
          const max = Math.max(...h.days.map((d) => d.usage), 1);
          return (
            <section className="card" key={h.tool}>
              <div className="card-head">
                <span className="tool">{h.tool}</span>
                <span className="hist-period">{t(lang, "last7days")}</span>
              </div>
              <div className="hist-bars">
                {h.days.map((d) => (
                  <div className="hist-col" key={d.date}>
                    <span className="hist-val">
                      {h.unit === "requests"
                        ? d.count
                        : formatAmount(d.usage)}
                    </span>
                    <div
                      className="hist-bar"
                      style={{ height: `${Math.max(4, (d.usage / max) * 100)}%` }}
                      title={`${d.date}: ${formatAmount(d.usage)} ${unitLabel(h.unit)} · ${d.count}`}
                    />
                    <span className="hist-date">{d.date}</span>
                  </div>
                ))}
              </div>
            </section>
          );
        })}
    </main>
  );
}

function App() {
  const label = getCurrentWindow().label;
  if (label === "settings") return <SettingsView />;
  if (label === "history") return <HistoryView />;
  return <Dashboard />;
}

export default App;
