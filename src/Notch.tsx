import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./Notch.css";

interface NotchLayout {
  notchWidth: number;
  wing: number;
  barHeight: number;
  expandedWidth: number;
  expandedHeight: number;
}

/** NSPanel 전환 프로토타입 — 섬 하나와 클릭 카운터만 둔다. */
export function NotchView() {
  const [layout, setLayout] = useState<NotchLayout | null>(null);
  const [clicks, setClicks] = useState(0);

  useEffect(() => {
    document.documentElement.style.background = "transparent";
    document.body.style.background = "transparent";
    invoke<NotchLayout>("notch_layout").then(setLayout).catch(() => {});
  }, []);

  if (!layout || layout.barHeight <= 0) return null;
  return (
    <div className="notch-root">
      <div
        className="notch-island"
        style={{
          width: layout.notchWidth + layout.wing * 2,
          height: layout.barHeight,
          gridTemplateColumns: `${layout.wing}px ${layout.notchWidth}px ${layout.wing}px`,
        }}
      >
        <span className="notch-side notch-left">proto</span>
        <span />
        <button className="notch-side notch-right" onClick={() => setClicks((n) => n + 1)}>
          {clicks}
        </button>
      </div>
    </div>
  );
}
