import { useEffect, useState, useCallback, useRef } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import {
  getCurrentWindow,
  LogicalSize,
  LogicalPosition,
} from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import { openUrl } from "@tauri-apps/plugin-opener";
import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { t, resolveLang, type Lang } from "./i18n";
import petIdle from "./assets/pet/default/idle.svg";
import petGood from "./assets/pet/default/good.svg";
import petWarn from "./assets/pet/default/warn.svg";
import petDanger from "./assets/pet/default/danger.svg";
import petSpriteSheet from "./assets/pet/default/sprite.webp";
import "./App.css";

type PetLevel = "idle" | "good" | "warn" | "danger";

const BUILTIN_PET: Record<PetLevel, string> = {
  idle: petIdle,
  good: petGood,
  warn: petWarn,
  danger: petDanger,
};

/** 기본 내장 pet — "루니봇", Token Runway 전용으로 만든 오리지널 3D 로봇 새 캐릭터
 * (192×208, 8열×9행, 표준 Codex pet 레이아웃과 동일한 9개 애니메이션).
 * sheet는 이미 번들된 정적 자산 URL이라 convertFileSrc가 필요 없다
 * (커스텀 번들의 sheet는 파일시스템 절대경로라 convertFileSrc가 필요한 것과 다르다). */
const BUILTIN_SPRITE: PetSprite = {
  sheet: petSpriteSheet,
  frameWidth: 192,
  frameHeight: 208,
  columns: 8,
  rows: 9,
  fps: 8,
  defaultAnimation: "idle",
  animations: {
    idle: { row: 0, frames: 6, frameDurationsMs: [450, 450, 450, 450, 450, 450] },
    "running-right": {
      row: 1,
      frames: 8,
      frameDurationsMs: [120, 120, 120, 120, 120, 120, 120, 220],
    },
    "running-left": {
      row: 2,
      frames: 8,
      frameDurationsMs: [120, 120, 120, 120, 120, 120, 120, 220],
    },
    waving: { row: 3, frames: 4, frameDurationsMs: [140, 140, 140, 280] },
    jumping: { row: 4, frames: 5, frameDurationsMs: [150, 150, 150, 150, 260] },
    failed: {
      row: 5,
      frames: 8,
      frameDurationsMs: [140, 140, 140, 140, 140, 140, 140, 240],
    },
    waiting: { row: 6, frames: 6, frameDurationsMs: [220, 220, 220, 220, 220, 320] },
    running: { row: 7, frames: 6, frameDurationsMs: [160, 160, 160, 160, 160, 280] },
    review: { row: 8, frames: 6, frameDurationsMs: [200, 200, 200, 200, 200, 300] },
  },
};

/** 쉴 때 idle 대신 아주 가끔 섞어 쓰는 포즈 후보 — 번들이 실제로 갖고 있는 것만 쓴다. */
const REST_POSE_CANDIDATES = ["waving", "waiting", "review"];
/** 쉬는 구간에서 idle이 아닌 특별 포즈가 나올 확률. 나머지는 전부 idle. */
const REST_SPECIAL_CHANCE = 0.12;

/** pet 걷기 속도 (논리픽셀/틱) — 경보 레벨이 급할수록 바쁘게 움직인다. */
const PET_SPEED: Record<PetLevel, number> = {
  idle: 0.4,
  good: 0.5,
  warn: 1.0,
  danger: 1.8,
};
/** 배율 1.0일 때 pet 한 변(논리 px). Rust의 PET_WINDOW_BASE(100)에서 여백 4px씩 뺀 값. */
const PET_BASE_SIZE = 96;
const PET_TICK_MS = 50;
/** 활동 상태 폴링 주기 — 잔여율(30초)과 달리 이 정도는 돼야 반응이 살아 있어 보인다.
 * get_activity는 네트워크·전체 파싱 없이 파일 꼬리만 읽어 이 주기를 견딘다. */
const ACTIVITY_POLL_MS = 1500;
/** "다 됐어요" 말풍선이 떠 있는 시간(ms). */
const DONE_FLASH_MS = 6000;
/** 에이전트가 도는 동안 한 번 산책하는 시간 범위(ms). */
const STROLL_MS_MIN = 5000;
const STROLL_MS_MAX = 9000;
/** 산책 사이 제자리 작업 구간(ms) — 하루종일 도는 날 pet이 얼어 있지 않게, 다만 작업
 * 포즈가 주인공이도록 드물게. */
const STROLL_GAP_MIN = 30_000;
const STROLL_GAP_MAX = 60_000;

/** 스프라이트시트 안의 애니메이션 한 줄: row(0-based y) + frames(가로 프레임 수). */
interface SpriteAnimation {
  row: number;
  frames: number;
  frameDurationsMs?: number[];
}

/** Codex pet 포맷(pet.json+spritesheet) — 프레임 격자·애니메이션 목록. */
interface PetSprite {
  sheet: string;
  frameWidth: number;
  frameHeight: number;
  columns: number;
  rows: number;
  fps: number;
  defaultAnimation?: string;
  animations: Record<string, SpriteAnimation>;
}

interface PetBundle {
  id: string;
  name: string;
  dir: string;
  states?: { idle: string; good: string; warn: string; danger: string };
  sprite?: PetSprite;
}

/** 전체 도구 중 최악의 verdict — pet이 반응할 상태. danger가 하나라도 있으면 즉시 확정. */
function worstPetLevel(statuses: RunwayStatus[]): PetLevel {
  let worst: PetLevel = "idle";
  for (const s of statuses) {
    const level = s.verdict?.level;
    if (level === "danger") return "danger";
    if (level === "warn") worst = "warn";
    else if (level === "good" && worst === "idle") worst = "good";
  }
  return worst;
}

/** 에이전트 활동 상태 — 잔여율(PetLevel)과 독립된 축. Rust `activity::ActivityState`와 짝. */
type ActivityState = "working" | "justDone" | "idle";

interface AgentActivity {
  tool: string;
  state: ActivityState;
  sinceMs: number;
  failed: boolean;
}

/** pet이 따라갈 도구 하나 — 도는 게 있으면 그중 가장 최근, 없으면 가장 최근에 끝난 것.
 * 여러 도구를 동시에 굴려도 pet은 하나뿐이라 제일 최근에 움직인 쪽을 대표로 보여준다. */
function leadActivity(list: AgentActivity[]): AgentActivity | null {
  const pick = (state: ActivityState) =>
    list
      .filter((a) => a.state === state)
      .sort((a, b) => b.sinceMs - a.sinceMs)[0] ?? null;
  return pick("working") ?? pick("justDone");
}

function petImageSrc(level: PetLevel, bundle: PetBundle | null): string {
  const custom = bundle?.states?.[level];
  return custom ? convertFileSrc(custom) : BUILTIN_PET[level];
}

function petErrorKey(raw: string): string {
  return raw.split(":")[0];
}

// --- 스프라이트시트 애니메이션 (Codex pet 포맷) ---
// Orca(stablyai/orca)의 sprite-animation-css.ts를 그대로 포팅 — 프레임별 보유시간이
// 균일하지 않은(idle이 마지막 프레임에서 오래 쉬는 등) Codex 페이싱은 steps()로 표현이
// 안 돼서, 프레임마다 step-end 정지점을 하나씩 찍은 @keyframes를 만든다.
const MAX_FRAME_DURATION_MS = 60_000;
const SPRITE_BASE_HEIGHT = 88; // 배율 1.0일 때 스프라이트 표시 높이

function validFrameDurations(
  ms: number[] | undefined,
  frames: number
): number[] | null {
  if (
    Array.isArray(ms) &&
    ms.length === frames &&
    ms.every((v) => Number.isFinite(v) && v > 0 && v <= MAX_FRAME_DURATION_MS)
  ) {
    return ms;
  }
  return null;
}

function stepEndStops(
  durations: number[],
  totalMs: number,
  frameWidth: number,
  scale: number,
  rowOffsetY: number
): string[] | null {
  const stops: string[] = [];
  let elapsed = 0;
  let prevPct = -1;
  for (let i = 0; i < durations.length; i++) {
    const pct = +((elapsed / totalMs) * 100).toFixed(4);
    if (pct <= prevPct || pct >= 100) return null;
    prevPct = pct;
    const x = -(i * frameWidth * scale);
    stops.push(`${pct}% { background-position: ${x}px ${rowOffsetY}px; }`);
    elapsed += durations[i];
  }
  return stops;
}

function buildSpriteAnimationCss(opts: {
  keyframesId: string;
  frames: number;
  fps: number;
  frameWidth: number;
  scale: number;
  rowOffsetY: number;
  frameDurationsMs?: number[];
}): { keyframesCss: string; animationCss: string } {
  const name = `pet-${opts.keyframesId}`;
  const durations = validFrameDurations(opts.frameDurationsMs, opts.frames);
  if (durations) {
    const totalMs = durations.reduce((s, v) => s + v, 0);
    const stops = stepEndStops(
      durations,
      totalMs,
      opts.frameWidth,
      opts.scale,
      opts.rowOffsetY
    );
    if (stops) {
      return {
        keyframesCss: `@keyframes ${name} { ${stops.join(" ")} }`,
        animationCss: `${name} ${totalMs / 1000}s step-end infinite`,
      };
    }
  }
  const duration = Math.max(0.1, opts.frames / Math.max(0.1, opts.fps));
  const endX = -(opts.frames * opts.frameWidth * opts.scale);
  return {
    keyframesCss: `@keyframes ${name} { from { background-position: 0px ${opts.rowOffsetY}px; } to { background-position: ${endX}px ${opts.rowOffsetY}px; } }`,
    animationCss: `${name} ${duration}s steps(${opts.frames}) infinite`,
  };
}

/** 상태(danger 등)와 이동 방향에 맞는 애니메이션을 고른다. 번들에 없는 이름은 건너뛴다.
 * restPose는 쉬는 동안 idle 대신 쓸 포즈 이름(그 쉬는 구간 동안 고정) — 안 쉬는 중이면 null.
 * activity는 에이전트 활동 상태로, 잔여율 기반 포즈보다 우선한다 — 지금 뭘 하는지가
 * 얼마 남았는지보다 눈에 먼저 들어와야 해서다. */
