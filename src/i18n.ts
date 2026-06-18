// 프론트 다국어 — 경량 자체 구현 (한국어/English)
export type Lang = "ko" | "en";

const dict: Record<Lang, Record<string, string>> = {
  ko: {
    refresh: "새로고침",
    settings: "설정",
    loading: "불러오는 중…",
    noTools: "감지된 도구가 없습니다.",
    remaining: "남음",
    noLimit: "한도 미설정",
    used: "사용",
    burnRate: "소진 속도",
    eta: "예상 소진",
    today: "오늘",
    requests: "요청",
    messages: "메시지",
    weekly: "주간 {n}% 남음",
    lessThanMin: "1분 미만",
    resetSoon: "곧 리셋",
    afterReset: "{t} 후 리셋",
    "note.estimate": "추정치 — 무료티어 1000 req/day 가정",
    "note.no_limit": "한도 정보 없음 — 사용량/속도만 표시",
    osNotif: "OS 알림",
    permWarn: "시스템 알림 권한이 꺼져 있어 알림이 오지 않아요.",
    reqPerm: "권한 요청",
    openSysSettings: "시스템 설정 열기",
    permOk: "시스템 알림 권한 허용됨 ✓",
    alertThreshold: "경보 임계치",
    pctRemaining: "{n}% 남음",
    etaAlert: "예상 소진 경보",
    off: "끔",
    minBefore: "{n}분 전",
    resetAlert: "리셋 임박 알림",
    pollInterval: "새로고침 주기",
    sec: "{n}초",
    trayDisplay: "트레이 표시",
    trayAuto: "자동 (가장 임박한 도구)",
    monitorTools: "모니터링 도구",
    notDetected: " (미감지)",
    language: "언어",
    langAuto: "시스템 자동",
    autostart: "시작 시 자동 실행",
    settingsHint:
      "잔여율 또는 예상 소진 시간 중 하나라도 도달하면 알림을 보냅니다. 리셋 임박 알림은 반대로, 곧 리셋되는데 토큰이 많이 남아있을 때 \"지금 더 써도 된다\"고 알려줍니다.",
  },
  en: {
    refresh: "Refresh",
    settings: "Settings",
    loading: "Loading…",
    noTools: "No tools detected.",
    remaining: "left",
    noLimit: "No limit",
    used: "used",
    burnRate: "Burn rate",
    eta: "Runs out",
    today: "Today",
    requests: "requests",
    messages: "messages",
    weekly: "Weekly {n}% left",
    lessThanMin: "<1 min",
    resetSoon: "resets soon",
    afterReset: "resets in {t}",
    "note.estimate": "Estimate — assumes 1000 req/day free tier",
    "note.no_limit": "No limit info — usage/rate only",
    osNotif: "OS notifications",
    permWarn: "System notification permission is off, so alerts won't arrive.",
    reqPerm: "Request permission",
    openSysSettings: "Open System Settings",
    permOk: "System notification permission granted ✓",
    alertThreshold: "Alert threshold",
    pctRemaining: "{n}% left",
    etaAlert: "Runs-out alert",
    off: "Off",
    minBefore: "{n} min before",
    resetAlert: "Reset-soon alert",
    pollInterval: "Refresh interval",
    sec: "{n}s",
    trayDisplay: "Tray display",
    trayAuto: "Auto (most imminent)",
    monitorTools: "Monitored tools",
    notDetected: " (not detected)",
    language: "Language",
    langAuto: "System default",
    autostart: "Launch at login",
    settingsHint:
      "An alert fires when either the remaining percentage or the runs-out time is reached. The reset-soon alert is the opposite — it tells you to use the remaining tokens when a reset is near but plenty is left.",
  },
};

/** 설정값(ko/en) 우선, 없으면 브라우저 로케일로 결정. */
export function resolveLang(setting: string | null | undefined): Lang {
  if (setting === "ko" || setting === "en") return setting;
  return typeof navigator !== "undefined" &&
    navigator.language.toLowerCase().startsWith("ko")
    ? "ko"
    : "en";
}

/** 번역. {key} 형태 플레이스홀더를 params로 치환. */
export function t(
  lang: Lang,
  key: string,
  params?: Record<string, string | number>
): string {
  let s = dict[lang][key] ?? key;
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      s = s.replace(`{${k}}`, String(v));
    }
  }
  return s;
}
