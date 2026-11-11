import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, resolveLang, type Lang } from "./i18n";
import { ICONS, Icon } from "./icons";
import {
  ACTIVITY_POLL_MS,
  formatCountdown,
  formatDuration,
  formatEta,
  formatObservedAgo,
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
  // 지난 관측값은 빌려 오지 않는다. 한눈에 보는 숫자가 몇 시간 전 값이면 지금 상황으로 읽힌다.
  const other = (s.accounts ?? [])
    .filter((a) => a.percentRemaining != null && a.observedAtMs == null)
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

/** 게이지 줄 라벨 — 좁은 칸이라 "세션 (5h)" 대신 "5h"처럼 짧게 쓴다. */
function windowShort(hours: number, lang: Lang): string {
  if (hours >= 24 * 28) return t(lang, "windowMonth");
  if (hours >= 24 * 7) return t(lang, "windowWeek");
  if (hours >= 24) return t(lang, "windowDay");
  return `${Number.isInteger(hours) ? hours : hours.toFixed(1)}h`;
}

function Gauge({
  label,
  pct,
  danger,
  resetIn,
  lang,
}: {
  label: string;
  pct: number | null;
  danger: boolean;
  resetIn: number | null;
  lang: Lang;
}) {
  return (
    <div className={`notch-gauge${danger ? " danger" : ""}`}>
      <span className="notch-gauge-label">{label}</span>
      <span className="notch-gauge-track" aria-hidden>
        <span
          className="notch-gauge-fill"
          style={{ width: `${Math.max(0, Math.min(100, pct ?? 0))}%` }}
        />
      </span>
      <span className="notch-gauge-pct">{pct != null ? `${Math.round(pct)}%` : "—"}</span>
      <span className="notch-gauge-reset">
        {resetIn != null && t(lang, "notch.resetIn", { t: formatCountdown(resetIn, lang) })}
      </span>
    </div>
  );
}

function ToolCard({
  s,
  lang,
  threshold,
}: {
  s: RunwayStatus;
  lang: Lang;
  threshold: number;
}) {
  const danger = isDanger(s.percentRemaining, s.verdict, threshold);
  const active = (s.accounts ?? []).find((a) => a.isActive) ?? null;
  // 주간 윈도우 도구(Grok)는 5h·7d 슬롯에 같은 값이 들어 있어 주간 줄을 따로 띄우지 않는다.
  const weekly =
    s.sevenDayRemaining != null && s.windowHours < 24 * 7 ? s.sevenDayRemaining : null;
  const verdict = s.verdict
    ? t(lang, s.verdict.key, {
        eta: formatDuration(s.verdict.etaMinutes, lang),
        reset: formatCountdown(s.verdict.resetMinutes, lang),
      })
    : s.etaMinutes != null
      ? t(lang, "notch.eta", { t: formatEta(s.etaMinutes, lang) })
      : null;
  return (
    <li className={`notch-card${danger ? " danger" : ""}`}>
      <div className="notch-card-head">
        <span className="notch-card-name">{s.tool}</span>
        {s.plan && <span className="notch-plan">{s.plan}</span>}
        <span className="notch-card-pct">
          {s.percentRemaining != null ? (
            <>
              <strong>{Math.round(s.percentRemaining)}</strong>%
              <small>{t(lang, "remaining")}</small>
            </>
          ) : (
            "—"
          )}
        </span>
      </div>
      <Gauge
        label={windowShort(s.windowHours, lang)}
        pct={s.percentRemaining}
        danger={s.percentRemaining != null && s.percentRemaining <= threshold}
        resetIn={minutesUntil(s.resetsAt)}
        lang={lang}
      />
      {weekly != null && (
        <Gauge
          label={t(lang, "windowWeek")}
          pct={weekly}
          danger={weekly <= threshold}
          resetIn={minutesUntil(active?.sevenDayResetsAt ?? null)}
          lang={lang}
        />
      )}
      {verdict && <p className={`notch-verdict ${s.verdict?.level ?? ""}`}>{verdict}</p>}
      {(s.accounts ?? []).length > 1 && (
        <ul className="notch-accts">
          {s.accounts.map((a) => {
            const stale =
              a.observedAtMs != null ? formatObservedAgo(a.observedAtMs, lang) : null;
            // 지난 값은 위험 색을 쓰지 않는다 — 지금 바닥났다는 근거가 아니다.
            const low =
              !stale && a.percentRemaining != null && a.percentRemaining <= threshold;
            return (
              <li
                key={a.id}
                className={`notch-acct${a.isActive ? " active" : ""}${stale ? " stale" : ""}`}
                title={stale ?? undefined}
              >
                <span className="notch-acct-name" title={stale ? undefined : a.label}>
                  {a.label}
                  {stale && <span className="sr-only"> ({stale})</span>}
                </span>
                {a.percentRemaining != null ? (
                  <>
                    <span className={`notch-gauge-track mini${low ? " danger" : ""}`} aria-hidden>
                      <span
                        className="notch-gauge-fill"
                        style={{ width: `${Math.max(0, Math.min(100, a.percentRemaining))}%` }}
                      />
                    </span>
                    <span className={`notch-acct-pct${low ? " danger" : ""}`}>
                      {Math.round(a.percentRemaining)}%
                    </span>
                  </>
                ) : (
                  <span
                    className="notch-acct-note"
                    title={t(lang, a.note ?? "acctFailed", { tool: s.tool })}
                  >
                    {t(lang, a.note ?? "acctFailed", { tool: s.tool })}
                  </span>
                )}
              </li>
            );
          })}
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
  const scrollRef = useRef<HTMLDivElement>(null);
  // 최대 높이를 넘겨 스크롤이 생겼을 때만 가려진 쪽 끝을 흐리게 해 더 있다는 걸 알린다.
  const [edges, setEdges] = useState({ top: false, bottom: false });
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

  useEffect(() => {
    const el = scrollRef.current;
    const inner = scrollInnerRef.current;
    if (!el || !inner) return;
    const update = () => {
      const top = el.scrollTop > 1;
      const bottom = el.scrollTop + el.clientHeight < el.scrollHeight - 1;
      setEdges((prev) => (prev.top === top && prev.bottom === bottom ? prev : { top, bottom }));
    };
    update();
    el.addEventListener("scroll", update, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(el);
    ro.observe(inner);
    return () => {
      el.removeEventListener("scroll", update);
      ro.disconnect();
    };
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
        className={`notch-island${open ? " open" : ""}${mode === "full" ? " full" : ""}${
          danger ? " danger" : ""
        }`}
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
          <div
            className={`notch-scroll${edges.top ? " fade-top" : ""}${edges.bottom ? " fade-bottom" : ""}`}
            ref={scrollRef}
          >
            <div className="notch-content" ref={scrollInnerRef}>
              <ul className="notch-cards">
                {tools.length === 0 && <li className="notch-empty">{t(lang, "noTools")}</li>}
                {tools.map((s) => (
                  <ToolCard key={s.tool} s={s} lang={lang} threshold={threshold} />
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
              <Icon d={ICONS.dashboard} />
              {t(lang, "notch.dashboard")}
            </button>
            <button onClick={() => invoke("open_history_window").catch(() => {})}>
              <Icon d={ICONS.history} />
              {t(lang, "history")}
            </button>
            <button onClick={() => invoke("open_inbox_window").catch(() => {})}>
              <Icon d={ICONS.inbox} />
              {t(lang, "inbox")}
              {unread > 0 && <span className="notch-badge">{unread}</span>}
            </button>
            <button onClick={() => invoke("open_settings_window").catch(() => {})}>
              <Icon d={ICONS.settings} />
              {t(lang, "settings")}
            </button>
          </nav>
        </div>
      </div>
    </div>
  );
}
