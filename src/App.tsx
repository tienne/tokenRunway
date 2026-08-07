import { useEffect, useState, useCallback } from "react";
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

interface Insight {
  key: string;
  level: "good" | "warn" | "tip";
}

interface ModelBreakdown {
  model: string;
  usage: number;
  cost: number;
}

/** 주간 윈도우 안의 하루 — 그날이 주간 한도의 몇 %를 썼는지 */
interface WeeklyDay {
  date: string; // "MM/DD"
  weekday: number; // 0=월 ~ 6=일
  usage: number;
  dailyPercent: number;
  cumulativePercent: number;
  isToday: boolean;
  isFuture: boolean;
}

/** 카드 맨 위 한 줄 결론 — 소진이 먼저인지 리셋이 먼저인지 */
interface Verdict {
  key: string;
  level: "danger" | "warn" | "good";
  etaMinutes: number | null;
  resetMinutes: number | null;
}

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
  insights: Insight[];
  planHint: Insight | null;
  models: ModelBreakdown[];
  sparkline: number[];
  limit: number | null;
  percentRemaining: number | null;
  burnRatePerMin: number;
  burnTrend: string; // "up" | "down" | "flat"
  etaMinutes: number | null;
  resetsAt: string | null;
  sevenDayRemaining: number | null;
  sevenDayEtaMinutes: number | null;
  weeklyDays: WeeklyDay[];
  verdict: Verdict | null;
  isEstimate: boolean;
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
  trayShowPercent: boolean;
  trayShowReset: boolean;
}

interface ToolInfo {
  tool: string;
  available: boolean;
}