function pickSpriteAnimation(opts: {
  sprite: PetSprite;
  level: PetLevel;
  dir: 1 | -1;
  dragging: boolean;
  restPose: string | null;
  alerting: boolean;
  activity: AgentActivity | null;
  strolling: boolean;
}): string {
  const { sprite, level, dir, dragging, restPose, alerting, activity, strolling } = opts;
  const has = (n: string) => Object.prototype.hasOwnProperty.call(sprite.animations, n);
  if (dragging && has("jumping")) return "jumping";
  if (alerting && has("jumping")) return "jumping";
  // 작업 중엔 제자리 작업 포즈가 기본이고, 산책 구간에서만 방향별 걷기로 내려간다.
  if (activity?.state === "working" && !strolling && has("running")) return "running";
  if (activity?.state === "justDone") {
    if (activity.failed && has("failed")) return "failed";
    if (has("waving")) return "waving";
  }
  // 쉬는 중일 때만 포즈를 바꾼다 — 걷는 도중에 상태별 포즈가 끼어들면 걸음이 끊겨 보인다.
  if (restPose && has(restPose)) return restPose;
  // 산책 중엔 잔여율이 idle이어도 걷는 그림이어야 한다.
  if (level === "idle" && !strolling && has("idle")) return "idle";
  // 여기부터는 이동 중 — 방향별 걷기로 고정한다.
  const dirName = dir === 1 ? "running-right" : "running-left";
  if (has(dirName)) return dirName;
  if (has("running")) return "running";
  if (sprite.defaultAnimation && has(sprite.defaultAnimation)) {
    return sprite.defaultAnimation;
  }
  if (has("idle")) return "idle";
  return Object.keys(sprite.animations)[0];
}

/** 화면 전체를 돌아다니는 데스크톱 pet 오버레이 창의 콘텐츠. */
/** 클릭으로 칠지, 드래그로 칠지 가르는 최소 이동 거리(px). */
const DRAG_THRESHOLD = 4;
/** 배회 반경 — 화면 전체가 아니라 홈 중심에서 이 거리 안에서만 랜덤하게 돌아다닌다. */
const WANDER_RADIUS = 160;
/** 목표 지점에 도착했다고 볼 거리(px). */
const WANDER_ARRIVE_DIST = 4;
/** 도착 후 idle로 쉬는 시간 범위(ms) — 이 사이에서 매번 랜덤.
 * 걷는 데 한 번에 10초쯤 걸려서, 이 값이 짧으면 한가할 때가 에이전트 도는 중보다
 * 훨씬 많이 돌아다니게 된다(신호가 거꾸로 간다). 쉴 때는 늘어져 있도록 길게 잡는다. */
const REST_MS_MIN = 10_000;
const REST_MS_MAX = 25_000;
/** 경보 말풍선이 떠 있는 시간(ms). */
const ALERT_FLASH_MS = 6000;

/** pet이 설 수 있는 영역 하나(모니터 한 대의 작업영역, 논리 px). */
interface Area {
  minX: number;
  maxX: number;
  minY: number;
  maxY: number;
}

function inArea(x: number, y: number, a: Area): boolean {
  return x >= a.minX && x <= a.maxX && y >= a.minY && y <= a.maxY;
}

/** 점이 속한 영역. 어디에도 안 속하면(모니터 사이 빈 공간) null. */
function areaAt(x: number, y: number, areas: Area[]): Area | null {
  return areas.find((a) => inArea(x, y, a)) ?? null;
}

/** 점을 가장 가까운 영역 안으로 끌어당긴다 — 드래그로 모니터 밖에 놓았을 때. */
function clampToAreas(x: number, y: number, areas: Area[]): { x: number; y: number } {
  if (areas.length === 0) return { x, y };
  let best = { x, y };
  let bestDist = Infinity;
  for (const a of areas) {
    const cx = Math.min(a.maxX, Math.max(a.minX, x));
    const cy = Math.min(a.maxY, Math.max(a.minY, y));
    const d = Math.hypot(cx - x, cy - y);
    if (d < bestDist) {
      bestDist = d;
      best = { x: cx, y: cy };
    }
  }
  return best;
}

/** center에서 반경 WANDER_RADIUS 안의 임의 지점을 고르되, 지금 있는 모니터 안에서만 고른다 —
 * 스스로 모니터를 넘나들지는 않는다(옮기는 건 사용자가 드래그로).
 * 몇 번 뽑아봐도 안 들어오면(중심이 모서리 쪽) 중심 자체를 목표로 삼는다. */
function pickWanderTarget(
  center: { x: number; y: number },
  areas: Area[]
): { x: number; y: number } {
  const here = areaAt(center.x, center.y, areas);
  for (let i = 0; i < 8; i++) {
    const angle = Math.random() * Math.PI * 2;
    const dist = Math.random() * WANDER_RADIUS;
    const x = center.x + Math.cos(angle) * dist;
    const y = center.y + Math.sin(angle) * dist;
    if (here ? inArea(x, y, here) : areaAt(x, y, areas)) {
      return { x, y };
    }
  }
  return { x: center.x, y: center.y };
}

/** 쉬는 구간 하나에서 쓸 포즈. 대부분 idle이고, REST_SPECIAL_CHANCE 확률로만 다른 포즈를
 * 섞는다. danger일 때는 failed도 후보에 넣어 상태가 드러나게 한다.
 * 후보가 번들에 하나도 없으면(순수 idle만 있는 번들) "idle"로 고정. */
function pickRestPose(sprite: PetSprite, level: PetLevel): string {
  if (Math.random() >= REST_SPECIAL_CHANCE) return "idle";
  const candidates =
    level === "danger" ? [...REST_POSE_CANDIDATES, "failed"] : REST_POSE_CANDIDATES;
  const available = candidates.filter((name) =>
    Object.prototype.hasOwnProperty.call(sprite.animations, name)
  );
  if (available.length === 0) return "idle";
  return available[Math.floor(Math.random() * available.length)];
}

