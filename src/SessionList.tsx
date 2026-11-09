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

/** 훅이 하나도 설치돼 있지 않으면 null — 목록 자리를 아예 그리지 않는다. */
export function useSessions(active: boolean): HookSession[] | null {
  const [list, setList] = useState<HookSession[] | null>(null);
  useEffect(() => {
    if (!active) return;
    const poll = () =>
      invoke<SessionListPayload>("get_sessions")
        .then((p) => setList(p.installed ? p.sessions : null))
        .catch(() => {});
    poll();
    const id = setInterval(poll, ACTIVITY_POLL_MS);
    return () => clearInterval(id);
  }, [active]);
  return list;
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
    <ul className="session-list" aria-label={t(lang, "sessions.title")}>
      {sessions.map((s) => {
        const state = s.state === "justDone" && s.failed ? "failed" : s.state;
        // 갈 데가 없는 세션은 눌러도 아무 일이 없으니 버튼으로 만들지 않는다.
        const landable = s.target.kind !== "none";
        const body = (
          <>
            <span className={`session-dot ${state}`} aria-hidden />
            <span className="session-name">{s.project ?? s.tool}</span>
            <span className="session-state">{t(lang, `sessions.${state}`)}</span>
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
  );
}
