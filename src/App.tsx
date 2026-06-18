import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import { openUrl } from "@tauri-apps/plugin-opener";
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
  limit: number | null;
  percentRemaining: number | null;
  burnRatePerMin: number;
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

  async function refresh() {
    try {
      const data = await invoke<RunwayStatus[]>("get_runway");
      setStatuses(data);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    refresh();
    invoke<Settings>("get_settings")
      .then((s) => {
        setPollSeconds(s.pollSeconds);
        setLang(resolveLang(s.language));
      })
      .catch(() => {});
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

          <dl className="metrics">
            <div>
              <dt>
                {windowLabel(s.windowHours)} {t(lang, "used")}
              </dt>
              <dd>
                {formatAmount(s.windowUsage)} {unitLabel(s.unit)}
              </dd>
            </div>
            <div>
              <dt>{t(lang, "burnRate")}</dt>
              <dd>
                {formatAmount(Math.round(s.burnRatePerMin))} {unitLabel(s.unit)}
                /min
              </dd>
            </div>
            <div>
              <dt>{t(lang, "eta")}</dt>
              <dd>{formatEta(s.etaMinutes, lang)}</dd>
            </div>
          </dl>

          <p className="daily">
            {t(lang, "today")}{" "}
            {s.unit === "requests"
              ? `${s.dailyCount} ${t(lang, "requests")}`
              : `${formatAmount(s.dailyUsage)} ${unitLabel(s.unit)} · ${s.dailyCount} ${t(lang, "messages")}`}
          </p>

          {s.sevenDayRemaining != null && (
            <p className="weekly">
              {t(lang, "weekly", { n: s.sevenDayRemaining.toFixed(0) })}
            </p>
          )}

          {s.note && <p className="note">{t(lang, s.note)}</p>}
        </section>
      ))}
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
  });
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [permGranted, setPermGranted] = useState<boolean | null>(null);

  const lang = resolveLang(settings.language);
  const tr = (k: string, p?: Record<string, string | number>) => t(lang, k, p);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings).catch(() => {});
    invoke<ToolInfo[]>("get_available_tools").then(setTools).catch(() => {});
    isPermissionGranted().then(setPermGranted).catch(() => {});
  }, []);

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

function App() {
  const isSettings = getCurrentWindow().label === "settings";
  return isSettings ? <SettingsView /> : <Dashboard />;
}

export default App;