function PetOverlay() {
  const [level, setLevel] = useState<PetLevel>("idle");
  const [bundle, setBundle] = useState<PetBundle | null>(null);
  const [src, setSrc] = useState(() => petImageSrc("idle", null));
  const [dir, setDir] = useState<1 | -1>(1);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [dragging, setDragging] = useState(false);
  // 쉬는 동안 보여줄 포즈 이름(idle/waving/waiting 등) — 안 쉬는 중이면 null.
  const [restPose, setRestPose] = useState<string | null>(null);
  // 경보(소진·예상소진·리셋임박)가 막 떴을 때 잠깐 보여줄 말풍선 — OS 알림 권한이
  // 없어도 놓치지 않도록 하는 보완 채널. null이면 평소 상태.
  const [alertFlash, setAlertFlash] = useState<{ tool: string; kind: string } | null>(null);
  // 지금 도는(또는 막 끝난) 에이전트. 아무 도구도 안 움직이면 null.
  const [activity, setActivity] = useState<AgentActivity | null>(null);
  // 턴이 끝난 순간 잠깐 띄우는 말풍선 — 잔여율 경보(alertFlash)보다 우선순위가 낮다.
  const [doneFlash, setDoneFlash] = useState<{ tool: string; failed: boolean } | null>(null);
  // 배회 루프는 [level, bundle]로만 다시 만들어지므로 활동 상태는 ref로 읽는다 —
  // 1.5초 폴링마다 interval을 새로 걸면 걸음이 끊긴다.
  const activityRef = useRef<AgentActivity | null>(null);
  // 작업 중 산책 구간인지. 걷는 그림을 쓸지 작업 포즈를 쓸지 가른다.
  const [strolling, setStrolling] = useState(false);
  const strollingRef = useRef(false);
  /** 지금 산책이 끝나는 시각. 0이면 산책 중이 아니다. */
  const strollUntilRef = useRef(0);
  /** 다음 산책을 시작할 수 있는 가장 이른 시각. 0이면 작업이 막 시작돼 아직 안 정해졌다. */
  const nextStrollAtRef = useRef(0);
  const dirRef = useRef<1 | -1>(1);
  const xRef = useRef(0);
  const yRef = useRef(700);
  // 현재 배율 기준으로 pet이 설 수 있는 곳 전부. 비어 있으면(모니터 조회 실패)
  // 클램프를 하지 않는다 — 임의의 기본 사각형에 갇히는 것보다 자유로운 편이 낫다.
  const areasRef = useRef<Area[]>([]);
  // 표시 배율 — 설정에서 바꾸면 창 크기와 스프라이트가 같이 커진다.
  const [scale, setScale] = useState(1);
  const petSize = PET_BASE_SIZE * scale;
  // 배회 중심 — 이 지점 반경 WANDER_RADIUS 안에서만 목표를 고른다. 드래그로 옮기면 재설정.
  const centerRef = useRef({ x: 0, y: 700 });
  // 지금 향해 걷고 있는 목표 지점.
  const targetRef = useRef({ x: 0, y: 700 });
  // 목표에 도착한 뒤 이 시각(ms)까지는 안 움직이고 쉰다. 0이면 쉬는 중 아님.
  const restUntilRef = useRef(0);
  const restPoseRef = useRef<string | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);

  // 드래그 상태 — 패트롤 루프와 같은 xRef/yRef를 공유해 손을 떼면 그 자리에서 이어걷는다.
  const draggingRef = useRef(false);
  const dragMovedRef = useRef(false);
  const dragOffsetRef = useRef({ x: 0, y: 0 });
  const dragBaselineXRef = useRef(0);
  const activePointerRef = useRef<number | null>(null);

  useEffect(() => {
    document.documentElement.style.background = "transparent";
    document.body.style.background = "transparent";
  }, []);

  // 상태 폴링 — Dashboard와 동일한 get_runway 재사용, 새 이벤트 채널 없음.
  useEffect(() => {
    async function poll() {
      try {
        const [statuses, s] = await Promise.all([
          invoke<RunwayStatus[]>("get_runway"),
          invoke<Settings>("get_settings"),
        ]);
        setLevel(worstPetLevel(statuses));
        setBundle(s.petBundles.find((b) => b.id === s.activePetBundleId) ?? null);
        setScale(s.petScale ?? 1);
        setLang(resolveLang(s.language));
      } catch {
        /* noop */
      }
    }
    poll();
    const id = setInterval(poll, 30_000);
    return () => clearInterval(id);
  }, []);

  // 활동 상태 폴링 — 잔여율과 주기가 다르다(1.5초). 턴이 끝나는 순간을 잡아 말풍선을
  // 띄우는데, 같은 턴에 두 번 띄우지 않도록 sinceMs로 구분한다.
  useEffect(() => {
    let lastDoneAt = 0;
    let flashTimer = 0;
    async function poll() {
      try {
        const lead = leadActivity(await invoke<AgentActivity[]>("get_activity"));
        activityRef.current = lead;
        setActivity(lead);
        if (lead?.state === "justDone" && lead.sinceMs !== lastDoneAt) {
          lastDoneAt = lead.sinceMs;
          setDoneFlash({ tool: lead.tool, failed: lead.failed });
          window.clearTimeout(flashTimer);
          flashTimer = window.setTimeout(() => setDoneFlash(null), DONE_FLASH_MS);
        }
      } catch {
        /* noop */
      }
    }
    poll();
    const id = setInterval(poll, ACTIVITY_POLL_MS);
    return () => {
      clearInterval(id);
      window.clearTimeout(flashTimer);
    };
  }, []);

  // 경보 발생 순간을 Rust에서 직접 통지받아 말풍선 + 점프로 반짝인다 —
  // OS 알림 권한이 없어도(또는 그냥 놓쳤어도) 데스크톱에서 눈에 띄게.
  useEffect(() => {
    const unlisten = listen<{ tool: string; kind: string }>("pet-alert", (event) => {
      setAlertFlash(event.payload);
      window.setTimeout(() => setAlertFlash(null), ALERT_FLASH_MS);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    // 내장 스프라이트(bundle===null)나 커스텀 스프라이트는 이 정지 이미지 state가 필요 없다 —
    // 커스텀 "정지 이미지 4장" 번들(states만 있고 sprite는 없는 경우)에만 쓰인다.
    if (bundle && !bundle.sprite) {
      setSrc(petImageSrc(level, bundle));
    }
  }, [level, bundle]);

  // pet이 설 수 있는 영역을 Rust에서 받아온다 — 연결된 모니터 전부의 작업영역이다.
  // 종료 시점에 저장해둔 위치가 있으면(petLastX/Y) 거기서 이어서 시작한다.
  useEffect(() => {
    invoke<Settings>("get_settings")
      .then(async (s) => {
        const size = PET_BASE_SIZE * (s.petScale ?? 1);
        const areas = await invoke<Area[]>("pet_areas", { petSize: size });
        if (areas.length === 0) return;
        areasRef.current = areas;
        if (s.petLastX != null && s.petLastY != null) {
          const p = clampToAreas(s.petLastX, s.petLastY, areas);
          xRef.current = p.x;
          yRef.current = p.y;
        } else {
          xRef.current = areas[0].minX;
          yRef.current = areas[0].maxY;
        }
        centerRef.current = { x: xRef.current, y: yRef.current };
        targetRef.current = pickWanderTarget(centerRef.current, areas);
        getCurrentWindow()
          .setPosition(new LogicalPosition(xRef.current, yRef.current))
          .catch(() => {});
      })
      .catch((e) => console.warn("[pet] 영역 초기화 실패", e));
  }, []);

  // 설정 창에서 배율을 바꾸면 폴링(30초)을 기다리지 않고 즉시 반영한다.
  useEffect(() => {
    const un = listen("settings-changed", () => {
      invoke<Settings>("get_settings")
        .then((s) => setScale(s.petScale ?? 1))
        .catch(() => {});
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // 배율이 바뀌면 설 수 있는 영역이 줄거나 늘어난다 — 다시 받아 현재 위치를 그 안으로.
  useEffect(() => {
    let alive = true;
    invoke<Area[]>("pet_areas", { petSize })
      .then((areas) => {
        if (!alive || areas.length === 0) return;
        areasRef.current = areas;
        const p = clampToAreas(xRef.current, yRef.current, areas);
        xRef.current = p.x;
        yRef.current = p.y;
        centerRef.current = clampToAreas(centerRef.current.x, centerRef.current.y, areas);
        targetRef.current = pickWanderTarget(centerRef.current, areas);
        getCurrentWindow().setPosition(new LogicalPosition(p.x, p.y)).catch(() => {});
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [petSize]);

  // 배회 루프 — 목표 지점을 향해 상하좌우 대각선 자유롭게 걷다가, 도착하면 잠깐(REST_MS)
  // idle로 멈춰 쉬고, 그다음 홈 중심 반경 WANDER_RADIUS 안에서 새 목표를 뽑아 다시 걷는다.
  // 드래그 중이거나 idle(할 일 없음) 레벨일 땐 항상 멈춘다.
  // 에이전트가 도는 중엔 대체로 제자리에서 작업 포즈를 보여준다 — 걸으면 방향별 걷기
  // 그림에 가려 "지금 일하고 있다"가 안 읽힌다. 다만 하루종일 도는 날 계속 얼어 있으면
  // 그게 더 답답하니 STROLL_GAP마다 한 번씩 짧게(STROLL_MS) 산책을 끼운다.
  // 턴이 막 끝난(justDone) 동안은 손을 흔들어야 하니 산책하지 않는다.
  // 정지 이미지 모드(커스텀 4장 번들)만 좌우반전 CSS가 필요 — 스프라이트는 방향별 그림이 따로 있다.
  useEffect(() => {
    const win = getCurrentWindow();
    const isStaticImageMode = !!bundle && !bundle.sprite;
    const spriteForRest = bundle ? bundle.sprite : BUILTIN_SPRITE;
    const id = setInterval(() => {
      if (draggingRef.current) return;
      const now = Date.now();
      const activity = activityRef.current;
      const working = activity?.state === "working";
      if (!working && strollingRef.current) {
        strollingRef.current = false;
        strollUntilRef.current = 0;
        nextStrollAtRef.current = 0;
        setStrolling(false);
      }
      if (activity !== null && !working) return; // 막 끝남 — 제자리에서 손 흔드는 중
      if (working) {
        if (!strollingRef.current) {
          // 작업이 막 시작됐으면 첫 산책까지의 간격을 정한다.
          if (nextStrollAtRef.current === 0) {
            nextStrollAtRef.current =
              now + STROLL_GAP_MIN + Math.random() * (STROLL_GAP_MAX - STROLL_GAP_MIN);
            return;
          }
          if (now < nextStrollAtRef.current) return; // 아직 제자리 작업 구간
          strollingRef.current = true;
          strollUntilRef.current =
            now + STROLL_MS_MIN + Math.random() * (STROLL_MS_MAX - STROLL_MS_MIN);
          targetRef.current = pickWanderTarget(centerRef.current, areasRef.current);
          restUntilRef.current = 0; // 짧은 산책이라 중간에 쉬지 않는다
          setStrolling(true);
        } else if (now >= strollUntilRef.current) {
          strollingRef.current = false;
          strollUntilRef.current = 0;
          nextStrollAtRef.current =
            now + STROLL_GAP_MIN + Math.random() * (STROLL_GAP_MAX - STROLL_GAP_MIN);
          setStrolling(false);
          return;
        }
      } else if (level === "idle") {
        return;
      }
      if (now < restUntilRef.current) {
        return; // 쉬는 중 — 움직이지 않는다.
      }
      if (restPoseRef.current !== null) {
        restPoseRef.current = null;
        setRestPose(null);
      }
      const target = targetRef.current;
      const dx = target.x - xRef.current;
      const dy = target.y - yRef.current;
      const dist = Math.hypot(dx, dy);
      if (dist < WANDER_ARRIVE_DIST) {
        targetRef.current = pickWanderTarget(centerRef.current, areasRef.current);
        // 산책은 정해진 시간만큼 계속 걷는다 — 도착했다고 쉬면 산책이 아니라 순간이동이 된다.
        if (strollingRef.current) return;
        restUntilRef.current = now + REST_MS_MIN + Math.random() * (REST_MS_MAX - REST_MS_MIN);
        const pose = spriteForRest ? pickRestPose(spriteForRest, level) : "idle";
        restPoseRef.current = pose;
        setRestPose(pose);
        return;
      }
      // 산책은 "일하다 잠깐 도는" 그림이라 급할 필요가 없다. 다만 잔여율 idle의
      // 기본 속도(0.4)면 5초 동안 20px밖에 못 가 걷는 티가 안 나서 최소치를 준다.
      const speed = strollingRef.current
        ? Math.max(PET_SPEED[level], PET_SPEED.good)
        : PET_SPEED[level];
      const nx = xRef.current + (dx / dist) * speed;
      const ny = yRef.current + (dy / dist) * speed;
      xRef.current = nx;
      yRef.current = ny;
      if (Math.abs(dx) > 2) {
        const newDir: 1 | -1 = dx > 0 ? 1 : -1;
        if (dirRef.current !== newDir) {
          dirRef.current = newDir;
          if (isStaticImageMode && wrapRef.current) {
            wrapRef.current.style.transform = `scaleX(${newDir})`;
          }
          setDir(newDir);
        }
      }
      win.setPosition(new LogicalPosition(nx, ny)).catch(() => {});
    }, PET_TICK_MS);
    return () => clearInterval(id);
  }, [level, bundle]);

  function onPointerDown(e: React.PointerEvent<HTMLDivElement>) {
    if (e.button !== 0 || activePointerRef.current !== null) return;
    activePointerRef.current = e.pointerId;
    draggingRef.current = true;
    setDragging(true);
    dragMovedRef.current = false;
    dragOffsetRef.current = { x: e.screenX - xRef.current, y: e.screenY - yRef.current };
    dragBaselineXRef.current = e.screenX;
    e.currentTarget.setPointerCapture(e.pointerId);
    e.preventDefault();
  }

  function onPointerMove(e: React.PointerEvent<HTMLDivElement>) {
    if (e.pointerId !== activePointerRef.current) return;
    const dx = e.screenX - dragBaselineXRef.current;
    if (!dragMovedRef.current && Math.abs(dx) >= DRAG_THRESHOLD) {
      dragMovedRef.current = true;
    }
    if (dx >= DRAG_THRESHOLD) {
      dragBaselineXRef.current = e.screenX;
      if (dirRef.current !== 1) {
        dirRef.current = 1;
        setDir(1);
      }
    } else if (dx <= -DRAG_THRESHOLD) {
      dragBaselineXRef.current = e.screenX;
      if (dirRef.current !== -1) {
        dirRef.current = -1;
        setDir(-1);
      }
    }
    // 드래그 중에는 클램프하지 않는다 — 모니터 사이를 지나갈 때 가로막히면
    // 다른 화면으로 옮길 수가 없다. 놓는 순간에만 화면 안으로 스냅한다(endDrag).
    const nx = e.screenX - dragOffsetRef.current.x;
    const ny = e.screenY - dragOffsetRef.current.y;
    xRef.current = nx;
    yRef.current = ny;
    if (!bundle?.sprite && wrapRef.current) {
      wrapRef.current.style.transform = `scaleX(${dirRef.current})`;
    }
    getCurrentWindow().setPosition(new LogicalPosition(nx, ny)).catch(() => {});
  }

  function endDrag(e: React.PointerEvent<HTMLDivElement>) {
    if (e.pointerId !== activePointerRef.current) return;
    activePointerRef.current = null;
    draggingRef.current = false;
    setDragging(false);
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    // 실제로 끌지 않고 그냥 눌렀다 뗀 거면 클릭으로 취급해 팝오버를 연다.
    if (!dragMovedRef.current) {
      invoke("open_main_window").catch(() => {});
    } else {
      // 놓은 자리를 화면 안으로 스냅한 뒤 그곳을 새 배회 중심으로 삼는다 —
      // 원래 반경으로 되돌아가지 않고, 옮겨준 모니터에서 계속 돌아다닌다.
      const snapped = clampToAreas(xRef.current, yRef.current, areasRef.current);
      xRef.current = snapped.x;
      yRef.current = snapped.y;
      getCurrentWindow().setPosition(new LogicalPosition(snapped.x, snapped.y)).catch(() => {});
      centerRef.current = snapped;
      targetRef.current = pickWanderTarget(centerRef.current, areasRef.current);
    }
  }

  const dragHandlers = {
    onPointerDown,
    onPointerMove,
    onPointerUp: endDrag,
    onPointerCancel: endDrag,
    onLostPointerCapture: endDrag,
    onContextMenu: (e: React.MouseEvent) => {
      e.preventDefault();
      invoke("show_pet_context_menu").catch(() => {});
    },
  };

  // 잔여율 경보가 활동 알림보다 급하니 먼저 자리를 차지한다.
  const speechBubble = alertFlash ? (
    <div className="pet-speech-bubble">{t(lang, `pet.alert.${alertFlash.kind}`, { tool: alertFlash.tool })}</div>
  ) : doneFlash ? (
    <div className="pet-speech-bubble">
      {t(lang, doneFlash.failed ? "pet.activity.failed" : "pet.activity.done", {
        tool: doneFlash.tool,
      })}
    </div>
  ) : null;

  // 커스텀 번들에 sprite가 있으면 그걸, 커스텀 번들 자체가 없으면(기본 펫) 내장
  // 스프라이트를 쓴다. 커스텀 sheet는 파일시스템 절대경로라 convertFileSrc가 필요하고,
  // 내장 sheet는 이미 번들된 정적 자산 URL이라 그대로 쓴다.
  const sprite = bundle ? bundle.sprite : BUILTIN_SPRITE;
  const spriteIsCustomFile = !!bundle?.sprite;

  if (sprite) {
    const spriteHeight = SPRITE_BASE_HEIGHT * scale;
    const spriteScale = spriteHeight / sprite.frameHeight;
    const dispW = sprite.frameWidth * spriteScale;
    const animName = pickSpriteAnimation({
      sprite,
      level,
      dir,
      dragging,
      restPose,
      alerting: !!alertFlash,
      activity,
      strolling,
    });
    const anim = sprite.animations[animName];
    const rowOffsetY = -(anim.row * sprite.frameHeight * spriteScale);
    const { keyframesCss, animationCss } = buildSpriteAnimationCss({
      keyframesId: animName.replace(/[^a-zA-Z0-9_-]/g, "_"),
      frames: anim.frames,
      fps: sprite.fps,
      frameWidth: sprite.frameWidth,
      scale: spriteScale,
      rowOffsetY,
      frameDurationsMs: anim.frameDurationsMs,
    });
    const sheetUrl = spriteIsCustomFile ? convertFileSrc(sprite.sheet) : sprite.sheet;
    return (
      <div
        ref={wrapRef}
        className="pet-overlay"
        style={{ width: petSize, height: petSize }}
        {...dragHandlers}
      >
        <style>{keyframesCss}</style>
        {speechBubble}
        <div
          className="pet-sprite-frame"
          style={{
            width: dispW,
            height: spriteHeight,
            backgroundImage: `url(${sheetUrl})`,
            backgroundSize: `${sprite.columns * sprite.frameWidth * spriteScale}px ${
              sprite.rows * sprite.frameHeight * spriteScale
            }px`,
            animation: animationCss,
          }}
        />
      </div>
    );
  }

  return (
    <div
      ref={wrapRef}
      className="pet-overlay"
      style={{ width: petSize, height: petSize }}
      {...dragHandlers}
    >
      {speechBubble}
      <img
        src={src}
        alt=""
        className="pet-sprite pet-bob"
        style={{ width: SPRITE_BASE_HEIGHT * scale, height: SPRITE_BASE_HEIGHT * scale }}
        onError={() => setSrc(BUILTIN_PET[level])}
      />
    </div>
  );
}

interface Insight {
  key: string;
  level: "good" | "warn" | "tip";
}

interface ModelBreakdown {
  model: string;
  usage: number;
  cost: number;
}

/** 주간 윈도우 안의 하루 — 그날이 주간 한도의 몇 %를 썼는지 */
interface WeeklyDay {
  date: string; // "MM/DD"
  weekday: number; // 0=월 ~ 6=일
  usage: number;
  dailyPercent: number;
  cumulativePercent: number;
  isToday: boolean;
  isFuture: boolean;
}

/** 카드 맨 위 한 줄 결론 — 소진이 먼저인지 리셋이 먼저인지 */
interface Verdict {
  key: string;
  level: "danger" | "warn" | "good";
  etaMinutes: number | null;
  resetMinutes: number | null;
}

interface RunwayStatus {
  tool: string;
  available: boolean;
  unit: string; // "tokens" | "requests"
  windowHours: number;
  windowUsage: number;
  dailyUsage: number;
  dailyCount: number;
  dailyCost: number;
  cacheHitRate: number | null;
  insights: Insight[];
  planHint: Insight | null;
  models: ModelBreakdown[];
  sparkline: number[];
  limit: number | null;
  percentRemaining: number | null;
  burnRatePerMin: number;
  burnTrend: string; // "up" | "down" | "flat"
  etaMinutes: number | null;
  resetsAt: string | null;
  sevenDayRemaining: number | null;
  sevenDayEtaMinutes: number | null;
  weeklyDays: WeeklyDay[];
  verdict: Verdict | null;
  isEstimate: boolean;
  plan: string | null;
  note: string | null;
}

interface Settings {
  alertThreshold: number;
  etaAlertMinutes: number;
  resetAlertMinutes: number;
  pollSeconds: number;
  disabledTools: string[];
  trayTool: string | null;
  notificationsEnabled: boolean;
  language: string | null;
  quietEnabled: boolean;
  quietStartHour: number;
  quietEndHour: number;
  analyticsEnabled: boolean;
  hideInactive: boolean;
  trayShowPercent: boolean;
  trayShowReset: boolean;
  petEnabled: boolean;
  petBundles: PetBundle[];
  activePetBundleId: string | null;
  petLastX: number | null;
  petLastY: number | null;
  petScale: number;
}

interface ToolInfo {
  tool: string;
  available: boolean;
}

function formatAmount(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
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

/**
 * 누적 윈도우 제목. 도구마다 주기가 달라(Claude·Codex 5h, Gemini 24h, Grok 주간)
 * "세션 (168h)" 같은 표기가 나오지 않게 길이에 맞는 말을 쓴다.
 */
function windowTitle(hours: number, lang: Lang): string {
  if (hours >= 24 * 28) return t(lang, "windowMonth");
  if (hours >= 24 * 7) return t(lang, "windowWeek");
  if (hours >= 24) return t(lang, "windowDay");
  return `${t(lang, "session")} (${windowLabel(hours)})`;
}

function formatEta(min: number | null, lang: Lang): string {
  if (min == null) return "—";
  if (min < 1) return t(lang, "lessThanMin");
  const h = Math.floor(min / 60);
  const m = Math.round(min % 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** 하루가 넘는 기간용 — "3d 4h". 주간 한도·리셋처럼 긴 시간에 쓴다. */
function formatDuration(min: number | null, lang: Lang): string {
  if (min == null) return "—";
  if (min < 1) return t(lang, "lessThanMin");
  const d = Math.floor(min / 1440);
  if (d >= 1) {
    const h = Math.round((min % 1440) / 60);
    return h > 0 ? `${d}d ${h}h` : `${d}d`;
  }
  return formatEta(min, lang);
}

/** 요일 라벨 — 0=월 ~ 6=일 */
function weekdayLabel(i: number, lang: Lang): string {
  return t(lang, `wd.${i}`);
}

/**
 * 주간 한도 일별 막대 — 팝오버(압축)와 히스토리 창(상세)이 공유한다.
 *
 * 막대 영역과 요일 축을 따로 두는 이유: 누적 선 SVG를 막대 영역에만 정확히 겹치기
 * 위해서다. 라벨까지 한 상자에 넣으면 선이 라벨 높이만큼 밀린다.
 * 칸 간격도 gap이 아니라 안쪽 padding으로 준다 — gap을 쓰면 flex 칸이 균등 분할되지
 * 않아 선의 x좌표와 막대 중심이 어긋난다.
 */
function WeeklyBars({
  days,
  lang,
  showLine,
  onHover,
}: {
  days: WeeklyDay[];
  lang: Lang;
  showLine?: boolean;
  onHover?: (i: number | null) => void;
}) {
  // 가장 많이 쓴 날을 꽉 차게 그려야 하루치 차이가 눈에 들어온다.
  const max = Math.max(...days.map((d) => d.dailyPercent), 0.01);

  // 누적 선은 첫 사용일부터 오늘까지만 긋는다. 미래로 이으면 데이터가 있는 것처럼
  // 보이고, 아직 안 쓴 앞 구간은 점이 바닥에 붙어 요일 라벨과 겹친다.
  const firstUsed = days.findIndex((d) => d.cumulativePercent > 0);
  const past = days
    .map((d, i) => ({ d, i }))
    .filter((x) => !x.d.isFuture && firstUsed >= 0 && x.i >= firstUsed);
  const line = past
    .map(
      ({ d, i }, k) =>
        `${k === 0 ? "M" : "L"}${i + 0.5},${(
          100 - Math.min(100, d.cumulativePercent)
        ).toFixed(2)}`
    )
    .join(" ");

  return (
    <>
      <div className={`wk-plot${showLine ? " wk-plot-tall" : ""}`}>
        {days.map((d, i) => (
          <div
            className={`wk-col${d.isToday ? " wk-today" : ""}${
              d.isFuture ? " wk-future" : ""
            }`}
            key={d.date}
            title={
              onHover
                ? undefined
                : `${d.date} · ${d.dailyPercent.toFixed(1)}%`
            }
            onMouseEnter={() => onHover?.(i)}
            onMouseLeave={() => onHover?.(null)}
          >
            <div className="wk-track">
              <div
                className="wk-fill"
                style={{ height: `${(d.dailyPercent / max) * 100}%` }}
              />
            </div>
          </div>
        ))}
        {/* 누적 선은 막대(주황)와 다른 색이라야 겹쳐도 읽힌다. 기존 추세 차트와 같은 파랑. */}
        {showLine && past.length > 1 && (
          <svg
            className="wk-line"
            viewBox={`0 0 ${days.length} 100`}
            preserveAspectRatio="none"
            aria-hidden
          >
            <path
              d={line}
              fill="none"
              stroke="#0a84ff"
              strokeWidth="1.5"
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          </svg>
        )}
        {/* 점은 SVG 대신 HTML로 — preserveAspectRatio="none"이라 SVG 원은 타원으로 찌그러진다 */}
        {showLine &&
          past.map(({ d, i }) => (
            <span
              className="wk-dot"
              key={d.date}
              style={{
                left: `${((i + 0.5) / days.length) * 100}%`,
                top: `${100 - Math.min(100, d.cumulativePercent)}%`,
              }}
            />
          ))}
      </div>
      <div className="wk-axis">
        {days.map((d) => (
          <span
            className={d.isToday ? "wk-today-label" : d.isFuture ? "wk-dim" : ""}
            key={d.date}
          >
            {weekdayLabel(d.weekday, lang)}
          </span>
        ))}
      </div>
    </>
  );
}

/** 팝오버용 — 폭이 좁아 축·툴팁 없이 막대와 요일만 */
function WeeklyMiniBars({ days, lang }: { days: WeeklyDay[]; lang: Lang }) {
  if (!days.length) return null;
  return (
    <div className="wk-mini">
      <WeeklyBars days={days} lang={lang} />
    </div>
  );
}

/** "얼마나 남았어?"에 대한 직접적인 한 줄 답 */
function VerdictLine({ v, lang }: { v: Verdict; lang: Lang }) {
  return (
    <p className={`verdict verdict-${v.level}`}>
      {t(lang, v.key, {
        eta: formatDuration(v.etaMinutes, lang),
        reset: formatDuration(v.resetMinutes, lang),
      })}
    </p>
  );
}

/** 구간별 사용량 미니 막대 스파크라인 */
function Sparkline({ data }: { data: number[] }) {
  if (!data.length || data.every((v) => v === 0)) return null;
  const max = Math.max(...data, 1);
  return (
    <div className="spark" aria-hidden>
      {data.map((v, i) => (
        <span key={i} style={{ height: `${Math.max(6, (v / max) * 100)}%` }} />
      ))}
    </div>
  );
}

function formatUpdated(ts: number, lang: Lang): string {
  const diffSec = Math.round((Date.now() - ts) / 1000);
  if (diffSec < 60) return t(lang, "updatedJustNow");
  const m = Math.floor(diffSec / 60);
  return t(lang, "updatedAgo", { t: `${m}m` });
}

function formatResetsAt(iso: string | null, lang: Lang): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (isNaN(d.getTime())) return null;
  const diffMin = Math.round((d.getTime() - Date.now()) / 60000);
  if (diffMin <= 0) return t(lang, "resetSoon");
  const h = Math.floor(diffMin / 60);
  const m = diffMin % 60;
  const time = h > 0 ? `${h}h ${m}m` : `${m}m`;
  return t(lang, "afterReset", { t: time });
}

/** 트레이 팝오버 — 런웨이 대시보드 */
function Dashboard() {
  const [statuses, setStatuses] = useState<RunwayStatus[]>([]);
  const [loading, setLoading] = useState(true);
  const [pollSeconds, setPollSeconds] = useState(30);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);

  async function refresh() {
    try {
      const data = await invoke<RunwayStatus[]>("get_runway");
      setStatuses(data);
      setLastUpdated(Date.now());
    } finally {
      setLoading(false);
    }
  }

  async function reloadSettings() {
    try {
      const s = await invoke<Settings>("get_settings");
      setPollSeconds(s.pollSeconds);
      setLang(resolveLang(s.language));
    } catch {
      /* noop */
    }
  }

  useEffect(() => {
    refresh();
    reloadSettings();
    // 설정 창에서 변경하면 즉시 반영 (폴링 기다리지 않음)
    const unlisten = listen("settings-changed", () => {
      reloadSettings();
      refresh();
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 콘텐츠 너비에 맞춰 팝오버 창 너비 자동 조정 (텍스트가 줄바꿈되지 않게)
  useEffect(() => {
    const ro = new ResizeObserver(() => {
      // 줄바꿈 없이 필요한 너비(scrollWidth) + 높이
      const w = Math.ceil(document.documentElement.scrollWidth);
      const h = Math.ceil(document.documentElement.scrollHeight);
      const win = getCurrentWindow();
      win
        .setSize(
          new LogicalSize(
            Math.min(Math.max(w, 360), 560), // 360~560 범위
            Math.min(Math.max(h, 160), 760)
          )
        )
        .catch(() => {});
    });
    ro.observe(document.body);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const id = setInterval(async () => {
      refresh();
      try {
        const s = await invoke<Settings>("get_settings");
        if (s.pollSeconds !== pollSeconds) setPollSeconds(s.pollSeconds);
        setLang(resolveLang(s.language));
      } catch {
        /* noop */
      }
    }, Math.max(5, pollSeconds) * 1000);
    return () => clearInterval(id);
  }, [pollSeconds]);

  return (
    <main className="container">
      <header>
        <h1>🛬 Token Runway</h1>
        <div className="header-actions">
          <button onClick={refresh} className="refresh">
            {t(lang, "refresh")}
          </button>
          <button
            onClick={() => invoke("open_history_window")}
            className="icon-btn"
            title={t(lang, "history")}
          >
            📅
          </button>
          <button
            onClick={() => invoke("open_settings_window")}
            className="icon-btn"
            title={t(lang, "settings")}
          >
            ⚙️
          </button>
        </div>
      </header>

      {loading && <p className="muted">{t(lang, "loading")}</p>}
      {!loading && statuses.length === 0 && (
        <p className="muted">{t(lang, "noTools")}</p>
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
                  {/* 가정한 한도로 낸 추정치는 근사 기호로 구분한다 */}
                  {s.isEstimate && "≈"}
                  {s.percentRemaining.toFixed(0)}%
                  <span className="pct-label"> {t(lang, "remaining")}</span>
                </>
              ) : (
                t(lang, "noLimit")
              )}
            </span>
          </div>

          {s.percentRemaining != null && (
            <div className="bar">
              <div
                className="bar-fill"
                style={{
                  width: `${Math.max(0, Math.min(100, s.percentRemaining))}%`,
                }}
              />
            </div>
          )}

          {s.verdict && <VerdictLine v={s.verdict} lang={lang} />}

          {formatResetsAt(s.resetsAt, lang) && (
            <p className="resets">⏱ {formatResetsAt(s.resetsAt, lang)}</p>
          )}

          <div className="today-section">
            <span className="today-title">{windowTitle(s.windowHours, lang)}</span>
            <div className="today">
              <div className="today-item">
                <span className="today-val">{formatAmount(s.windowUsage)}</span>
                <span className="today-label">{t(lang, "used")}</span>
              </div>
              <div className="today-item">
                <span className="today-val">
                  {formatAmount(Math.round(s.burnRatePerMin))}/min
                </span>
                <span className="today-label">
                  {t(lang, "burnRate")}
                  {s.burnTrend === "up" && <span className="trend-up"> ↑</span>}
                  {s.burnTrend === "down" && (
                    <span className="trend-down"> ↓</span>
                  )}
                  {s.burnTrend === "flat" && (
                    <span className="trend-flat"> →</span>
                  )}
                </span>
              </div>
              <div className="today-item">
                <span className="today-val">{formatEta(s.etaMinutes, lang)}</span>
                <span className="today-label">{t(lang, "eta")}</span>
              </div>
            </div>
          </div>

          <div className="today-section">
            <span className="today-title">{t(lang, "today")}</span>
            <div className="today">
              {s.unit === "requests" ? (
                <div className="today-item">
                  <span className="today-val">{s.dailyCount}</span>
                  <span className="today-label">{t(lang, "todayRequests")}</span>
                </div>
              ) : (
                <>
                  <div className="today-item">
                    <span className="today-val">
                      {formatAmount(s.dailyUsage)}
                    </span>
                    <span className="today-label">{t(lang, "todayTokens")}</span>
                  </div>
                  <div className="today-item">
                    <span className="today-val">{s.dailyCount}</span>
                    <span className="today-label">{t(lang, "todayMessages")}</span>
                  </div>
                  {s.dailyCost > 0 && (
                    <div className="today-item">
                      <span className="today-val">${s.dailyCost.toFixed(2)}</span>
                      <span className="today-label">{t(lang, "apiValue")}</span>
                    </div>
                  )}
                </>
              )}
            </div>
          </div>

          <Sparkline data={s.sparkline} />

          {s.cacheHitRate != null && (
            <p className="cache-hit">
              ⚡ {t(lang, "cacheHit", { n: s.cacheHitRate.toFixed(0) })}
            </p>
          )}

          {s.insights.map((ins) => (
            <p key={ins.key} className={`insight insight-${ins.level}`}>
              {t(lang, ins.key)}
            </p>
          ))}

          {s.planHint && (
            <p className={`insight plan-hint insight-${s.planHint.level}`}>
              {t(lang, s.planHint.key)}
            </p>
          )}

          {s.models.length > 0 && (
            <div className="model-mini">
              <span className="model-mini-title">
                {t(lang, "modelBreakdown")}
              </span>
              {s.models.map((m) => (
                <div className="model-row" key={m.model}>
                  <span className="model-name">{m.model}</span>
                  <div className="model-track">
                    <div
                      className="model-fill"
                      style={{
                        width: `${(m.usage / (s.models[0].usage || 1)) * 100}%`,
                      }}
                    />
                  </div>
                  <span className="model-val">
                    {formatAmount(m.usage)}
                    {m.cost > 0 ? ` · $${m.cost.toFixed(2)}` : ""}
                  </span>
                </div>
              ))}
            </div>
          )}

          {/* 주 윈도우가 이미 주간인 도구(Grok)는 위 잔여율이 곧 주간이라 중복이다 */}
          {s.sevenDayRemaining != null && s.windowHours < 24 * 7 && (
            <p className="weekly">
              {t(lang, "weekly", { n: s.sevenDayRemaining.toFixed(0) })}
              {/* Max 사용자의 실제 병목은 5시간이 아니라 주간인 경우가 많다 */}
              {s.sevenDayEtaMinutes != null && (
                <span className="weekly-eta">
                  {" · "}
                  {t(lang, "weeklyEta", {
                    t: formatDuration(s.sevenDayEtaMinutes, lang),
                  })}
                </span>
              )}
            </p>
          )}
          <WeeklyMiniBars days={s.weeklyDays} lang={lang} />

          {s.note && (
            <p className={s.note.startsWith("error.") ? "note-error" : "note"}>
              {t(lang, s.note, { tool: s.tool })}
            </p>
          )}
        </section>
      ))}

      {lastUpdated && (
        <p className="updated">{formatUpdated(lastUpdated, lang)}</p>
      )}
    </main>
  );
}

/** 설정 전용 창 */
function SettingsView() {
  const [settings, setSettings] = useState<Settings>({
    alertThreshold: 20,
    etaAlertMinutes: 30,
    resetAlertMinutes: 0,
    pollSeconds: 30,
    disabledTools: [],
    trayTool: null,
    notificationsEnabled: true,
    language: null,
    quietEnabled: false,
    quietStartHour: 22,
    quietEndHour: 8,
    analyticsEnabled: false,
    hideInactive: false,
    trayShowPercent: true,
    trayShowReset: true,
    petEnabled: true,
    petBundles: [],
    activePetBundleId: null,
    petLastX: null,
    petLastY: null,
    petScale: 1,
  });
  const [petError, setPetError] = useState<string | null>(null);
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [permGranted, setPermGranted] = useState<boolean | null>(null);
  const [autostart, setAutostart] = useState(false);

  const lang = resolveLang(settings.language);
  const tr = (k: string, p?: Record<string, string | number>) => t(lang, k, p);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings).catch(() => {});
    invoke<ToolInfo[]>("get_available_tools").then(setTools).catch(() => {});
    isPermissionGranted().then(setPermGranted).catch(() => {});
    isEnabled().then(setAutostart).catch(() => {});
    invoke("track_event", { event: "settings_opened" }).catch(() => {});
    // pet 우클릭 메뉴·트레이에서 바꾼 값(활성 스킨, 표시 여부)을 화면에 반영한다 —
    // 안 들으면 여기 state가 낡은 채로 남아 다음 저장에서 그 변경을 덮어쓴다.
    const reload = () => {
      invoke<Settings>("get_settings").then(setSettings).catch(() => {});
    };
    const unlisten = listen("settings-changed", reload);
    // 창이 다시 포커스를 받을 때도 읽는다 — 이벤트를 놓쳤더라도 화면을 보는 순간엔 맞다.
    const unfocus = getCurrentWindow().onFocusChanged(({ payload }) => {
      if (payload) reload();
    });
    return () => {
      unlisten.then((f) => f());
      unfocus.then((f) => f());
    };
  }, []);

  async function toggleAutostart(value: boolean) {
    try {
      if (value) await enable();
      else await disable();
      setAutostart(value);
    } catch {
      /* noop */
    }
  }

  /// set_settings는 전체 구조체를 받으므로, 화면 state가 낡았으면 다른 경로(우클릭 메뉴·
  /// 트레이)에서 바뀐 값까지 같이 되돌려버린다. 저장 직전에 서버 값을 다시 읽어 그 위에
  /// 이번 변경만 덮는다.
  async function update(patch: Partial<Settings>) {
    setSettings((prev) => ({ ...prev, ...patch }));
    let base = settings;
    try {
      base = await invoke<Settings>("get_settings");
    } catch {
      /* 조회 실패 시엔 화면 state를 기준으로 진행 */
    }
    const next = { ...base, ...patch };
    setSettings(next);
    await invoke("set_settings", { settings: next });
  }

  function toggleTool(tool: string, enabled: boolean) {
    const disabled = enabled
      ? settings.disabledTools.filter((x) => x !== tool)
      : [...settings.disabledTools, tool];
    update({ disabledTools: disabled });
  }

  async function pickPetBundle() {
    const dir = await openDialog({ directory: true, multiple: false });
    if (!dir || Array.isArray(dir)) return;
    setPetError(null);
    try {
      const bundle = await invoke<PetBundle>("import_pet_bundle", {
        sourceDir: dir,
      });
      await update({
        petBundles: [...settings.petBundles, bundle],
        activePetBundleId: bundle.id,
      });
    } catch (e) {
      setPetError(String(e));
    }
  }

  function selectPetBundle(id: string | null) {
    update({ activePetBundleId: id });
  }

  async function deletePetBundle(id: string) {
    try {
      await invoke("delete_pet_bundle", { id });
      await update({
        petBundles: settings.petBundles.filter((b) => b.id !== id),
        activePetBundleId:
          settings.activePetBundleId === id ? null : settings.activePetBundleId,
      });
      setPetError(null);
    } catch (e) {
      setPetError(String(e));
    }
  }

  async function askPermission() {
    const result = await requestPermission();
    setPermGranted(result === "granted");
  }

  async function setNotifications(enabled: boolean) {
    update({ notificationsEnabled: enabled });
    if (enabled) {
      let granted = await isPermissionGranted();
      if (!granted) {
        const res = await requestPermission();
        granted = res === "granted";
      }
      setPermGranted(granted);
    }
  }

  async function openNotifSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.notifications"
    ).catch(() => {});
  }

  return (
    <main className="container settings-window win-framed">
      <div className="win-titlebar-bg" />
      <section className="settings">
        <label className="setting-row toggle-row">
          <span>{tr("osNotif")}</span>
          <input
            type="checkbox"
            checked={settings.notificationsEnabled}
            onChange={(e) => setNotifications(e.target.checked)}
          />
        </label>
        {settings.notificationsEnabled && permGranted === false && (
          <p className="perm-warn">
            ⚠️ {tr("permWarn")}{" "}
            <button className="link-btn" onClick={askPermission}>
              {tr("reqPerm")}
            </button>
            {" · "}
            <button className="link-btn" onClick={openNotifSettings}>
              {tr("openSysSettings")}
            </button>
          </p>
        )}
        {settings.notificationsEnabled && permGranted === true && (
          <p className="setting-hint">{tr("permOk")}</p>
        )}

        <label className="setting-row toggle-row">
          <span>{tr("quietHours")}</span>
          <input
            type="checkbox"
            checked={settings.quietEnabled}
            onChange={(e) => update({ quietEnabled: e.target.checked })}
          />
        </label>
        {settings.quietEnabled && (
          <div className="quiet-range">
            <label>
              {tr("quietFrom")}
              <select
                value={settings.quietStartHour}
                onChange={(e) =>
                  update({ quietStartHour: Number(e.target.value) })
                }
              >
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>
                    {String(h).padStart(2, "0")}:00
                  </option>
                ))}
              </select>
            </label>
            <label>
              {tr("quietTo")}
              <select
                value={settings.quietEndHour}
                onChange={(e) =>
                  update({ quietEndHour: Number(e.target.value) })
                }
              >
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>
                    {String(h).padStart(2, "0")}:00
                  </option>
                ))}
              </select>
            </label>
          </div>
        )}

        <hr className="setting-divider" />

        <label className="setting-row">
          <span>{tr("alertThreshold")}</span>
          <span className="setting-value">
            {tr("pctRemaining", { n: settings.alertThreshold })}
          </span>
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
          <span>{tr("etaAlert")}</span>
          <span className="setting-value">
            {settings.etaAlertMinutes === 0
              ? tr("off")
              : tr("minBefore", { n: settings.etaAlertMinutes })}
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
          <span>{tr("resetAlert")}</span>
          <span className="setting-value">
            {settings.resetAlertMinutes === 0
              ? tr("off")
              : tr("minBefore", { n: settings.resetAlertMinutes })}
          </span>
        </label>
        <input
          type="range"
          min={0}
          max={60}
          step={5}
          value={settings.resetAlertMinutes}
          onChange={(e) => update({ resetAlertMinutes: Number(e.target.value) })}
        />

        <label className="setting-row">
          <span>{tr("pollInterval")}</span>
          <span className="setting-value">
            {tr("sec", { n: settings.pollSeconds })}
          </span>
        </label>
        <input
          type="range"
          min={10}
          max={120}
          step={10}
          value={settings.pollSeconds}
          onChange={(e) => update({ pollSeconds: Number(e.target.value) })}
        />

        <label className="setting-row">
          <span>{tr("trayDisplay")}</span>
        </label>
        <select
          className="setting-select"
          value={settings.trayTool ?? ""}
          onChange={(e) => update({ trayTool: e.target.value || null })}
        >
          <option value="">{tr("trayAuto")}</option>
          {tools
            .filter((x) => x.available)
            .map((x) => (
              <option key={x.tool} value={x.tool}>
                {x.tool}
              </option>
            ))}
        </select>

        <label className="setting-row">
          <span>{tr("trayInfo")}</span>
        </label>
        <label className="tool-toggle">
          <input
            type="checkbox"
            checked={settings.trayShowPercent}
            onChange={(e) => update({ trayShowPercent: e.target.checked })}
          />
          <span>{tr("trayShowPercent")}</span>
        </label>
        <label className="tool-toggle">
          <input
            type="checkbox"
            checked={settings.trayShowReset}
            onChange={(e) => update({ trayShowReset: e.target.checked })}
          />
          <span>{tr("trayShowReset")}</span>
        </label>

        <label className="setting-row">
          <span>{tr("language")}</span>
        </label>
        <select
          className="setting-select"
          value={settings.language ?? ""}
          onChange={(e) => update({ language: e.target.value || null })}
        >
          <option value="">{tr("langAuto")}</option>
          <option value="ko">한국어</option>
          <option value="en">English</option>
        </select>

        <label className="setting-row toggle-row">
          <span>{tr("autostart")}</span>
          <input
            type="checkbox"
            checked={autostart}
            onChange={(e) => toggleAutostart(e.target.checked)}
          />
        </label>

        <label className="setting-row toggle-row">
          <span>{tr("analytics")}</span>
          <input
            type="checkbox"
            checked={settings.analyticsEnabled}
            onChange={(e) => update({ analyticsEnabled: e.target.checked })}
          />
        </label>
        <p className="setting-hint">{tr("analyticsHint")}</p>

        <label className="setting-row toggle-row">
          <span>{tr("hideInactive")}</span>
          <input
            type="checkbox"
            checked={settings.hideInactive}
            onChange={(e) => update({ hideInactive: e.target.checked })}
          />
        </label>

        <hr className="setting-divider" />

        <label className="setting-row toggle-row">
          <span>{tr("pet.enable")}</span>
          <input
            type="checkbox"
            checked={settings.petEnabled}
            onChange={(e) => update({ petEnabled: e.target.checked })}
          />
        </label>
        {settings.petEnabled && (
          <>
            <label className="setting-row">
              <span>{tr("pet.size")}</span>
              <span className="setting-value">
                {Math.round((settings.petScale ?? 1) * 100)}%
              </span>
            </label>
            <input
              type="range"
              min={50}
              max={250}
              step={10}
              value={Math.round((settings.petScale ?? 1) * 100)}
              onChange={(e) => update({ petScale: Number(e.target.value) / 100 })}
            />
          </>
        )}
        {settings.petEnabled && (
          <div className="pet-settings">
            <ul className="pet-list">
              <li
                className={`pet-list-item${
                  settings.activePetBundleId === null ? " active" : ""
                }`}
              >
                <button className="link-btn" onClick={() => selectPetBundle(null)}>
                  {settings.activePetBundleId === null ? "●" : "○"} {tr("pet.currentBuiltin")}
                </button>
              </li>
              {settings.petBundles.map((b) => (
                <li
                  key={b.id}
                  className={`pet-list-item${
                    settings.activePetBundleId === b.id ? " active" : ""
                  }`}
                >
                  <button className="link-btn" onClick={() => selectPetBundle(b.id)}>
                    {settings.activePetBundleId === b.id ? "●" : "○"} {b.name}
                  </button>
                  <button
                    className="link-btn pet-delete"
                    onClick={() => deletePetBundle(b.id)}
                    title={tr("pet.delete")}
                  >
                    ✕
                  </button>
                </li>
              ))}
            </ul>
            <div className="pet-actions">
              <button className="link-btn" onClick={pickPetBundle}>
                {tr("pet.chooseFolder")}
              </button>
            </div>
            {petError && (
              <p className="perm-warn">⚠️ {tr(petErrorKey(petError))}</p>
            )}
          </div>
        )}

        {tools.length > 0 && (
          <div className="setting-tools">
            <span className="setting-row">{tr("monitorTools")}</span>
            {tools.map((x) => (
              <label key={x.tool} className="tool-toggle">
                <input
                  type="checkbox"
                  checked={!settings.disabledTools.includes(x.tool)}
                  disabled={!x.available}
                  onChange={(e) => toggleTool(x.tool, e.target.checked)}
                />
                <span>
                  {x.tool}
                  {!x.available && <span className="muted">{tr("notDetected")}</span>}
                </span>
              </label>
            ))}
          </div>
        )}

        <p className="setting-hint">{tr("settingsHint")}</p>
      </section>
    </main>
  );
}

