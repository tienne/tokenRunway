import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, resolveLang, type Lang } from "./i18n";
import {
  ACTIVITY_POLL_MS,
  formatEta,
  formatResetsAt,
  leadActivity,
  type AgentActivity,
  type RunwayStatus,
  type Settings,
} from "./App";
import "./Notch.css";

interface NotchLayout {
  notchWidth: number;
  wing: number;
  barHeight: number;
  expandedWidth: number;
  expandedHeight: number;
  noticeHeight: number;
}

type NotchMode = "collapsed" | "notice" | "full";

interface Notice {
  id: string;
  title: string;
  context: string | null;
}

/** 트레이와 같은 규칙 — 지정한 도구, 없으면 공식 잔여율 중 가장 적게 남은 도구. */
function pickRepresentative(
  statuses: RunwayStatus[],
  trayTool: string | null
): RunwayStatus | null {
  if (trayTool) return statuses.find((s) => s.tool === trayTool) ?? null;
  return (
    statuses
      .filter((s) => s.percentRemaining != null)
      .sort(
        (a, b) =>
          Number(a.isEstimate) - Number(b.isEstimate) ||
          (a.percentRemaining ?? Infinity) - (b.percentRemaining ?? Infinity)
      )[0] ?? null
  );
}

/** 노치 양옆 섬. 펼침과 클릭 통과는 Rust가 커서 위치로 정하고 여기선 그리기만 한다. */
export function NotchView() {
  const [layout, setLayout] = useState<NotchLayout | null>(null);
  const [mode, setMode] = useState<NotchMode>("collapsed");
  const [statuses, setStatuses] = useState<RunwayStatus[]>([]);
  const [activity, setActivity] = useState<AgentActivity | null>(null);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [trayTool, setTrayTool] = useState<string | null>(null);
  const [pollSeconds, setPollSeconds] = useState(30);
  const [notice, setNotice] = useState<Notice | null>(null);

  useEffect(() => {
    document.documentElement.style.background = "transparent";
    document.body.style.background = "transparent";
    invoke<NotchLayout>("notch_layout").then(setLayout).catch(() => {});
    const loadSettings = () =>
      invoke<Settings>("get_settings")
        .then((s) => {
          setLang(resolveLang(s.language));
          setTrayTool(s.trayTool);
          setPollSeconds(s.pollSeconds);
        })
        .catch(() => {});
    loadSettings();
    const unSettings = listen("settings-changed", loadSettings);
    const unNotice = listen<Notice>("notch-notice", (e) => setNotice(e.payload));
    const unMode = listen<NotchMode>("notch-mode", (e) => {
      setMode(e.payload);
      if (e.payload === "collapsed") setNotice(null);
      // 모니터가 바뀌어 다시 배치됐을 수 있어 펼칠 때마다 치수를 새로 받는다.
      invoke<NotchLayout>("notch_layout").then(setLayout).catch(() => {});
    });
    return () => {
      unSettings.then((f) => f());
      unMode.then((f) => f());
      unNotice.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const load = () =>
      invoke<RunwayStatus[]>("get_runway").then(setStatuses).catch(() => {});
    load();
    const id = setInterval(load, Math.max(pollSeconds, 10) * 1000);
    return () => clearInterval(id);
  }, [pollSeconds]);

  useEffect(() => {
    const poll = () =>
      invoke<AgentActivity[]>("get_activity")
        .then((list) => setActivity(leadActivity(list)))
        .catch(() => {});
    poll();
    const id = setInterval(poll, ACTIVITY_POLL_MS);
    return () => clearInterval(id);
  }, []);

  if (!layout || layout.barHeight <= 0) return null;

  const rep = pickRepresentative(statuses, trayTool);
  const danger = rep?.verdict?.level === "danger";
  const open = mode !== "collapsed";
  const islandWidth = layout.notchWidth + layout.wing * 2;
  const tools = statuses.filter((s) => s.available);
  const height =
    mode === "full"
      ? layout.expandedHeight
      : mode === "notice"
        ? layout.barHeight + layout.noticeHeight
        : layout.barHeight;

  // 랜딩이 성공해야 inbox_land가 읽음으로 바꾼다. 실패하면 줄을 남겨 다시 누를 수 있게 한다.
  const land = (n: Notice) =>
    invoke("inbox_land", { id: n.id })
      .then(() => {
        setNotice(null);
        invoke("notch_dismiss").catch(() => {});
      })
      .catch(() => {});

  return (
    <div className="notch-root">
      <div
        className={`notch-island${open ? " open" : ""}${danger ? " danger" : ""}`}
        style={{
          width: open ? layout.expandedWidth : islandWidth,
          height,
        }}
      >
        <div
          className="notch-bar"
          style={{
            height: layout.barHeight,
            gridTemplateColumns: `1fr ${layout.notchWidth}px 1fr`,
          }}
        >
          <span className="notch-side notch-pct">
            {rep?.percentRemaining != null
              ? `${Math.round(rep.percentRemaining)}%`
              : "—"}
          </span>
          <span />
          <span className={`notch-side notch-activity ${activity?.state ?? "idle"}`}>
            {activity?.state === "working" && t(lang, "notch.working")}
            {activity?.state === "justDone" && t(lang, "notch.justDone")}
          </span>
        </div>

        {open && notice && (
          <button
            className="notch-notice"
            style={{ height: layout.noticeHeight }}
            onClick={() => land(notice)}
          >
            <span className="notch-notice-title">{notice.title}</span>
            {notice.context && (
              <span className="notch-notice-context">{notice.context}</span>
            )}
          </button>
        )}

        {mode === "full" && (
          <ul className="notch-tools">
            {tools.length === 0 && <li className="notch-empty">{t(lang, "noTools")}</li>}
            {tools.map((s) => (
              <li
                key={s.tool}
                className={`notch-tool${s.verdict?.level === "danger" ? " danger" : ""}`}
              >
                <span className="notch-tool-name">{s.tool}</span>
                <span className="notch-tool-pct">
                  {s.percentRemaining != null
                    ? `${Math.round(s.percentRemaining)}% ${t(lang, "remaining")}`
                    : "—"}
                </span>
                <span className="notch-tool-meta">
                  {t(lang, "notch.eta", { t: formatEta(s.etaMinutes, lang) })}
                  {formatResetsAt(s.resetsAt, lang) &&
                    ` · ${formatResetsAt(s.resetsAt, lang)}`}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
