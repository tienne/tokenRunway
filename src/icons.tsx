// 노치와 대시보드, 알림함이 같이 쓰는 선 아이콘. 이모지는 OS마다 모양과 색이 달라 화면 톤과 안 맞아서 바꿨다.
export const ICONS = {
  dashboard: "M2.5 2.5h4.5v4.5h-4.5zM9 2.5h4.5v4.5h-4.5zM2.5 9h4.5v4.5h-4.5zM9 9h4.5v4.5h-4.5z",
  history: "M2.5 13.5h11M4.5 11V8M8 11V4.5M11.5 11V6.5",
  inbox: "M4 11.5V7a4 4 0 0 1 8 0v4.5l1 1H3zM6.5 14h3",
  settings:
    "M8 5.5a2.5 2.5 0 1 0 0 5a2.5 2.5 0 1 0 0-5zM8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4",
  trash: "M2.5 4.5h11M6.5 4.5V2.5h3v2M4 4.5l.7 9h6.6l.7-9M6.7 7v4M9.3 7v4",
  check: "M3.5 8.5l3 3 6-7",
  download: "M8 2.5v8M4.5 7L8 10.5 11.5 7M3 13.5h10",
  pause: "M6 4v8M10 4v8",
  cross: "M4.5 4.5l7 7M11.5 4.5l-7 7",
};

export function Icon({ d, size = 13 }: { d: string; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" aria-hidden>
      <path
        d={d}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