interface DayStat {
  date: string;
  usage: number;
  count: number;
  cost: number;
}
interface ModelUsage {
  model: string;
  usage: number;
  cost: number;
  count: number;
}
interface PlanTier {
  plan: string;
  projectedUtil: number;
  current: boolean;
  recommended: boolean;
}
interface LimitEstimate {
  limitTokens: number;
  usedTokens: number;
  messages: number;
}
interface PlanAdvice {
  currentPlan: string;
  recommendedPlan: string;
  direction: "upgrade" | "downgrade" | "keep" | "managed" | "na";
  managed: boolean;
  tiers: PlanTier[];
  estimate: LimitEstimate | null;
}
interface ToolStats {
  tool: string;
  unit: string;
  weeklyDays: WeeklyDay[];
  days: DayStat[];
  totalUsage: number;
  totalCost: number;
  avgUsage: number;
  peakDate: string;
  peakUsage: number;
  models: ModelUsage[];
  hourly: number[];
  thisWeekUsage: number;
  lastWeekUsage: number;
  thisWeekCost: number;
  lastWeekCost: number;
  planAdvice: PlanAdvice | null;
}

const PERIODS = [7, 14, 30, 60, 90, 180, 365];

/** 통계 로딩 중 표시할 스켈레톤 카드 (shimmer) */
function StatsCardSkeleton() {
  return (
    <section className="card" aria-hidden="true">
      <div className="sk sk-title" />
      <div className="sk-summary">
        {[0, 1, 2, 3].map((i) => (
          <div className="sk sk-stat" key={i} />
        ))}
      </div>
      <div className="sk sk-bars" />
      <div className="sk sk-block" />
      <div className="sk sk-block" />
    </section>
  );
}