function formatAmount(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
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

/**
 * 누적 윈도우 제목. 도구마다 주기가 달라(Claude·Codex 5h, Gemini 24h, Grok 주간)
 * "세션 (168h)" 같은 표기가 나오지 않게 길이에 맞는 말을 쓴다.
 */
function windowTitle(hours: number, lang: Lang): string {
  if (hours >= 24 * 28) return t(lang, "windowMonth");
  if (hours >= 24 * 7) return t(lang, "windowWeek");
  if (hours >= 24) return t(lang, "windowDay");
  return `${t(lang, "session")} (${windowLabel(hours)})`;
}

function formatEta(min: number | null, lang: Lang): string {
  if (min == null) return "—";
  if (min < 1) return t(lang, "lessThanMin");
  const h = Math.floor(min / 60);
  const m = Math.round(min % 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** 하루가 넘는 기간용 — "3d 4h". 주간 한도·리셋처럼 긴 시간에 쓴다. */
function formatDuration(min: number | null, lang: Lang): string {
  if (min == null) return "—";
  if (min < 1) return t(lang, "lessThanMin");
  const d = Math.floor(min / 1440);
  if (d >= 1) {
    const h = Math.round((min % 1440) / 60);
    return h > 0 ? `${d}d ${h}h` : `${d}d`;
  }
  return formatEta(min, lang);
}

/** 요일 라벨 — 0=월 ~ 6=일 */
function weekdayLabel(i: number, lang: Lang): string {
  return t(lang, `wd.${i}`);
}

/**
 * 주간 한도 일별 막대 — 팝오버(압축)와 히스토리 창(상세)이 공유한다.
 *
 * 막대 영역과 요일 축을 따로 두는 이유: 누적 선 SVG를 막대 영역에만 정확히 겹치기
 * 위해서다. 라벨까지 한 상자에 넣으면 선이 라벨 높이만큼 밀린다.
 * 칸 간격도 gap이 아니라 안쪽 padding으로 준다 — gap을 쓰면 flex 칸이 균등 분할되지
 * 않아 선의 x좌표와 막대 중심이 어긋난다.
 */
function WeeklyBars({
  days,
  lang,
  showLine,
  onHover,
}: {
  days: WeeklyDay[];
  lang: Lang;
  showLine?: boolean;
  onHover?: (i: number | null) => void;
}) {
  // 가장 많이 쓴 날을 꽉 차게 그려야 하루치 차이가 눈에 들어온다.
  const max = Math.max(...days.map((d) => d.dailyPercent), 0.01);

  // 누적 선은 첫 사용일부터 오늘까지만 긋는다. 미래로 이으면 데이터가 있는 것처럼
  // 보이고, 아직 안 쓴 앞 구간은 점이 바닥에 붙어 요일 라벨과 겹친다.
  const firstUsed = days.findIndex((d) => d.cumulativePercent > 0);
  const past = days
    .map((d, i) => ({ d, i }))
    .filter((x) => !x.d.isFuture && firstUsed >= 0 && x.i >= firstUsed);
  const line = past
    .map(
      ({ d, i }, k) =>
        `${k === 0 ? "M" : "L"}${i + 0.5},${(
          100 - Math.min(100, d.cumulativePercent)
        ).toFixed(2)}`
    )
    .join(" ");

  return (
    <>
      <div className={`wk-plot${showLine ? " wk-plot-tall" : ""}`}>
        {days.map((d, i) => (
          <div
            className={`wk-col${d.isToday ? " wk-today" : ""}${
              d.isFuture ? " wk-future" : ""
            }`}
            key={d.date}
            title={
              onHover
                ? undefined
                : `${d.date} · ${d.dailyPercent.toFixed(1)}%`
            }
            onMouseEnter={() => onHover?.(i)}
            onMouseLeave={() => onHover?.(null)}
          >
            <div className="wk-track">
              <div
                className="wk-fill"
                style={{ height: `${(d.dailyPercent / max) * 100}%` }}
              />
            </div>
          </div>
        ))}
        {/* 누적 선은 막대(주황)와 다른 색이라야 겹쳐도 읽힌다. 기존 추세 차트와 같은 파랑. */}
        {showLine && past.length > 1 && (
          <svg
            className="wk-line"
            viewBox={`0 0 ${days.length} 100`}
            preserveAspectRatio="none"
            aria-hidden
          >
            <path
              d={line}
              fill="none"
              stroke="#0a84ff"
              strokeWidth="1.5"
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          </svg>
        )}
        {/* 점은 SVG 대신 HTML로 — preserveAspectRatio="none"이라 SVG 원은 타원으로 찌그러진다 */}
        {showLine &&
          past.map(({ d, i }) => (
            <span
              className="wk-dot"
              key={d.date}
              style={{
                left: `${((i + 0.5) / days.length) * 100}%`,
                top: `${100 - Math.min(100, d.cumulativePercent)}%`,
              }}
            />
          ))}
      </div>
      <div className="wk-axis">
        {days.map((d) => (
          <span
            className={d.isToday ? "wk-today-label" : d.isFuture ? "wk-dim" : ""}
            key={d.date}
          >
            {weekdayLabel(d.weekday, lang)}
          </span>
        ))}
      </div>
    </>
  );
}

/** 팝오버용 — 폭이 좁아 축·툴팁 없이 막대와 요일만 */
function WeeklyMiniBars({ days, lang }: { days: WeeklyDay[]; lang: Lang }) {
  if (!days.length) return null;
  return (
    <div className="wk-mini">
      <WeeklyBars days={days} lang={lang} />
    </div>
  );
}

/** "얼마나 남았어?"에 대한 직접적인 한 줄 답 */
function VerdictLine({ v, lang }: { v: Verdict; lang: Lang }) {
  return (
    <p className={`verdict verdict-${v.level}`}>
      {t(lang, v.key, {
        eta: formatDuration(v.etaMinutes, lang),
        reset: formatDuration(v.resetMinutes, lang),
      })}
    </p>
  );
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
                  {/* 가정한 한도로 낸 추정치는 근사 기호로 구분한다 */}
                  {s.isEstimate && "≈"}
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

          {s.verdict && <VerdictLine v={s.verdict} lang={lang} />}

          {formatResetsAt(s.resetsAt, lang) && (
            <p className="resets">⏱ {formatResetsAt(s.resetsAt, lang)}</p>
          )}

          <div className="today-section">
            <span className="today-title">{windowTitle(s.windowHours, lang)}</span>
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

          {s.insights.map((ins) => (
            <p key={ins.key} className={`insight insight-${ins.level}`}>
              {t(lang, ins.key)}
            </p>
          ))}

          {s.planHint && (
            <p className={`insight plan-hint insight-${s.planHint.level}`}>
              {t(lang, s.planHint.key)}
            </p>
          )}

          {s.models.length > 0 && (
            <div className="model-mini">
              <span className="model-mini-title">
                {t(lang, "modelBreakdown")}
              </span>
              {s.models.map((m) => (
                <div className="model-row" key={m.model}>
                  <span className="model-name">{m.model}</span>
                  <div className="model-track">
                    <div
                      className="model-fill"
                      style={{
                        width: `${(m.usage / (s.models[0].usage || 1)) * 100}%`,
                      }}
                    />
                  </div>
                  <span className="model-val">
                    {formatAmount(m.usage)}
                    {m.cost > 0 ? ` · $${m.cost.toFixed(2)}` : ""}
                  </span>
                </div>
              ))}
            </div>
          )}

          {/* 주 윈도우가 이미 주간인 도구(Grok)는 위 잔여율이 곧 주간이라 중복이다 */}
          {s.sevenDayRemaining != null && s.windowHours < 24 * 7 && (
            <p className="weekly">
              {t(lang, "weekly", { n: s.sevenDayRemaining.toFixed(0) })}
              {/* Max 사용자의 실제 병목은 5시간이 아니라 주간인 경우가 많다 */}
              {s.sevenDayEtaMinutes != null && (
                <span className="weekly-eta">
                  {" · "}
                  {t(lang, "weeklyEta", {
                    t: formatDuration(s.sevenDayEtaMinutes, lang),
                  })}
                </span>
              )}
            </p>
          )}
          <WeeklyMiniBars days={s.weeklyDays} lang={lang} />

          {s.note && (
            <p className={s.note.startsWith("error.") ? "note-error" : "note"}>
              {t(lang, s.note, { tool: s.tool })}
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
    trayShowPercent: true,
    trayShowReset: true,
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
    <main className="container settings-window win-framed">
      <div className="win-titlebar-bg" />
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
          <span>{tr("trayInfo")}</span>
        </label>
        <label className="tool-toggle">
          <input
            type="checkbox"
            checked={settings.trayShowPercent}
            onChange={(e) => update({ trayShowPercent: e.target.checked })}
          />
          <span>{tr("trayShowPercent")}</span>
        </label>
        <label className="tool-toggle">
          <input
            type="checkbox"
            checked={settings.trayShowReset}
            onChange={(e) => update({ trayShowReset: e.target.checked })}
          />
          <span>{tr("trayShowReset")}</span>
        </label>

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

interface DayStat {
  date: string;
  usage: number;
  count: number;
  cost: number;
}
interface ModelUsage {
  model: string;
  usage: number;
  cost: number;
  count: number;
}
interface PlanTier {
  plan: string;
  projectedUtil: number;
  current: boolean;
  recommended: boolean;
}
interface LimitEstimate {
  limitTokens: number;
  usedTokens: number;
  messages: number;
}
interface PlanAdvice {
  currentPlan: string;
  recommendedPlan: string;
  direction: "upgrade" | "downgrade" | "keep" | "managed" | "na";
  managed: boolean;
  tiers: PlanTier[];
  estimate: LimitEstimate | null;
}
interface ToolStats {
  tool: string;
  unit: string;
  weeklyDays: WeeklyDay[];
  days: DayStat[];
  totalUsage: number;
  totalCost: number;
  avgUsage: number;
  peakDate: string;
  peakUsage: number;
  models: ModelUsage[];
  hourly: number[];
  thisWeekUsage: number;
  lastWeekUsage: number;
  thisWeekCost: number;
  lastWeekCost: number;
  planAdvice: PlanAdvice | null;
}

const PERIODS = [7, 14, 30, 60, 90, 180, 365];

/** 통계 로딩 중 표시할 스켈레톤 카드 (shimmer) */
function StatsCardSkeleton() {
  return (
    <section className="card" aria-hidden="true">
      <div className="sk sk-title" />
      <div className="sk-summary">
        {[0, 1, 2, 3].map((i) => (
          <div className="sk sk-stat" key={i} />
        ))}
      </div>
      <div className="sk sk-bars" />
      <div className="sk sk-block" />
      <div className="sk sk-block" />
    </section>
  );
}

/** 일수가 많을 때의 일별 추세 — 영역+꺾은선 + 호버/클릭 툴팁 */
function TrendChart({
  days,
  fmt,
  unit,
  lang,
}: {
  days: DayStat[];
  fmt: (n: number) => string;
  unit: string;
  lang: Lang;
}) {
  const [active, setActive] = useState<number | null>(null);
  const W = 100;
  const H = 40;
  const max = Math.max(...days.map((d) => d.usage), 1);
  const pts = days.map((d, i): [number, number] => [
    days.length > 1 ? (i / (days.length - 1)) * W : W / 2,
    H - (d.usage / max) * H,
  ]);
  const line = pts
    .map(([x, y], i) => `${i === 0 ? "M" : "L"}${x.toFixed(2)},${y.toFixed(2)}`)
    .join(" ");
  const area = `${line} L${W},${H} L0,${H} Z`;
  const mid = Math.floor((days.length - 1) / 2);

  // 포인터 x → 가장 가까운 데이터 인덱스
  const pick = (e: React.MouseEvent<HTMLDivElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width === 0) return;
    const ratio = (e.clientX - rect.left) / rect.width;
    const idx = Math.round(ratio * (days.length - 1));
    setActive(Math.max(0, Math.min(days.length - 1, idx)));
  };

  const a = active != null ? days[active] : null;
  const ax = active != null ? pts[active][0] : 0;
  const ay = active != null ? (pts[active][1] / H) * 100 : 0;
  const tipLeft = Math.min(82, Math.max(18, ax)); // 좌우 끝 잘림 방지

  return (
    <div className="trend">
      <div
        className="trend-plot"
        onMouseMove={pick}
        onMouseLeave={() => setActive(null)}
        onClick={pick}
      >
        <svg
          className="trend-svg"
          viewBox={`0 0 ${W} ${H}`}
          preserveAspectRatio="none"
        >
          <defs>
            <linearGradient id="trendFill" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="#0a84ff" stopOpacity="0.35" />
              <stop offset="100%" stopColor="#0a84ff" stopOpacity="0" />
            </linearGradient>
          </defs>
          <path d={area} fill="url(#trendFill)" />
          <path
            d={line}
            fill="none"
            stroke="#0a84ff"
            strokeWidth="1.5"
            strokeLinejoin="round"
            vectorEffect="non-scaling-stroke"
          />
        </svg>
        {a && (
          <>
            <div className="trend-guide" style={{ left: `${ax}%` }} />
            <div
              className="trend-active-dot"
              style={{ left: `${ax}%`, top: `${ay}%` }}
            />
            <div className="trend-tip" style={{ left: `${tipLeft}%` }}>
              <span className="trend-tip-date">{a.date}</span>
              <span>
                {fmt(a.usage)} {unitLabel(unit)}
              </span>
              <span>
                {a.count}{" "}
                {t(lang, unit === "requests" ? "requests" : "messages")}
              </span>
              {a.cost > 0 && <span>${a.cost.toFixed(2)}</span>}
            </div>
          </>
        )}
      </div>
      <div className="trend-axis">
        <span>{days[0]?.date}</span>
        <span>{days[mid]?.date}</span>
        <span>{days[days.length - 1]?.date}</span>
      </div>
    </div>
  );
}

/**
 * 주간 한도 소진 상세 — 일별 막대 + 누적 선 (히스토리 창용).
 *
 * 막대는 그날 하루가 쓴 몫, 선은 그날까지 누적. 100% 선이 주간 한도다.
 * 막대 합계는 공식 주간 사용률과 항상 일치한다(백엔드에서 안분).
 */
function WeeklyLimitBlock({ days, lang }: { days: WeeklyDay[]; lang: Lang }) {
  const [active, setActive] = useState<number | null>(null);
  if (!days.length) return null;

  const total = days[days.length - 1].cumulativePercent;
  const a = active != null ? days[active] : null;

  return (
    <div className="stat-block">
      <span className="stat-title">{t(lang, "wkTitle")}</span>
      <p className="wk-summary">
        {t(lang, "wkSummary", {
          used: total.toFixed(0),
          left: Math.max(0, 100 - total).toFixed(0),
        })}
      </p>
      <WeeklyBars days={days} lang={lang} showLine onHover={setActive} />
      {a ? (
        <p className="wk-tip">
          <b>{a.date}</b> · {t(lang, "wkDaily", { n: a.dailyPercent.toFixed(1) })}{" "}
          · {t(lang, "wkCum", { n: a.cumulativePercent.toFixed(1) })} ·{" "}
          {formatAmount(a.usage)} tok
        </p>
      ) : (
        <p className="wk-tip wk-tip-hint">{t(lang, "wkHint")}</p>
      )}
    </div>
  );
}

/** 요금제 티어 한 줄 — 예상 소진율 막대 */
function TierBar({
  tier,
  lang,
  managed,
}: {
  tier: PlanTier;
  lang: Lang;
  managed?: boolean;
}) {
  const lvl =
    tier.projectedUtil > 90 ? "over" : tier.projectedUtil > 75 ? "tight" : "ok";
  return (
    <div className="plan-tier">
      <span className="plan-tier-name">
        {tier.plan}
        {tier.current && (
          <span className="plan-tag">{t(lang, "planCurrent")}</span>
        )}
        {tier.recommended && (
          <span className="plan-tag plan-tag-rec">
            ★ {t(lang, managed ? "planEquivTag" : "planRecommended")}
          </span>
        )}
      </span>
      <div className="model-track">
        <div
          className={`plan-fill plan-fill-${lvl}`}
          style={{ width: `${Math.min(100, tier.projectedUtil)}%` }}
        />
      </div>
      <span className="model-val">{tier.projectedUtil.toFixed(0)}%</span>
    </div>
  );
}

/** 사용량 기반 요금제 추천 — 티어별 예상 소진율 막대 */
function PlanAdviceBlock({ a, lang }: { a: PlanAdvice; lang: Lang }) {
  // 관리형(Enterprise/Team) — 추정 5시간 한도 + 개인 플랜 환산(참고용, 전환 추천 아님).
  if (a.managed) {
    const e = a.estimate;
    const hasTiers = a.tiers.length > 0;
    return (
      <div className="stat-block">
        <span className="stat-title">{t(lang, "planManagedTitle")}</span>
        {e ? (
          <>
            <p className="plan-est">
              {t(lang, "est5hLimit", { n: formatAmount(e.limitTokens) })}
            </p>
            <p className="plan-est-sub">
              {t(lang, "est5hDetail", {
                used: formatAmount(e.usedTokens),
                msg: e.messages,
              })}
            </p>
          </>
        ) : (
          <p className="plan-note">{t(lang, "planNA", { plan: a.currentPlan })}</p>
        )}
        {hasTiers && (
          <>
            <p className="plan-rec-headline plan-rec-managed">
              {t(lang, "planEquiv", { rec: a.recommendedPlan })}
            </p>
            {a.tiers.map((tier) => (
              <TierBar key={tier.plan} tier={tier} lang={lang} managed />
            ))}
          </>
        )}
        {(e || hasTiers) && (
          <p className="plan-note">{t(lang, "est5hNote")}</p>
        )}
      </div>
    );
  }

  const headline =
    a.direction === "keep"
      ? t(lang, "planKeep")
      : a.direction === "upgrade"
      ? t(lang, "planUpgrade", { rec: a.recommendedPlan })
      : t(lang, "planDowngrade", { rec: a.recommendedPlan });
  return (
    <div className="stat-block">
      <span className="stat-title">{t(lang, "planRec")}</span>
      <p className={`plan-rec-headline plan-rec-${a.direction}`}>{headline}</p>
      {a.tiers.map((tier) => (
        <TierBar key={tier.plan} tier={tier} lang={lang} />
      ))}
      <p className="plan-note">{t(lang, "planEstimateNote")}</p>
    </div>
  );
}

/** 도구 하나의 통계 카드 — 요약·요금제추천·일별·주간비교·모델별·시간대별 */
function StatsCard({ s, lang }: { s: ToolStats; lang: Lang }) {
  const isReq = s.unit === "requests";
  const fmt = (n: number) => (isReq ? `${n}` : formatAmount(n));
  // 막대가 많으면(14/30일) 컬럼이 좁아져 텍스트가 창을 넘친다.
  // 막대 위 값은 숨기고, 날짜는 일정 간격으로만 표시한다.
  const dense = s.days.length > 10;
  const maxDay = Math.max(...s.days.map((d) => d.usage), 1);
  const maxHour = Math.max(...s.hourly, 1);
  const maxModel = Math.max(...s.models.map((m) => m.usage), 1);

  // 이번주 vs 지난주 증감률
  const wDelta =
    s.lastWeekUsage > 0
      ? ((s.thisWeekUsage - s.lastWeekUsage) / s.lastWeekUsage) * 100
      : null;
  const wClass =
    wDelta == null
      ? "trend-flat"
      : wDelta > 0
      ? "trend-up"
      : wDelta < 0
      ? "trend-down"
      : "trend-flat";

  return (
    <section className="card">
      <div className="card-head">
        <span className="tool">{s.tool}</span>
      </div>

      {/* 요약 */}
      <div className="today">
        <div className="today-item">
          <span className="today-val">{fmt(s.totalUsage)}</span>
          <span className="today-label">{t(lang, "statTotal")}</span>
        </div>
        <div className="today-item">
          <span className="today-val">{fmt(s.avgUsage)}</span>
          <span className="today-label">{t(lang, "statAvg")}</span>
        </div>
        <div className="today-item">
          <span className="today-val">{fmt(s.peakUsage)}</span>
          <span className="today-label">
            {t(lang, "statPeak")}
            {s.peakDate ? ` ${s.peakDate}` : ""}
          </span>
        </div>
        {s.totalCost > 0 && (
          <div className="today-item">
            <span className="today-val">${s.totalCost.toFixed(2)}</span>
            <span className="today-label">{t(lang, "apiValue")}</span>
          </div>
        )}
      </div>

      {/* 주간 한도 소진 — 주간 윈도우가 없는 도구는 빈 배열이라 그려지지 않는다 */}
      <WeeklyLimitBlock days={s.weeklyDays} lang={lang} />

      {/* 사용량 기반 요금제 추천 */}
      {s.planAdvice && <PlanAdviceBlock a={s.planAdvice} lang={lang} />}

      {/* 일별: 일수 많으면 추세 차트, 적으면 막대 */}
      {dense ? (
        <TrendChart days={s.days} fmt={fmt} unit={s.unit} lang={lang} />
      ) : (
        <div className="hist-bars">
          {s.days.map((d) => (
            <div className="hist-col" key={d.date}>
              <span className="hist-val">{fmt(d.usage)}</span>
              <div
                className="hist-bar"
                style={{ height: `${Math.max(4, (d.usage / maxDay) * 100)}%` }}
                title={`${d.date}: ${fmt(d.usage)} ${unitLabel(s.unit)} · ${d.count}${
                  d.cost > 0 ? ` · $${d.cost.toFixed(2)}` : ""
                }`}
              />
              <span className="hist-date">{d.date}</span>
            </div>
          ))}
        </div>
      )}

      {/* 주간 비교 */}
      <div className="stat-block">
        <span className="stat-title">{t(lang, "weekCompare")}</span>
        <div className="week-cmp">
          <span>
            {t(lang, "thisWeek")} <b>{fmt(s.thisWeekUsage)}</b>
          </span>
          <span className="week-vs">vs</span>
          <span>
            {t(lang, "lastWeek")} <b>{fmt(s.lastWeekUsage)}</b>
          </span>
          {wDelta != null && (
            <span className={`week-delta ${wClass}`}>
              {wDelta > 0 ? "▲" : wDelta < 0 ? "▼" : "—"}{" "}
              {Math.abs(wDelta).toFixed(0)}%
            </span>
          )}
        </div>
      </div>

      {/* 모델별 분해 */}
      {s.models.length > 0 && (
        <div className="stat-block">
          <span className="stat-title">{t(lang, "modelBreakdown")}</span>
          {s.models.map((m) => (
            <div className="model-row" key={m.model}>
              <span className="model-name">{m.model}</span>
              <div className="model-track">
                <div
                  className="model-fill"
                  style={{ width: `${(m.usage / maxModel) * 100}%` }}
                />
              </div>
              <span className="model-val">
                {fmt(m.usage)}
                {m.cost > 0 ? ` · $${m.cost.toFixed(2)}` : ""}
              </span>
            </div>
          ))}
        </div>
      )}

      {/* 시간대별 패턴 */}
      <div className="stat-block">
        <span className="stat-title">{t(lang, "hourlyPattern")}</span>
        <div className="hourly-bars">
          {s.hourly.map((h, i) => (
            <div
              className="hourly-bar"
              key={i}
              style={{ height: `${Math.max(3, (h / maxHour) * 100)}%` }}
              title={`${i}–${i + 1}: ${fmt(h)} ${unitLabel(s.unit)}`}
            />
          ))}
        </div>
        <div className="hourly-axis">
          <span>0</span>
          <span>6</span>
          <span>12</span>
          <span>18</span>
          <span>23</span>
        </div>
      </div>
    </section>
  );
}

/** 통계 전용 창 — 기간 토글 + 도구별 요약·추세 */
function HistoryView() {
  const [stats, setStats] = useState<ToolStats[]>([]);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [loading, setLoading] = useState(true);
  const [days, setDays] = useState(7);

  useEffect(() => {
    invoke<Settings>("get_settings")
      .then((s) => setLang(resolveLang(s.language)))
      .catch(() => {});
    invoke("track_event", { event: "history_opened" }).catch(() => {});
  }, []);

  const load = useCallback(() => {
    setLoading(true);
    const start = Date.now();
    invoke<ToolStats[]>("get_stats", { days })
      .then(setStats)
      .catch(() => {})
      .finally(() => {
        // 캐시 적중 시 즉시 반환되면 스켈레톤이 깜빡일 새도 없다.
        // 최소 350ms는 유지해 로딩 상태가 인지되게 한다.
        const wait = Math.max(0, 350 - (Date.now() - start));
        setTimeout(() => setLoading(false), wait);
      });
  }, [days]);

  // 마운트 + 기간 변경 시 조회
  useEffect(() => {
    load();
  }, [load]);

  // 숨겨뒀던 창이 다시 열릴 때(웹뷰 재부팅 없이 show) 데이터 갱신
  useEffect(() => {
    const un = listen("history-refresh", () => load());
    return () => {
      un.then((f) => f());
    };
  }, [load]);

  const active = stats.filter((s) => s.days.length > 0);

  return (
    <main className="container win-framed">
      <div className="win-titlebar-bg" />
      <div className="period-bar">
        <span className="period-label">{t(lang, "period")}</span>
        <select
          className="period-select"
          value={days}
          onChange={(e) => setDays(Number(e.target.value))}
        >
          {PERIODS.map((p) => (
            <option key={p} value={p}>
              {p === 365 ? t(lang, "period1y") : t(lang, "nDays", { n: p })}
            </option>
          ))}
        </select>
      </div>

      {loading && (
        <>
          <StatsCardSkeleton />
          <StatsCardSkeleton />
        </>
      )}
      {!loading && active.length === 0 && (
        <p className="muted">{t(lang, "noHistory")}</p>
      )}

      {!loading &&
        active.map((s) => <StatsCard key={s.tool} s={s} lang={lang} />)}
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
