import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { t, type Lang } from "./i18n";
import { ACTIVITY_POLL_MS, formatAge } from "./App";
import "./SessionList.css";

/** Rust `activity::HookSession`과 짝. */
export interface HookSession {
  tool: string;
  session: string;
  state: "working" | "waiting" | "justDone" | "idle";
  failed: boolean;
  sinceMs: number;
  project: string | null;
  target: { kind: string };
  configDir: string | null;
}

interface SessionListPayload {
  installed: boolean;
  sessions: HookSession[];
}

/**
 * 훅이 하나도 설치돼 있지 않으면 null — 목록 자리를 아예 그리지 않는다.
 * 아직 한 번도 못 받았으면 undefined라 펼친 화면이 자리표시를 띄울 수 있다.
 */
export function useSessions(active: boolean): HookSession[] | null | undefined {
  const [list, setList] = useState<HookSession[] | null | undefined>(undefined);
  useEffect(() => {
    const poll = () =>
      invoke<SessionListPayload>("get_sessions")
        .then((p) => setList(p.installed ? p.sessions : null))
        // 첫 요청부터 실패하면 자리표시가 영영 남으니 그때만 없는 것으로 친다.
        .catch(() => setList((prev) => (prev === undefined ? null : prev)));
    // 접혀 있을 때도 한 번은 받아둔다 — 펼친 뒤에 받으면 목록이 늦게 붙으며 섬 높이가 튄다.
    poll();
    if (!active) return;
    const id = setInterval(poll, ACTIVITY_POLL_MS);
    return () => clearInterval(id);
  }, [active]);
  return list;
}

const SKELETON_DELAY_MS = 150;

/** 목록을 아직 못 받았을 때의 자리표시. 응답이 금방 오면 깜빡이지 않게 늦게 띄운다. */
export function SessionSkeleton({ lang }: { lang: Lang }) {
  const [shown, setShown] = useState(false);
  useEffect(() => {
    const id = setTimeout(() => setShown(true), SKELETON_DELAY_MS);
    return () => clearTimeout(id);
  }, []);
  if (!shown) return null;
  return (
    <>
      <h3 className="session-heading">{t(lang, "sessions.title")}</h3>
      <ul className="session-list" aria-busy="true" aria-label={t(lang, "sessions.title")}>
        {[0, 1].map((i) => (
          <li key={i}>
            <div className="session-row skeleton">
              <span className="session-dot" aria-hidden />
              <span className="skeleton-bar" aria-hidden />
            </div>
          </li>
        ))}
      </ul>
    </>
  );
}

export function SessionList({
  sessions,
  lang,
  onLand,
}: {
  sessions: HookSession[];
  lang: Lang;
  onLand: (s: HookSession) => void;
}) {
  // 빈 목록도 아무것도 그리지 않아야 노치의 세션 칸이 :empty로 접힌다.
  if (sessions.length === 0) return null;
  return (
    <>
      <h3 className="session-heading">
        {t(lang, "sessions.title")}
        <span className="session-count">{sessions.length}</span>
      </h3>
      <ul className="session-list" aria-label={t(lang, "sessions.title")}>
        {sessions.map((s) => {
          const state = s.state === "justDone" && s.failed ? "failed" : s.state;
          // 갈 데가 없는 세션은 눌러도 아무 일이 없으니 버튼으로 만들지 않는다.
          const landable = s.target.kind !== "none";
          const body = (
            <>
              <span className={`session-dot ${state}`} aria-hidden />
              <span className="session-name">{s.project ?? s.tool}</span>
              <span className={`session-state ${state}`}>{t(lang, `sessions.${state}`)}</span>
              <span className="session-age">{formatAge(s.sinceMs, lang)}</span>
            </>
          );
          return (
            <li key={`${s.tool}:${s.session}`}>
              {landable ? (
                <button className="session-row" title={s.tool} onClick={() => onLand(s)}>
                  {body}
                </button>
              ) : (
                <div className="session-row" title={s.tool}>
                  {body}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </>
  );
}