/** 일수가 많을 때의 일별 추세 — 영역+꺾은선 + 호버/클릭 툴팁 */
function TrendChart({
  days,
  fmt,
  unit,
  lang,
}: {
  days: DayStat[];
  fmt: (n: number) => string;
  unit: string;
  lang: Lang;
}) {
  const [active, setActive] = useState<number | null>(null);
  const W = 100;
  const H = 40;
  const max = Math.max(...days.map((d) => d.usage), 1);
  const pts = days.map((d, i): [number, number] => [
    days.length > 1 ? (i / (days.length - 1)) * W : W / 2,
    H - (d.usage / max) * H,
  ]);
  const line = pts
    .map(([x, y], i) => `${i === 0 ? "M" : "L"}${x.toFixed(2)},${y.toFixed(2)}`)
    .join(" ");
  const area = `${line} L${W},${H} L0,${H} Z`;
  const mid = Math.floor((days.length - 1) / 2);

  // 포인터 x → 가장 가까운 데이터 인덱스
  const pick = (e: React.MouseEvent<HTMLDivElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width === 0) return;
    const ratio = (e.clientX - rect.left) / rect.width;
    const idx = Math.round(ratio * (days.length - 1));
    setActive(Math.max(0, Math.min(days.length - 1, idx)));
  };

  const a = active != null ? days[active] : null;
  const ax = active != null ? pts[active][0] : 0;
  const ay = active != null ? (pts[active][1] / H) * 100 : 0;
  const tipLeft = Math.min(82, Math.max(18, ax)); // 좌우 끝 잘림 방지

  return (
    <div className="trend">
      <div
        className="trend-plot"
        onMouseMove={pick}
        onMouseLeave={() => setActive(null)}
        onClick={pick}
      >
        <svg
          className="trend-svg"
          viewBox={`0 0 ${W} ${H}`}
          preserveAspectRatio="none"
        >
          <defs>
            <linearGradient id="trendFill" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="#0a84ff" stopOpacity="0.35" />
              <stop offset="100%" stopColor="#0a84ff" stopOpacity="0" />
            </linearGradient>
          </defs>
          <path d={area} fill="url(#trendFill)" />
          <path
            d={line}
            fill="none"
            stroke="#0a84ff"
            strokeWidth="1.5"
            strokeLinejoin="round"
            vectorEffect="non-scaling-stroke"
          />
        </svg>
        {a && (
          <>
            <div className="trend-guide" style={{ left: `${ax}%` }} />
            <div
              className="trend-active-dot"
              style={{ left: `${ax}%`, top: `${ay}%` }}
            />
            <div className="trend-tip" style={{ left: `${tipLeft}%` }}>
              <span className="trend-tip-date">{a.date}</span>
              <span>
                {fmt(a.usage)} {unitLabel(unit)}
              </span>
              <span>
                {a.count}{" "}
                {t(lang, unit === "requests" ? "requests" : "messages")}
              </span>
              {a.cost > 0 && <span>${a.cost.toFixed(2)}</span>}
            </div>
          </>
        )}
      </div>
      <div className="trend-axis">
        <span>{days[0]?.date}</span>
        <span>{days[mid]?.date}</span>
        <span>{days[days.length - 1]?.date}</span>
      </div>
    </div>
  );
}

