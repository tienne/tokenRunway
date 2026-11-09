import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, resolveLang, type Lang } from "./i18n";
import {
  ACTIVITY_POLL_MS,
  formatCountdown,
  formatDuration,
  formatEta,
  leadActivity,
  type AgentActivity,
  type RunwayStatus,
  type Settings,
} from "./App";
import { SessionList, SessionSkeleton, useSessions } from "./SessionList";
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

type AccountStatus = RunwayStatus["accounts"][number];

/** 알림함 배지에 쓰는 필드만. */
interface InboxEntry {
  read: boolean;
}

interface NotchFigure {
  pct: number | null;
  /** 활성 계정 값을 못 받아 다른 계정 값을 빌려 왔으면 그 계정. */
  borrowed: AccountStatus | null;
}

/**
 * 노치 왼쪽에 띄울 숫자. 카드·트레이·경보는 활성 계정 값만 쓰지만(폴백 없음), 노치는
 * 한눈에 보는 자리라 빈칸보다 값을 받은 다른 계정 숫자가 낫다. 빌려 온 값은 흐리게
 * 그려 활성 계정 값과 구분한다.
 */
function figureOf(s: RunwayStatus): NotchFigure {
  if (s.percentRemaining != null) return { pct: s.percentRemaining, borrowed: null };
  const other = (s.accounts ?? [])
    .filter((a) => a.percentRemaining != null)
    .sort((a, b) => (a.percentRemaining ?? 0) - (b.percentRemaining ?? 0))[0];
  return other
    ? { pct: other.percentRemaining, borrowed: other }
    : { pct: null, borrowed: null };
}

/** 트레이와 같은 규칙 — 지정한 도구, 없으면 공식 잔여율 중 가장 적게 남은 도구. */
function pickRepresentative(
  statuses: RunwayStatus[],
  trayTool: string | null
): RunwayStatus | null {
  if (trayTool) return statuses.find((s) => s.tool === trayTool) ?? null;
  return (
    statuses
      .filter((s) => figureOf(s).pct != null)
      .sort(
        (a, b) =>
          Number(a.isEstimate) - Number(b.isEstimate) ||
          Number(figureOf(a).borrowed != null) - Number(figureOf(b).borrowed != null) ||
          (figureOf(a).pct ?? Infinity) - (figureOf(b).pct ?? Infinity)
      )[0] ?? null
  );
}

/** 잔여율이 경보 임계치 이하이거나 리셋 전에 바닥날 페이스면 위험으로 칠한다. */
function isDanger(pct: number | null, verdict: RunwayStatus["verdict"], threshold: number) {
  return (pct != null && pct <= threshold) || verdict?.level === "danger";
}

function minutesUntil(iso: string | null): number | null {
  if (!iso) return null;
  const ms = new Date(iso).getTime() - Date.now();
  return Number.isFinite(ms) && ms > 0 ? ms / 60_000 : null;
}

