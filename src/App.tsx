import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
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
  pollSeconds: number;
  disabledTools: string[];
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

function App() {
  const [statuses, setStatuses] = useState<RunwayStatus[]>([]);
  const [loading, setLoading] = useState(true);
  const [settings, setSettings] = useState<Settings>({
    alertThreshold: 20,
    etaAlertMinutes: 30,
    pollSeconds: 30,
    disabledTools: [],
  });
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [showSettings, setShowSettings] = useState(false);

  async function refresh() {
    try {
      const data = await invoke<RunwayStatus[]>("get_runway");
      setStatuses(data);
    } finally {
      setLoading(false);
    }
  }

  async function update(patch: Partial<Settings>) {
    const next = { ...settings, ...patch };
    setSettings(next);
    await invoke("set_settings", { settings: next });
    refresh();
  }

  function toggleTool(tool: string, enabled: boolean) {
    const disabled = enabled
      ? settings.disabledTools.filter((t) => t !== tool)
      : [...settings.disabledTools, tool];
    update({ disabledTools: disabled });
  }

  useEffect(() => {
    refresh();
    invoke<Settings>("get_settings").then(setSettings).catch(() => {});
    invoke<ToolInfo[]>("get_available_tools").then(setTools).catch(() => {});
  }, []);

  // 폴링 주기는 설정에 따라 동적 적용.
  useEffect(() => {
    const ms = Math.max(5, settings.pollSeconds) * 1000;
    const id = setInterval(refresh, ms);
    return () => clearInterval(id);
  }, [settings.pollSeconds]);

  return (
    <main className="container">
      <header>
        <h1>🛬 Token Runway</h1>
        <div className="header-actions">
          <button onClick={refresh} className="refresh">
            새로고침
          </button>
          <button
            onClick={() => setShowSettings((v) => !v)}
            className="icon-btn"
            title="설정"
          >
            ⚙️
          </button>
        </div>
      </header>

      {showSettings && (
        <section className="settings">
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
          </p>
        </section>
      )}

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

export default App;