/**
 * 주간 한도 소진 상세 — 일별 막대 + 누적 선 (히스토리 창용).
 *
 * 막대는 그날 하루가 쓴 몫, 선은 그날까지 누적. 100% 선이 주간 한도다.
 * 막대 합계는 공식 주간 사용률과 항상 일치한다(백엔드에서 안분).
 */
function WeeklyLimitBlock({ days, lang }: { days: WeeklyDay[]; lang: Lang }) {
  const [active, setActive] = useState<number | null>(null);
  if (!days.length) return null;

  const total = days[days.length - 1].cumulativePercent;
  const a = active != null ? days[active] : null;

  return (
    <div className="stat-block">
      <span className="stat-title">{t(lang, "wkTitle")}</span>
      <p className="wk-summary">
        {t(lang, "wkSummary", {
          used: total.toFixed(0),
          left: Math.max(0, 100 - total).toFixed(0),
        })}
      </p>
      <WeeklyBars days={days} lang={lang} showLine onHover={setActive} />
      {a ? (
        <p className="wk-tip">
          <b>{a.date}</b> · {t(lang, "wkDaily", { n: a.dailyPercent.toFixed(1) })}{" "}
          · {t(lang, "wkCum", { n: a.cumulativePercent.toFixed(1) })} ·{" "}
          {formatAmount(a.usage)} tok
        </p>
      ) : (
        <p className="wk-tip wk-tip-hint">{t(lang, "wkHint")}</p>
      )}
    </div>
  );
}