function ToolRow({
  s,
  lang,
  threshold,
}: {
  s: RunwayStatus;
  lang: Lang;
  threshold: number;
}) {
  const danger = isDanger(s.percentRemaining, s.verdict, threshold);
  const resetIn = minutesUntil(s.resetsAt);
  // 주간 윈도우 도구(Grok)는 5h·7d 슬롯에 같은 값이 들어 있어 주간 줄을 따로 띄우지 않는다.
  const weekly =
    s.sevenDayRemaining != null && s.windowHours < 24 * 7 ? s.sevenDayRemaining : null;
  const meta = [
    resetIn != null && t(lang, "notch.resetIn", { t: formatCountdown(resetIn, lang) }),
    weekly != null && t(lang, "weekly", { n: weekly.toFixed(0) }),
  ].filter(Boolean);
  return (
    <li className={`notch-tool${danger ? " danger" : ""}`}>
      <div className="notch-tool-head">
        <span className="notch-tool-name">
          {s.tool}
          {s.plan && <span className="notch-plan">{s.plan}</span>}
        </span>
        <span className="notch-tool-pct">
          {s.percentRemaining != null
            ? `${Math.round(s.percentRemaining)}% ${t(lang, "remaining")}`
            : "—"}
        </span>
      </div>
      {s.verdict ? (
        <p className={`notch-verdict ${s.verdict.level}`}>
          {t(lang, s.verdict.key, {
            eta: formatDuration(s.verdict.etaMinutes, lang),
            reset: formatCountdown(s.verdict.resetMinutes, lang),
          })}
        </p>
      ) : (
        s.etaMinutes != null && (
          <p className="notch-verdict">
            {t(lang, "notch.eta", { t: formatEta(s.etaMinutes, lang) })}
          </p>
        )
      )}
      {meta.length > 0 && <p className="notch-tool-meta">{meta.join(" · ")}</p>}
      {(s.accounts ?? []).length > 1 && (
        <ul className="notch-accts">
          {s.accounts.map((a) => (
            <li key={a.id} className={`notch-acct${a.isActive ? " active" : ""}`}>
              <span className="notch-acct-name">{a.label}</span>
              <span
                className={`notch-acct-pct${
                  a.percentRemaining != null && a.percentRemaining <= threshold ? " danger" : ""
                }`}
              >
                {a.percentRemaining != null
                  ? `${Math.round(a.percentRemaining)}%`
                  : t(lang, a.note ?? "acctFailed", { tool: s.tool })}
              </span>
            </li>
          ))}
        </ul>
      )}
    </li>
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
  const [threshold, setThreshold] = useState(20);
  const [unread, setUnread] = useState(0);
  // 펼친 패널의 내용 높이. 섬 높이와 Rust의 클릭 영역을 여기에 맞춘다.
  const [bodyHeight, setBodyHeight] = useState(0);
  const scrollInnerRef = useRef<HTMLDivElement>(null);
  const actionsRef = useRef<HTMLElement>(null);
  const sessions = useSessions(mode === "full");

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
          setThreshold(s.alertThreshold);
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
    invoke<InboxEntry[]>("get_inbox")
      .then((items) => setUnread(items.filter((i) => !i.read).length))
      .catch(() => {});
    const un = listen<number>("inbox-changed", (e) => setUnread(e.payload));
    return () => {
      un.then((f) => f()).catch(() => {});
    };
  }, []);

  // 패널은 접혀 있을 때도 그려두고(섬이 잘라 가린다) 높이를 계속 잰다. 펼치는 순간
  // Rust가 이미 맞는 클릭 영역을 들고 있게 하려는 것이다.
  useLayoutEffect(() => {
    const inner = scrollInnerRef.current;
    const actions = actionsRef.current;
    if (!inner || !actions) return;
    const measure = () => setBodyHeight(inner.offsetHeight + actions.offsetHeight);
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(inner);
    ro.observe(actions);
    return () => ro.disconnect();
  }, [layout]);

  const fullHeight = layout
    ? Math.min(
        layout.barHeight + (notice ? layout.noticeHeight : 0) + bodyHeight,
        layout.expandedHeight
      )
    : 0;

  useEffect(() => {
    if (fullHeight > 0) invoke("notch_fit", { height: fullHeight }).catch(() => {});
  }, [fullHeight]);

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
  const fig = rep ? figureOf(rep) : { pct: null, borrowed: null };
  // 트레이 숫자를 숨긴 상태에서도 경고를 놓치지 않게 노치 숫자에 위험 색을 입힌다.
  const danger = isDanger(fig.pct, rep?.verdict ?? null, threshold);
  const open = mode !== "collapsed";
  const islandWidth = layout.notchWidth + layout.wing * 2;
  const tools = statuses.filter((s) => s.available);
  const height =
    mode === "full"
      ? fullHeight
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
          <span
            className={`notch-side notch-pct${fig.borrowed ? " borrowed" : ""}`}
            title={fig.borrowed?.label}
          >
            {fig.pct != null ? `${Math.round(fig.pct)}%` : "—"}
          </span>
          <span />
          <span className={`notch-side notch-activity ${activity?.state ?? "idle"}`}>
            {activity?.state === "working" && t(lang, "notch.working")}
            {activity?.state === "waiting" && t(lang, "notch.waiting")}
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

        <div
          className="notch-body"
          style={{ width: layout.expandedWidth }}
          aria-hidden={mode !== "full"}
        >
          <div className="notch-scroll">
            <div ref={scrollInnerRef}>
              <ul className="notch-tools">
                {tools.length === 0 && <li className="notch-empty">{t(lang, "noTools")}</li>}
                {tools.map((s) => (
                  <ToolRow key={s.tool} s={s} lang={lang} threshold={threshold} />
                ))}
              </ul>
              {/* 세션이 없거나 훅이 없으면 SessionList가 null이라 :empty로 접힌다. */}
              <section className="notch-sessions">
                {sessions === undefined && <SessionSkeleton lang={lang} />}
                {sessions && (
                  <SessionList
                    sessions={sessions}
                    lang={lang}
                    onLand={(s) =>
                      invoke("land_session", { tool: s.tool, session: s.session })
                        .then(() => invoke("notch_dismiss"))
                        .catch(() => {})
                    }
                  />
                )}
              </section>
            </div>
          </div>
          <nav className="notch-actions" ref={actionsRef}>
            <button onClick={() => invoke("open_main_window").catch(() => {})}>
              {t(lang, "notch.dashboard")}
            </button>
            <button onClick={() => invoke("open_history_window").catch(() => {})}>
              {t(lang, "history")}
            </button>
            <button onClick={() => invoke("open_inbox_window").catch(() => {})}>
              {t(lang, "inbox")}
              {unread > 0 && <span className="notch-badge">{unread}</span>}
            </button>
            <button onClick={() => invoke("open_settings_window").catch(() => {})}>
              {t(lang, "settings")}
            </button>
          </nav>
        </div>
      </div>
    </div>
  );
}
