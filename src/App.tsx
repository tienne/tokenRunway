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
  note: string | null;
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
    const id = setInterval(refresh, 30_000); // 30s 폴링
    return () => clearInterval(id);
  }, []);

  return (
    <main className="container">
      <header>
        <h1>🛬 Token Runway</h1>
        <button onClick={refresh} className="refresh">
          새로고침
        </button>
      </header>

      {loading && <p className="muted">불러오는 중…</p>}
      {!loading && statuses.length === 0 && (
        <p className="muted">감지된 도구가 없습니다.</p>
      )}

      {statuses.map((s) => (
        <section className="card" key={s.tool}>
          <div className="card-head">
            <span className="tool">{s.tool}</span>
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