/** 요금제 티어 한 줄 — 예상 소진율 막대 */
function TierBar({
  tier,
  lang,
  managed,
}: {
  tier: PlanTier;
  lang: Lang;
  managed?: boolean;
}) {
  const lvl =
    tier.projectedUtil > 90 ? "over" : tier.projectedUtil > 75 ? "tight" : "ok";
  return (
    <div className="plan-tier">
      <span className="plan-tier-name">
        {tier.plan}
        {tier.current && (
          <span className="plan-tag">{t(lang, "planCurrent")}</span>
        )}
        {tier.recommended && (
          <span className="plan-tag plan-tag-rec">
            ★ {t(lang, managed ? "planEquivTag" : "planRecommended")}
          </span>
        )}
      </span>
      <div className="model-track">
        <div
          className={`plan-fill plan-fill-${lvl}`}
          style={{ width: `${Math.min(100, tier.projectedUtil)}%` }}
        />
      </div>
      <span className="model-val">{tier.projectedUtil.toFixed(0)}%</span>
    </div>
  );
}

/** 사용량 기반 요금제 추천 — 티어별 예상 소진율 막대 */
function PlanAdviceBlock({ a, lang }: { a: PlanAdvice; lang: Lang }) {
  // 관리형(Enterprise/Team) — 추정 5시간 한도 + 개인 플랜 환산(참고용, 전환 추천 아님).
  if (a.managed) {
    const e = a.estimate;
    const hasTiers = a.tiers.length > 0;
    return (
      <div className="stat-block">
        <span className="stat-title">{t(lang, "planManagedTitle")}</span>
        {e ? (
          <>
            <p className="plan-est">
              {t(lang, "est5hLimit", { n: formatAmount(e.limitTokens) })}
            </p>
            <p className="plan-est-sub">
              {t(lang, "est5hDetail", {
                used: formatAmount(e.usedTokens),
                msg: e.messages,
              })}
            </p>
          </>
        ) : (
          <p className="plan-note">{t(lang, "planNA", { plan: a.currentPlan })}</p>
        )}
        {hasTiers && (
          <>
            <p className="plan-rec-headline plan-rec-managed">
              {t(lang, "planEquiv", { rec: a.recommendedPlan })}
            </p>
            {a.tiers.map((tier) => (
              <TierBar key={tier.plan} tier={tier} lang={lang} managed />
            ))}
          </>
        )}
        {(e || hasTiers) && (
          <p className="plan-note">{t(lang, "est5hNote")}</p>
        )}
      </div>
    );
  }

  const headline =
    a.direction === "keep"
      ? t(lang, "planKeep")
      : a.direction === "upgrade"
      ? t(lang, "planUpgrade", { rec: a.recommendedPlan })
      : t(lang, "planDowngrade", { rec: a.recommendedPlan });
  return (
    <div className="stat-block">
      <span className="stat-title">{t(lang, "planRec")}</span>
      <p className={`plan-rec-headline plan-rec-${a.direction}`}>{headline}</p>
      {a.tiers.map((tier) => (
        <TierBar key={tier.plan} tier={tier} lang={lang} />
      ))}
      <p className="plan-note">{t(lang, "planEstimateNote")}</p>
    </div>
  );
}

