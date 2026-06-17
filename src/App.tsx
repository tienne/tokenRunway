import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import "./App.css";

interface RunwayStatus {
  tool: string;
  available: boolean;
  unit: string; // "tokens" | "requests"
  windowHours: number;
  windowUsage: number;
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

function formatEta(min: number | null): string {
  if (min == null) return "—";
  if (min < 1) return "1분 미만";
  const h = Math.floor(min / 60);
  const m = Math.round(min % 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

function formatResetsAt(iso: string | null): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (isNaN(d.getTime())) return null;
  const now = Date.now();
  const diffMin = Math.round((d.getTime() - now) / 60000);
  if (diffMin <= 0) return "곧 리셋";
  const h = Math.floor(diffMin / 60);
  const m = diffMin % 60;
  return h > 0 ? `${h}h ${m}m 후 리셋` : `${m}m 후 리셋`;
}

/** 트레이 팝오버 — 런웨이 대시보드 */
function Dashboard() {
  const [statuses, setStatuses] = useState<RunwayStatus[]>([]);
  const [loading, setLoading] = useState(true);
  const [pollSeconds, setPollSeconds] = useState(30);

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
      .then((s) => setPollSeconds(s.pollSeconds))
      .catch(() => {});
  }, []);

  // 폴링 + 설정에서 바뀐 주기 동기화.
  useEffect(() => {
    const id = setInterval(async () => {
      refresh();
      try {
        const s = await invoke<Settings>("get_settings");
        if (s.pollSeconds !== pollSeconds) setPollSeconds(s.pollSeconds);
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
            새로고침
          </button>
          <button
            onClick={() => invoke("open_settings_window")}
            className="icon-btn"
            title="설정"
          >
            ⚙️
          </button>
        </div>
      </header>

      {loading && <p className="muted">불러오는 중…</p>}
      {!loading && statuses.length === 0 && (
        <p className="muted">감지된 도구가 없습니다.</p>
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
                  {s.percentRemaining.toFixed(0)}%<span className="pct-label"> 남음</span>
                </>
              ) : (
                "한도 미설정"
              )}
            </span>
          </div>

          {s.percentRemaining != null && (
            <div className="bar">
              <div
                className="bar-fill"
                style={{ width: `${Math.max(0, Math.min(100, s.percentRemaining))}%` }}
              />
            </div>
          )}

          {formatResetsAt(s.resetsAt) && (
            <p className="resets">⏱ {formatResetsAt(s.resetsAt)}</p>
          )}

          <dl className="metrics">
            <div>
              <dt>{windowLabel(s.windowHours)} 사용</dt>
              <dd>
                {formatAmount(s.windowUsage)} {unitLabel(s.unit)}
              </dd>
            </div>
            <div>
              <dt>소진 속도</dt>
              <dd>
                {formatAmount(Math.round(s.burnRatePerMin))} {unitLabel(s.unit)}/min
              </dd>
            </div>
            <div>
              <dt>예상 소진</dt>
              <dd>{formatEta(s.etaMinutes)}</dd>
            </div>
          </dl>

          {s.sevenDayRemaining != null && (
            <p className="weekly">주간 {s.sevenDayRemaining.toFixed(0)}% 남음</p>
          )}

          {s.note && <p className="note">{s.note}</p>}
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
  });
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [permGranted, setPermGranted] = useState<boolean | null>(null);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings).catch(() => {});
    invoke<ToolInfo[]>("get_available_tools").then(setTools).catch(() => {});
    isPermissionGranted().then(setPermGranted).catch(() => {});
  }, []);

  async function askPermission() {
    const result = await requestPermission();
    setPermGranted(result === "granted");
  }

  async function update(patch: Partial<Settings>) {
    const next = { ...settings, ...patch };
    setSettings(next);
    await invoke("set_settings", { settings: next });
  }

  function toggleTool(tool: string, enabled: boolean) {
    const disabled = enabled
      ? settings.disabledTools.filter((t) => t !== tool)
      : [...settings.disabledTools, tool];
    update({ disabledTools: disabled });
  }

  return (
    <main className="container settings-window">
      <section className="settings">
        <label className="setting-row toggle-row">
          <span>OS 알림</span>
          <input
            type="checkbox"
            checked={settings.notificationsEnabled}
            onChange={(e) => update({ notificationsEnabled: e.target.checked })}
          />
        </label>
        {settings.notificationsEnabled && permGranted === false && (
          <p className="perm-warn">
            ⚠️ 시스템 알림 권한이 없어요.{" "}
            <button className="link-btn" onClick={askPermission}>
              권한 요청
            </button>
          </p>
        )}
        {settings.notificationsEnabled && permGranted === true && (
          <p className="setting-hint">시스템 알림 권한 허용됨 ✓</p>
        )}

        <hr className="setting-divider" />

        <label className="setting-row">
          <span>경보 임계치</span>
          <span className="setting-value">{settings.alertThreshold}% 남음</span>
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
          <span>예상 소진 경보</span>
          <span className="setting-value">
            {settings.etaAlertMinutes === 0
              ? "끔"
              : `${settings.etaAlertMinutes}분 전`}
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
          <span>리셋 임박 알림</span>
          <span className="setting-value">
            {settings.resetAlertMinutes === 0
              ? "끔"
              : `${settings.resetAlertMinutes}분 전`}
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
          <span>새로고침 주기</span>
          <span className="setting-value">{settings.pollSeconds}초</span>
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
          <span>트레이 표시</span>
        </label>
        <select
          className="setting-select"
          value={settings.trayTool ?? ""}
          onChange={(e) => update({ trayTool: e.target.value || null })}
        >
          <option value="">자동 (가장 임박한 도구)</option>
          {tools
            .filter((t) => t.available)
            .map((t) => (
              <option key={t.tool} value={t.tool}>
                {t.tool}
              </option>
            ))}
        </select>

        {tools.length > 0 && (
          <div className="setting-tools">
            <span className="setting-row">모니터링 도구</span>
            {tools.map((t) => (
              <label key={t.tool} className="tool-toggle">
                <input
                  type="checkbox"
                  checked={!settings.disabledTools.includes(t.tool)}
                  disabled={!t.available}
                  onChange={(e) => toggleTool(t.tool, e.target.checked)}
                />
                <span>
                  {t.tool}
                  {!t.available && <span className="muted"> (미감지)</span>}
                </span>
              </label>
            ))}
          </div>
        )}

        <p className="setting-hint">
          잔여율 또는 예상 소진 시간 중 하나라도 도달하면 알림을 보냅니다.
          리셋 임박 알림은 반대로, 곧 리셋되는데 토큰이 많이 남아있을 때
          "지금 더 써도 된다"고 알려줍니다.
        </p>
      </section>
    </main>
  );
}

function App() {
  // 창 label로 화면 분기 (main = 팝오버 대시보드, settings = 설정 창)
  const isSettings = getCurrentWindow().label === "settings";
  return isSettings ? <SettingsView /> : <Dashboard />;
}

export default App;