/** 도구 하나의 통계 카드 — 요약·요금제추천·일별·주간비교·모델별·시간대별 */
function StatsCard({ s, lang }: { s: ToolStats; lang: Lang }) {
  const isReq = s.unit === "requests";
  const fmt = (n: number) => (isReq ? `${n}` : formatAmount(n));
  // 막대가 많으면(14/30일) 컬럼이 좁아져 텍스트가 창을 넘친다.
  // 막대 위 값은 숨기고, 날짜는 일정 간격으로만 표시한다.
  const dense = s.days.length > 10;
  const maxDay = Math.max(...s.days.map((d) => d.usage), 1);
  const maxHour = Math.max(...s.hourly, 1);
  const maxModel = Math.max(...s.models.map((m) => m.usage), 1);

  // 이번주 vs 지난주 증감률
  const wDelta =
    s.lastWeekUsage > 0
      ? ((s.thisWeekUsage - s.lastWeekUsage) / s.lastWeekUsage) * 100
      : null;
  const wClass =
    wDelta == null
      ? "trend-flat"
      : wDelta > 0
      ? "trend-up"
      : wDelta < 0
      ? "trend-down"
      : "trend-flat";

  return (
    <section className="card">
      <div className="card-head">
        <span className="tool">{s.tool}</span>
      </div>

      {/* 요약 */}
      <div className="today">
        <div className="today-item">
          <span className="today-val">{fmt(s.totalUsage)}</span>
          <span className="today-label">{t(lang, "statTotal")}</span>
        </div>
        <div className="today-item">
          <span className="today-val">{fmt(s.avgUsage)}</span>
          <span className="today-label">{t(lang, "statAvg")}</span>
        </div>
        <div className="today-item">
          <span className="today-val">{fmt(s.peakUsage)}</span>
          <span className="today-label">
            {t(lang, "statPeak")}
            {s.peakDate ? ` ${s.peakDate}` : ""}
          </span>
        </div>
        {s.totalCost > 0 && (
          <div className="today-item">
            <span className="today-val">${s.totalCost.toFixed(2)}</span>
            <span className="today-label">{t(lang, "apiValue")}</span>
          </div>
        )}
      </div>

      {/* 주간 한도 소진 — 주간 윈도우가 없는 도구는 빈 배열이라 그려지지 않는다 */}
      <WeeklyLimitBlock days={s.weeklyDays} lang={lang} />

      {/* 사용량 기반 요금제 추천 */}
      {s.planAdvice && <PlanAdviceBlock a={s.planAdvice} lang={lang} />}

      {/* 일별: 일수 많으면 추세 차트, 적으면 막대 */}
      {dense ? (
        <TrendChart days={s.days} fmt={fmt} unit={s.unit} lang={lang} />
      ) : (
        <div className="hist-bars">
          {s.days.map((d) => (
            <div className="hist-col" key={d.date}>
              <span className="hist-val">{fmt(d.usage)}</span>
              <div
                className="hist-bar"
                style={{ height: `${Math.max(4, (d.usage / maxDay) * 100)}%` }}
                title={`${d.date}: ${fmt(d.usage)} ${unitLabel(s.unit)} · ${d.count}${
                  d.cost > 0 ? ` · $${d.cost.toFixed(2)}` : ""
                }`}
              />
              <span className="hist-date">{d.date}</span>
            </div>
          ))}
        </div>
      )}

      {/* 주간 비교 */}
      <div className="stat-block">
        <span className="stat-title">{t(lang, "weekCompare")}</span>
        <div className="week-cmp">
          <span>
            {t(lang, "thisWeek")} <b>{fmt(s.thisWeekUsage)}</b>
          </span>
          <span className="week-vs">vs</span>
          <span>
            {t(lang, "lastWeek")} <b>{fmt(s.lastWeekUsage)}</b>
          </span>
          {wDelta != null && (
            <span className={`week-delta ${wClass}`}>
              {wDelta > 0 ? "▲" : wDelta < 0 ? "▼" : "—"}{" "}
              {Math.abs(wDelta).toFixed(0)}%
            </span>
          )}
        </div>
      </div>

      {/* 모델별 분해 */}
      {s.models.length > 0 && (
        <div className="stat-block">
          <span className="stat-title">{t(lang, "modelBreakdown")}</span>
          {s.models.map((m) => (
            <div className="model-row" key={m.model}>
              <span className="model-name">{m.model}</span>
              <div className="model-track">
                <div
                  className="model-fill"
                  style={{ width: `${(m.usage / maxModel) * 100}%` }}
                />
              </div>
              <span className="model-val">
                {fmt(m.usage)}
                {m.cost > 0 ? ` · $${m.cost.toFixed(2)}` : ""}
              </span>
            </div>
          ))}
        </div>
      )}

      {/* 시간대별 패턴 */}
      <div className="stat-block">
        <span className="stat-title">{t(lang, "hourlyPattern")}</span>
        <div className="hourly-bars">
          {s.hourly.map((h, i) => (
            <div
              className="hourly-bar"
              key={i}
              style={{ height: `${Math.max(3, (h / maxHour) * 100)}%` }}
              title={`${i}–${i + 1}: ${fmt(h)} ${unitLabel(s.unit)}`}
            />
          ))}
        </div>
        <div className="hourly-axis">
          <span>0</span>
          <span>6</span>
          <span>12</span>
          <span>18</span>
          <span>23</span>
        </div>
      </div>
    </section>
  );
}

/** 통계 전용 창 — 기간 토글 + 도구별 요약·추세 */
function HistoryView() {
  const [stats, setStats] = useState<ToolStats[]>([]);
  const [lang, setLang] = useState<Lang>(resolveLang(null));
  const [loading, setLoading] = useState(true);
  const [days, setDays] = useState(7);

  useEffect(() => {
    invoke<Settings>("get_settings")
      .then((s) => setLang(resolveLang(s.language)))
      .catch(() => {});
    invoke("track_event", { event: "history_opened" }).catch(() => {});
  }, []);

  const load = useCallback(() => {
    setLoading(true);
    const start = Date.now();
    invoke<ToolStats[]>("get_stats", { days })
      .then(setStats)
      .catch(() => {})
      .finally(() => {
        // 캐시 적중 시 즉시 반환되면 스켈레톤이 깜빡일 새도 없다.
        // 최소 350ms는 유지해 로딩 상태가 인지되게 한다.
        const wait = Math.max(0, 350 - (Date.now() - start));
        setTimeout(() => setLoading(false), wait);
      });
  }, [days]);

  // 마운트 + 기간 변경 시 조회
  useEffect(() => {
    load();
  }, [load]);

  // 숨겨뒀던 창이 다시 열릴 때(웹뷰 재부팅 없이 show) 데이터 갱신
  useEffect(() => {
    const un = listen("history-refresh", () => load());
    return () => {
      un.then((f) => f());
    };
  }, [load]);

  const active = stats.filter((s) => s.days.length > 0);

  return (
    <main className="container win-framed">
      <div className="win-titlebar-bg" />
      <div className="period-bar">
        <span className="period-label">{t(lang, "period")}</span>
        <select
          className="period-select"
          value={days}
          onChange={(e) => setDays(Number(e.target.value))}
        >
          {PERIODS.map((p) => (
            <option key={p} value={p}>
              {p === 365 ? t(lang, "period1y") : t(lang, "nDays", { n: p })}
            </option>
          ))}
        </select>
      </div>

      {loading && (
        <>
          <StatsCardSkeleton />
          <StatsCardSkeleton />
        </>
      )}
      {!loading && active.length === 0 && (
        <p className="muted">{t(lang, "noHistory")}</p>
      )}

      {!loading &&
        active.map((s) => <StatsCard key={s.tool} s={s} lang={lang} />)}
    </main>
  );
}

function App() {
  const label = getCurrentWindow().label;
  if (label === "settings") return <SettingsView />;
  if (label === "history") return <HistoryView />;
  if (label === "pet") return <PetOverlay />;
  return <Dashboard />;
}

export default App;
