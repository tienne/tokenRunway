//! 에이전트 활동 감지 — pet이 "지금 도는 중 / 막 끝났음 / 쉬는 중"에 반응하게 한다.
//!
//! 잔여율(`runway`)과는 다른 축이다. 잔여율은 30초에 한 번 봐도 되지만 활동 상태는
//! 1~2초 안에 바뀌어야 살아 있어 보이므로, 네트워크도 전체 파싱도 하지 않는다 —
//! **도구별로 가장 최근 파일 하나의 꼬리만** 읽는다. 트리 탐색은 [`SCAN_TTL`] 주기로만
//! 하고 그 사이에는 찾아둔 경로를 그대로 다시 읽는다.
//! (실측: 세션 파일 1,300개·2GB짜리 `~/.claude/projects`에서 탐색 한 번이 80ms,
//! 탐색 없는 폴링은 파일 5개 stat + 꼬리 읽기라 사실상 공짜다.)
//!
//! 도구별 신호 품질이 다르다 (실데이터로 확인, 2026-08-20):
//! - Codex: `event_msg`의 `task_started`/`task_complete` — 턴 경계가 명시적
//! - Grok: `sessionUpdate == "turn_completed"` — 완료 이벤트가 따로 있다
//! - Claude Code: 훅(`trw hook claude`)을 깔았으면 세션별 이벤트, 아니면 마지막 엔트리 종류로
//!   추론 (`assistant`+`tool_use`면 도는 중)
//! - Gemini: 마지막 엔트리가 `user`/`gemini`/`error` — `error`로 실패까지 구분된다
//! - Antigravity: 완료 이벤트가 없어 `streamGenerateContent` 신선도로만 추론

use chrono::DateTime;
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// 꼬리에서 읽을 최대 바이트 — 한 턴 분량 로그는 이 안에 들어온다.
const TAIL_BYTES: u64 = 256 * 1024;
/// 파일이 이만큼 안 자라면 세션이 죽은 것으로 본다. 턴 중간에 도구를 종료하면 마지막
/// 줄이 "도는 중" 모양으로 남아 pet이 영원히 일하게 되는 걸 막는 안전장치다.
// app-server 폴백의 활성 홈 판정과 훅 상태 신선도가 같은 5분을 쓰도록 연다 — 기준이 둘로 갈리면 서로 다른 판정을 낸다.
pub const STALE_MS: i64 = 5 * 60 * 1000;
/// 턴이 끝난 뒤 이 시간 동안만 `JustDone`. 지나면 `Idle`.
const JUST_DONE_MS: i64 = 8_000;
/// 완료 이벤트가 없는 도구(Antigravity)에서 "요청 흔적이 이 안에 있으면 도는 중".
const BURST_MS: i64 = 12_000;
/// 최근 파일 탐색 결과를 재사용하는 기간. 꼬리 읽기는 매 폴링마다, 탐색은 이 주기로만.
const SCAN_TTL: Duration = Duration::from_secs(10);
/// 트리 탐색 최대 깊이 — 도구별 세션 경로 깊이에 맞춘 상한(무한 재귀·심링크 루프 방지).
const MAX_DEPTH: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityState {
    /// 에이전트가 지금 응답·툴 실행 중.
    Working,
    /// 사람이 답해야 진행된다(권한 요청 등). 지금은 Claude Code 훅에서만 나온다.
    Waiting,
    /// 턴이 방금(`JUST_DONE_MS` 안에) 끝났다.
    JustDone,
    /// 아무것도 안 하는 중.
    Idle,
}

/// 도구 하나의 활동 상태.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivity {
    /// 표시명 — `RunwayStatus::tool`과 같은 문자열이라 프론트에서 짝지을 수 있다.
    pub tool: String,
    pub state: ActivityState,
    /// 이 상태의 근거가 된 마지막 이벤트 시각(epoch ms).
    pub since_ms: i64,
    /// 실패로 끝났는지. Gemini의 `error` 엔트리와 Claude Code 훅의 StopFailure에서 판별된다.
    pub failed: bool,
}

impl AgentActivity {
    fn new(tool: &str, state: ActivityState, since_ms: i64) -> Self {
        Self {
            tool: tool.to_string(),
            state,
            since_ms,
            failed: false,
        }
    }
}

/// 턴 종료 시각을 상태로 옮긴다 — 방금이면 손을 흔들고, 오래됐으면 쉰다.
fn done_state(done_ms: i64, now_ms: i64) -> ActivityState {
    if now_ms - done_ms <= JUST_DONE_MS {
        ActivityState::JustDone
    } else {
        ActivityState::Idle
    }
}

// --- 파일 찾기 / 꼬리 읽기 ---

/// 탐색 결과 캐시 — `(찾은 시각, 도구별 최근 파일)`. 다섯 도구를 한 덩어리로 갱신한다.
static SCAN_CACHE: Mutex<Option<(Instant, Vec<Option<PathBuf>>)>> = Mutex::new(None);

fn mtime_ms(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let dur = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as i64)
}

/// `roots` 아래에서 `matches`를 만족하는 파일 중 mtime이 가장 최근인 것.
fn newest_file(roots: &[PathBuf], matches: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, i64)> = None;
    for root in roots {
        walk(root, matches, 0, &mut best);
    }
    best.map(|(p, _)| p)
}

fn walk(
    dir: &Path,
    matches: &dyn Fn(&str) -> bool,
    depth: usize,
    best: &mut Option<(PathBuf, i64)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk(&path, matches, depth + 1, best);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !matches(name) {
            continue;
        }
        let Some(mt) = mtime_ms(&path) else { continue };
        if best.as_ref().is_none_or(|(_, b)| mt > *b) {
            *best = Some((path, mt));
        }
    }
}

/// 파일 끝에서 최대 `TAIL_BYTES`만 읽어 줄 단위로 돌려준다.
/// 앞이 잘렸으면 첫 줄(불완전한 줄)은 버린다.
fn tail_lines(path: &Path) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

fn rfc3339_ms(raw: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.timestamp_millis())
}

// --- 도구별 판정 ---

/// 판정 대상 도구 목록 — 표시명, 탐색 루트, 파일명 조건, 꼬리 해석 함수.
/// 순서가 `SCAN_CACHE`의 인덱스라서 바꾸면 캐시 의미도 바뀐다(추가는 뒤에).
type Detector = fn(&[String], i64, i64) -> Option<AgentActivity>;
/// `(표시명, 탐색 루트, 파일명 조건, 해석 함수)`.
type Target = (&'static str, Vec<PathBuf>, fn(&str) -> bool, Detector);

fn targets() -> Vec<Target> {
    let home = dirs::home_dir();
    let grok_root = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".grok")));
    vec![
        (
            "Claude Code",
            home.as_ref().map(|h| h.join(".claude").join("projects")).into_iter().collect(),
            |n: &str| n.ends_with(".jsonl"),
            detect_claude as Detector,
        ),
        (
            "Codex",
            crate::providers::codex::session_roots(),
            |n: &str| n.starts_with("rollout-") && n.ends_with(".jsonl"),
            detect_codex as Detector,
        ),
        (
            "Grok",
            grok_root.map(|r| r.join("sessions")).into_iter().collect(),
            |n: &str| n == "updates.jsonl",
            detect_grok as Detector,
        ),
        (
            "Gemini",
            home.as_ref().map(|h| h.join(".gemini").join("tmp")).into_iter().collect(),
            |n: &str| n.starts_with("session-") && n.ends_with(".jsonl"),
            detect_gemini as Detector,
        ),
        (
            "Antigravity",
            home.as_ref()
                .map(|h| h.join(".gemini").join("antigravity-cli").join("log"))
                .into_iter()
                .collect(),
            |n: &str| n.starts_with("cli-") && n.ends_with(".log"),
            detect_antigravity as Detector,
        ),
    ]
}

/// Claude Code — 마지막 `assistant`/`user` 엔트리로 추론한다.
/// `assistant`가 `tool_use`로 끊겼으면 툴 실행 중, `user`(프롬프트든 tool_result든)면
/// 응답을 기다리는 중, 그 밖의 `assistant`면 턴이 끝난 것.
/// `attachment`·`system`·`ai-title` 같은 부수 엔트리는 턴 경계와 무관해 건너뛴다.
fn detect_claude(lines: &[String], now_ms: i64, mtime_ms: i64) -> Option<AgentActivity> {
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(rfc3339_ms)
            .unwrap_or(mtime_ms);
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                let stop = v
                    .get("message")
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|s| s.as_str());
                return Some(if stop == Some("tool_use") {
                    AgentActivity::new("Claude Code", ActivityState::Working, ts)
                } else {
                    AgentActivity::new("Claude Code", done_state(ts, now_ms), ts)
                });
            }
            Some("user") => {
                return Some(AgentActivity::new(
                    "Claude Code",
                    ActivityState::Working,
                    ts,
                ));
            }
            _ => continue,
        }
    }
    None
}

/// Codex — `task_started`/`task_complete` 이벤트가 턴 경계를 그대로 알려준다.
fn detect_codex(lines: &[String], now_ms: i64, mtime_ms: i64) -> Option<AgentActivity> {
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let kind = v
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(|t| t.as_str());
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(rfc3339_ms)
            .unwrap_or(mtime_ms);
        match kind {
            Some("task_started") => {
                return Some(AgentActivity::new("Codex", ActivityState::Working, ts))
            }
            Some("task_complete") => {
                return Some(AgentActivity::new("Codex", done_state(ts, now_ms), ts))
            }
            _ => continue,
        }
    }
    None
}

/// Grok — `turn_completed`가 완료, 그 밖의 스트리밍 업데이트는 진행 중.
/// `hook_execution`·`session_recap`은 턴이 끝난 뒤에도 붙어서 판정에서 뺀다.
fn detect_grok(lines: &[String], now_ms: i64, mtime_ms: i64) -> Option<AgentActivity> {
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let params = v.get("params");
        let kind = params
            .and_then(|p| p.get("update"))
            .and_then(|u| u.get("sessionUpdate"))
            .or_else(|| params.and_then(|p| p.get("sessionUpdate")))
            .and_then(|s| s.as_str());
        let ts = params
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("agentTimestampMs"))
            .and_then(|t| t.as_i64())
            .unwrap_or(mtime_ms);
        match kind {
            Some("turn_completed") => {
                return Some(AgentActivity::new("Grok", done_state(ts, now_ms), ts))
            }
            Some("hook_execution") | Some("session_recap") | None => continue,
            Some(_) => return Some(AgentActivity::new("Grok", ActivityState::Working, ts)),
        }
    }
    None
}

/// Gemini — `chats/session-*.jsonl`의 마지막 엔트리. `user`면 응답 대기, `gemini`면 완료,
/// `error`면 실패로 끝난 것. `$set`(lastUpdated)·`info` 줄은 턴과 무관해 건너뛴다.
fn detect_gemini(lines: &[String], now_ms: i64, mtime_ms: i64) -> Option<AgentActivity> {
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(rfc3339_ms)
            .unwrap_or(mtime_ms);
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => return Some(AgentActivity::new("Gemini", ActivityState::Working, ts)),
            Some("gemini") => {
                return Some(AgentActivity::new("Gemini", done_state(ts, now_ms), ts))
            }
            Some("error") => {
                let mut a = AgentActivity::new("Gemini", done_state(ts, now_ms), ts);
                a.failed = true;
                return Some(a);
            }
            _ => continue,
        }
    }
    None
}

/// Antigravity — 완료 이벤트가 없다. 작업 중에는 `streamGenerateContent` 호출이 몇 초
/// 간격으로 쏟아지므로 그 줄의 신선도로만 판정하고, `JustDone`은 만들지 않는다
/// (끝난 시점을 모르는데 손을 흔들면 엉뚱한 타이밍에 흔든다).
///
/// **mtime을 그대로 쓰면 안 된다** — 이 로그는 언어 서버라 아무것도 안 할 때도
/// `quotaRefreshLoop` 같은 줄을 계속 써서 항상 "작업 중"이 된다. 그래서 마지막 줄과
/// 마지막 요청 줄의 glog 시각차를 재고, 거기에 파일 자체의 경과를 더한다.
fn detect_antigravity(lines: &[String], now_ms: i64, mtime_ms: i64) -> Option<AgentActivity> {
    let last_tod = lines.iter().rev().find_map(|l| glog_tod_ms(l))?;
    let file_age = (now_ms - mtime_ms).max(0);

    for line in lines.iter().rev() {
        if line.contains("Terminal gone, shutting down") || line.contains("CLI program exited") {
            return Some(AgentActivity::new(
                "Antigravity",
                ActivityState::Idle,
                mtime_ms,
            ));
        }
        if !line.contains("streamGenerateContent")
            && !line.contains("Sending user message to conversation")
        {
            continue;
        }
        let Some(tod) = glog_tod_ms(line) else {
            continue;
        };
        // 자정을 넘겼으면 음수가 되므로 하루를 더해 되돌린다.
        let mut gap = last_tod - tod;
        if gap < 0 {
            gap += 24 * 3600 * 1000;
        }
        let elapsed = file_age + gap;
        let state = if elapsed <= BURST_MS {
            ActivityState::Working
        } else {
            ActivityState::Idle
        };
        return Some(AgentActivity::new("Antigravity", state, now_ms - elapsed));
    }
    None
}

/// glog 머리(`I0622 17:55:06.968994`)에서 그날 안의 시각을 ms로 뽑는다.
/// 연도가 없어 절대 시각을 만들 수 없으므로, 같은 파일 안의 두 줄을 비교할 때만 쓴다.
fn glog_tod_ms(line: &str) -> Option<i64> {
    let clock = line.split_whitespace().nth(1)?;
    let mut parts = clock.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    let (s_str, frac_str) = rest.split_once('.').unwrap_or((rest, "0"));
    let s: i64 = s_str.parse().ok()?;
    // glog는 마이크로초 6자리 — 앞 3자리만 ms로 쓴다.
    let ms: i64 = frac_str.chars().take(3).collect::<String>().parse().ok()?;
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * 1000 + ms)
}

// --- 훅 상태 (Claude Code / Codex) ---

/// `trw hook claude|codex`가 세션마다 남기는 상태 한 줄. 같은 모양을 `trw`가 쓴다.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct HookState {
    state: String,
    at_ms: i64,
    #[serde(default)]
    failed: bool,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    target: Option<serde_json::Value>,
    #[serde(default)]
    transcript: Option<String>,
    #[serde(default)]
    config_dir: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    pid_start: Option<u64>,
}

/// 훅을 거는 도구. `prefix`는 상태 파일 이름 앞부분(`trw`의 `write_hook_state`와 같다).
#[derive(Clone, Copy)]
struct HookTool {
    name: &'static str,
    prefix: &'static str,
    installed: fn() -> bool,
}

const HOOK_TOOLS: &[HookTool] = &[
    HookTool {
        name: "Claude Code",
        prefix: "claude",
        installed: crate::settings::claude_hooks_installed,
    },
    HookTool {
        name: "Codex",
        prefix: "codex",
        installed: crate::settings::codex_hooks_installed,
    },
];

fn hooks_dir() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("hooks"))
}

/// 훅 상태 디렉토리에서 `prefix-*.json`만 읽는다. 쓰는 중인 임시 파일(`.`으로 시작)은 건너뛴다.
fn read_hook_states(dir: &Path, prefix: &str) -> Vec<HookState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let head = format!("{prefix}-");
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&head) && name.ends_with(".json")
        })
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str(raw.trim()).ok()
        })
        .collect()
}

/// 세션 하나의 지금 상태. 훅이 남긴 상태에 세션 프로세스 생존과 transcript를 더해 정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Working,
    Waiting,
    Done,
    Idle,
}

/// 상태를 정하는 데 필요한 바깥 사실. 테스트에서 주입하려고 묶었다.
struct Evidence {
    /// 세션 프로세스가 살아 있는지. pid를 모르면 None.
    alive: Option<bool>,
    /// transcript 마지막 수정 시각.
    transcript_ms: Option<i64>,
    /// transcript 마지막 엔트리가 사용자 중단(Esc)인지. 필요할 때만 읽는다.
    interrupted: Box<dyn Fn() -> bool>,
}

/// 훅 이벤트 뒤 transcript가 이만큼 넘게 더 자랐으면 훅이 모르는 일이 있었다고 본다.
/// 훅과 transcript 쓰기가 같은 순간에 일어나 생기는 시차는 넘기지 않게 여유를 둔다.
const TRANSCRIPT_SLACK_MS: i64 = 1_000;

/// 세션 상태를 정한다. None이면 죽은 세션이라 목록에서 뺀다.
///
/// 신선한(`STALE_MS` 안) 상태는 그대로 믿는다. 오래된 상태는 세션 프로세스가 살아 있을 때만
/// 쓴다 — PreToolUse 뒤 10분짜리 빌드가 도는 동안엔 이벤트가 없어 5분 컷에 걸리는데,
/// 컷을 없애면 강제 종료된 세션이 영원히 작업 중으로 남는다. pid가 이 둘을 가른다.
/// pid는 기간을 늘리기만 하고 줄이지 않는다 — pid를 잘못 짚어도 예전 5분 규칙으로 돌아갈 뿐이다.
///
/// Esc로 턴을 끊으면 Stop 훅이 안 온다(공식 문서: 사용자 중단이면 Stop이 돌지 않는다).
/// 그때 transcript 끝에 중단 표시가 붙으므로, 작업 중·대기 상태 뒤로 transcript가 자랐으면
/// 그 표시를 확인해 쉬는 중으로 내린다.
fn resolve(st: &HookState, now_ms: i64, ev: &Evidence) -> Option<SessionState> {
    let fresh = now_ms - st.at_ms <= STALE_MS;
    if !fresh && ev.alive != Some(true) {
        return None;
    }
    let state = match st.state.as_str() {
        "working" => SessionState::Working,
        "waiting" => SessionState::Waiting,
        "done" => SessionState::Done,
        _ => SessionState::Idle,
    };
    let grew = ev
        .transcript_ms
        .filter(|t| *t > st.at_ms + TRANSCRIPT_SLACK_MS);
    match state {
        SessionState::Working | SessionState::Waiting if grew.is_some() && (ev.interrupted)() => {
            Some(SessionState::Idle)
        }
        SessionState::Working if !fresh => match grew {
            // 훅 없이 transcript만 자랐다 — 그것도 오래전이면 멈춘 것으로 본다.
            Some(t) if now_ms - t > STALE_MS => Some(SessionState::Idle),
            _ => Some(SessionState::Working),
        },
        // 오래된 완료는 그냥 쉬는 중이다. 막 끝남은 신선할 때만 의미가 있다.
        SessionState::Done if !fresh => Some(SessionState::Idle),
        s => Some(s),
    }
}

/// 세션 하나를 보여줄 꼴로.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSession {
    pub tool: String,
    pub session: String,
    pub state: ActivityState,
    pub failed: bool,
    pub since_ms: i64,
    /// cwd 마지막 마디 (오르카는 워크트리 이름).
    pub project: Option<String>,
    pub target: crate::inbox::Target,
    pub config_dir: Option<String>,
}

fn activity_state(state: SessionState, at_ms: i64, now_ms: i64) -> ActivityState {
    match state {
        SessionState::Working => ActivityState::Working,
        SessionState::Waiting => ActivityState::Waiting,
        SessionState::Done => done_state(at_ms, now_ms),
        SessionState::Idle => ActivityState::Idle,
    }
}

fn last_segment(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

fn to_session(tool: &str, st: &HookState, state: SessionState, now_ms: i64) -> HookSession {
    let target = st
        .target
        .clone()
        .and_then(|t| serde_json::from_value(t).ok())
        .unwrap_or(crate::inbox::Target::None);
    HookSession {
        tool: tool.to_string(),
        session: st.session.clone().unwrap_or_default(),
        state: activity_state(state, st.at_ms, now_ms),
        failed: st.failed,
        since_ms: st.at_ms,
        project: st
            .label
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| st.cwd.as_deref().and_then(last_segment)),
        target,
        config_dir: st.config_dir.clone(),
    }
}

/// 살아 있는 세션들. 실제 프로세스와 transcript를 본다.
fn live_sessions(tool: &HookTool, now_ms: i64) -> Vec<HookSession> {
    let Some(dir) = hooks_dir() else {
        return Vec::new();
    };
    read_hook_states(&dir, tool.prefix)
        .iter()
        .filter_map(|st| {
            let transcript = st.transcript.clone().map(PathBuf::from);
            let ev = Evidence {
                alive: st.pid.map(|pid| proc_info::alive(pid, st.pid_start)),
                transcript_ms: transcript.as_deref().and_then(mtime_ms),
                interrupted: Box::new(move || {
                    transcript.as_deref().is_some_and(ends_with_interrupt)
                }),
            };
            resolve(st, now_ms, &ev).map(|state| to_session(tool.name, st, state, now_ms))
        })
        .collect()
}

/// 세션들을 도구 하나의 활동으로 합친다. 세션이 없으면 None — 로그 추론으로 넘긴다.
///
/// 하나라도 도는 중이면 도는 중이다. 최근 파일 하나만 보는 로그 추론은 다른 세션이 끝난
/// 순간 도는 세션을 놓치는데, 훅은 세션마다 파일이 따로라 안 놓친다. 도는 게 없으면
/// 사람을 기다리는 세션이 그다음이다.
fn merge_sessions(tool: &str, sessions: &[HookSession]) -> Option<AgentActivity> {
    let newest = |state: ActivityState| {
        sessions
            .iter()
            .filter(|s| s.state == state)
            .max_by_key(|s| s.since_ms)
    };
    let pick = newest(ActivityState::Working)
        .or_else(|| newest(ActivityState::Waiting))
        .or_else(|| sessions.iter().max_by_key(|s| s.since_ms))?;
    Some(AgentActivity {
        tool: tool.to_string(),
        state: pick.state,
        since_ms: pick.since_ms,
        failed: pick.failed,
    })
}

/// transcript 꼬리의 마지막 대화 엔트리가 사용자 중단 표시인지.
/// 표시 문구는 실제 transcript에서 확인했다: `[Request interrupted by user]`와
/// `[Request interrupted by user for tool use]`.
fn ends_with_interrupt(path: &Path) -> bool {
    let Some(mt) = mtime_ms(path) else {
        return false;
    };
    let key = path.to_path_buf();
    let cached = INTERRUPT_CACHE.lock().ok().and_then(|c| {
        c.as_ref()?
            .get(&key)
            .filter(|(m, _)| *m == mt)
            .map(|(_, v)| *v)
    });
    if let Some(hit) = cached {
        return hit;
    }
    let v = last_entry_is_interrupt(&tail_lines(path));
    if let Ok(mut guard) = INTERRUPT_CACHE.lock() {
        let c = guard.get_or_insert_with(Default::default);
        // 끝난 세션의 transcript가 계속 쌓이지 않게 크게 자라면 비운다.
        if c.len() > 64 {
            c.clear();
        }
        c.insert(key, (mt, v));
    }
    v
}

/// 같은 transcript를 1.5초마다 다시 읽지 않게 mtime이 그대로면 직전 판정을 쓴다.
static INTERRUPT_CACHE: Mutex<Option<std::collections::HashMap<PathBuf, (i64, bool)>>> =
    Mutex::new(None);

fn last_entry_is_interrupt(lines: &[String]) -> bool {
    const MARK: &str = "[Request interrupted by user";
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                let content = v.get("message").and_then(|m| m.get("content"));
                return match content {
                    Some(serde_json::Value::String(s)) => s.starts_with(MARK),
                    Some(serde_json::Value::Array(items)) => items.iter().any(|it| {
                        it.get("type").and_then(|t| t.as_str()) == Some("text")
                            && it
                                .get("text")
                                .and_then(|t| t.as_str())
                                .is_some_and(|t| t.starts_with(MARK))
                    }),
                    _ => false,
                };
            }
            Some("assistant") => return false,
            _ => continue,
        }
    }
    false
}

/// 세션 프로세스 생존 확인. `trw`의 `proc_info`와 같은 libSystem 호출이다 — 앱이 시작 시각까지
/// 맞춰봐야 pid가 재사용된 다른 프로세스를 살아 있는 세션으로 착각하지 않는다.
#[cfg(target_os = "macos")]
mod proc_info {
    const PROC_PIDTBSDINFO: i32 = 3;

    /// `<sys/proc_info.h>`의 `proc_bsdinfo`와 같은 배치.
    #[repr(C)]
    struct ProcBsdInfo {
        head: [u32; 12],
        comm: [u8; 16],
        name: [u8; 32],
        tail: [u32; 6],
        start_tvsec: u64,
        start_tvusec: u64,
    }

    extern "C" {
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut u8, size: i32) -> i32;
    }

    pub fn start_sec(pid: u32) -> Option<u64> {
        let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::zeroed();
        let size = std::mem::size_of::<ProcBsdInfo>() as i32;
        // SAFETY: 버퍼 크기를 그대로 넘기고, 커널이 채운 바이트 수가 구조체 크기와 같을 때만 읽는다.
        let n = unsafe {
            proc_pidinfo(pid as i32, PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
        };
        (n == size).then(|| unsafe { info.assume_init() }.start_tvsec)
    }

    pub fn alive(pid: u32, start: Option<u64>) -> bool {
        match (start_sec(pid), start) {
            (Some(now), Some(then)) => now == then,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod proc_info {
    pub fn alive(_pid: u32, _start: Option<u64>) -> bool {
        false
    }
}

fn hook_activity(tool: &HookTool, now_ms: i64) -> Option<AgentActivity> {
    if !(tool.installed)() {
        return None;
    }
    merge_sessions(tool.name, &live_sessions(tool, now_ms))
}

/// 세션 목록 응답. 훅이 하나도 안 깔려 있으면 `installed`가 false라 프론트가 목록을 숨긴다.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionList {
    pub installed: bool,
    pub sessions: Vec<HookSession>,
}

/// 훅이 깔린 도구의 살아 있는 세션. 최근에 움직인 순서.
pub fn sessions() -> SessionList {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let disabled = crate::settings::disabled_tools();
    let mut installed = false;
    let mut sessions = Vec::new();
    for tool in HOOK_TOOLS {
        if !(tool.installed)() {
            continue;
        }
        installed = true;
        if disabled.iter().any(|d| d == tool.name) {
            continue;
        }
        sessions.extend(live_sessions(tool, now_ms));
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.since_ms));
    SessionList { installed, sessions }
}

/// 상태 파일 읽기와 프로세스 확인이 blocking이라 풀에서 돈다.
#[tauri::command]
pub async fn get_sessions() -> SessionList {
    tauri::async_runtime::spawn_blocking(sessions)
        .await
        .unwrap_or(SessionList {
            installed: false,
            sessions: Vec::new(),
        })
}

/// 세션 줄을 누르면 그 세션으로 데려간다. 랜딩 정보는 목록을 그린 뒤 바뀌었을 수 있어 다시 읽는다.
#[tauri::command]
pub async fn land_session(tool: String, session: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ht = HOOK_TOOLS
            .iter()
            .find(|t| t.name == tool)
            .ok_or("알 수 없는 도구입니다")?;
        let dir = hooks_dir().ok_or("홈 디렉토리를 찾을 수 없습니다")?;
        let st = read_hook_states(&dir, ht.prefix)
            .into_iter()
            .find(|s| s.session.as_deref() == Some(session.as_str()))
            .ok_or("세션을 찾을 수 없습니다")?;
        let target = st
            .target
            .and_then(|t| serde_json::from_value(t).ok())
            .unwrap_or(crate::inbox::Target::None);
        crate::landing::land(&target)
    })
    .await
    .map_err(|e| e.to_string())?
}

// --- 진입점 ---

/// 활성화된 도구들의 현재 활동 상태. blocking I/O — 호출부에서 `spawn_blocking`.
pub fn detect_all() -> Vec<AgentActivity> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let disabled = crate::settings::disabled_tools();
    let targets = targets();

    // 탐색은 SCAN_TTL 주기로만. 그 사이에는 찾아둔 경로를 다시 읽는다.
    let paths = {
        let mut guard = SCAN_CACHE.lock().ok();
        let cached = guard
            .as_ref()
            .and_then(|g| g.as_ref())
            .filter(|(at, _)| at.elapsed() < SCAN_TTL)
            .map(|(_, p)| p.clone());
        match cached {
            Some(p) => p,
            None => {
                let fresh: Vec<Option<PathBuf>> = targets
                    .iter()
                    .map(|(_, root, matches, _)| {
                        newest_file(root, matches)
                    })
                    .collect();
                if let Some(g) = guard.as_mut() {
                    **g = Some((Instant::now(), fresh.clone()));
                }
                fresh
            }
        }
    };

    let mut out = Vec::new();
    for ((tool, _, _, detect), path) in targets.iter().zip(paths) {
        if disabled.iter().any(|d| d == tool) {
            continue;
        }
        // 훅이 깔린 도구는 훅 상태가 먼저다. 살아 있는 세션이 없으면 로그 추론으로 넘긴다.
        if let Some(ht) = HOOK_TOOLS.iter().find(|t| t.name == *tool) {
            if let Some(a) = hook_activity(ht, now_ms) {
                out.push(a);
                continue;
            }
        }
        let Some(path) = path else { continue };
        let Some(mt) = mtime_ms(&path) else { continue };
        // 오래 안 자란 파일은 죽은 세션 — 마지막 줄이 "도는 중" 모양이어도 쉬는 것으로 본다.
        if now_ms - mt > STALE_MS {
            out.push(AgentActivity::new(tool, ActivityState::Idle, mt));
            continue;
        }
        let lines = tail_lines(&path);
        // 턴 경계를 못 찾으면(막 띄워 아직 아무것도 안 돌린 세션) 쉬는 것으로 내보낸다 —
        // 목록에서 아예 빠지면 프론트가 "데이터 없음"과 구분할 수 없다.
        out.push(
            detect(&lines, now_ms, mt)
                .unwrap_or_else(|| AgentActivity::new(tool, ActivityState::Idle, mt)),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_786_000_000_000;

    fn codex_complete_rollout(ts_ms: i64) -> Vec<String> {
        let ts = chrono::DateTime::from_timestamp_millis(ts_ms).unwrap().to_rfc3339();
        vec![
            format!(r#"{{"type":"event_msg","timestamp":"{ts}","payload":{{"type":"task_started"}}}}"#),
            format!(r#"{{"type":"event_msg","timestamp":"{ts}","payload":{{"type":"task_complete"}}}}"#),
        ]
    }

    fn hook(state: &str, at_ms: i64) -> HookState {
        HookState {
            state: state.to_string(),
            at_ms,
            failed: false,
            session: Some(format!("s-{at_ms}")),
            cwd: Some("/w/my-project".into()),
            label: None,
            target: None,
            transcript: None,
            config_dir: None,
            pid: None,
            pid_start: None,
        }
    }

    fn ev(alive: Option<bool>, transcript_ms: Option<i64>, interrupted: bool) -> Evidence {
        Evidence {
            alive,
            transcript_ms,
            interrupted: Box::new(move || interrupted),
        }
    }

    fn sessions_of(states: &[HookState], evidence: impl Fn() -> Evidence) -> Vec<HookSession> {
        states
            .iter()
            .filter_map(|st| resolve(st, NOW, &evidence()).map(|s| to_session("Codex", st, s, NOW)))
            .collect()
    }

    const SIX_MIN: i64 = 6 * 60 * 1000;

    #[test]
    fn fresh_working_hook_beats_completed_rollout() {
        // 다른 세션이 막 끝나 rollout은 완료지만 훅으로는 아직 도는 세션이 있다.
        let rollout = detect_codex(&codex_complete_rollout(NOW - 1_000), NOW, NOW).unwrap();
        assert_eq!(rollout.state, ActivityState::JustDone);
        let states = vec![hook("done", NOW - 1_000), hook("working", NOW - 30_000)];
        let a = merge_sessions("Codex", &sessions_of(&states, || ev(None, None, false))).unwrap();
        assert_eq!(a.state, ActivityState::Working);
        assert_eq!(a.since_ms, NOW - 30_000);
    }

    #[test]
    fn stale_hook_without_live_process_falls_back_to_rollout() {
        let states = vec![hook("working", NOW - SIX_MIN)];
        assert!(merge_sessions("Codex", &sessions_of(&states, || ev(None, None, false))).is_none());
        // 강제 종료된 세션 — 프로세스가 없으면 목록에서 빠진다.
        assert!(sessions_of(&states, || ev(Some(false), None, false)).is_empty());
    }

    #[test]
    fn long_tool_stays_working_while_process_lives() {
        // PreToolUse 뒤 10분짜리 빌드 — 이벤트도 transcript 변화도 없지만 세션은 살아 있다.
        let st = hook("working", NOW - 10 * 60 * 1000);
        assert_eq!(resolve(&st, NOW, &ev(Some(true), Some(st.at_ms), false)), Some(SessionState::Working));
        // 같은 상황에 프로세스가 죽었으면 뺀다.
        assert_eq!(resolve(&st, NOW, &ev(Some(false), Some(st.at_ms), false)), None);
        // 훅 없이 transcript만 자랐다가 그것도 오래 멈췄으면 쉬는 중이다.
        let grew_long_ago = st.at_ms + 60_000;
        assert_eq!(resolve(&st, NOW, &ev(Some(true), Some(grew_long_ago), false)), Some(SessionState::Idle));
    }

    #[test]
    fn esc_interrupt_turns_working_into_idle() {
        // Esc로 끊으면 Stop이 안 오고 transcript 끝에 중단 표시만 붙는다.
        let st = hook("working", NOW - 20_000);
        assert_eq!(resolve(&st, NOW, &ev(Some(true), Some(NOW - 5_000), true)), Some(SessionState::Idle));
        assert_eq!(resolve(&st, NOW, &ev(Some(true), Some(NOW - 5_000), false)), Some(SessionState::Working));
        // 훅과 같은 순간의 transcript 쓰기는 중단 확인을 부르지 않는다.
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let c = calls.clone();
        let near = Evidence {
            alive: Some(true),
            transcript_ms: Some(st.at_ms + 500),
            interrupted: Box::new(move || {
                c.set(c.get() + 1);
                true
            }),
        };
        assert_eq!(resolve(&st, NOW, &near), Some(SessionState::Working));
        assert_eq!(calls.get(), 0);
        let waiting = hook("waiting", NOW - 20_000);
        assert_eq!(resolve(&waiting, NOW, &ev(Some(true), Some(NOW - 5_000), true)), Some(SessionState::Idle));
    }

    #[test]
    fn waiting_outlives_the_stale_cut_and_beats_finished_sessions() {
        let waiting = hook("waiting", NOW - SIX_MIN);
        assert_eq!(resolve(&waiting, NOW, &ev(Some(true), None, false)), Some(SessionState::Waiting));
        let states = vec![hook("waiting", NOW - 60_000), hook("done", NOW - 1_000)];
        let a = merge_sessions("Claude Code", &sessions_of(&states, || ev(None, None, false))).unwrap();
        assert_eq!(a.state, ActivityState::Waiting);
        // 도는 세션이 있으면 그쪽이 먼저다.
        let states = vec![hook("waiting", NOW - 1_000), hook("working", NOW - 60_000)];
        let a = merge_sessions("Claude Code", &sessions_of(&states, || ev(None, None, false))).unwrap();
        assert_eq!(a.state, ActivityState::Working);
    }

    #[test]
    fn newest_finished_hook_decides_state() {
        let states = vec![hook("idle", NOW - 60_000), hook("done", NOW - 2_000)];
        let merged = merge_sessions("Codex", &sessions_of(&states, || ev(None, None, false)));
        assert_eq!(merged.unwrap().state, ActivityState::JustDone);
        let states = vec![hook("interrupted", NOW - 2_000)];
        let merged = merge_sessions("Codex", &sessions_of(&states, || ev(None, None, false)));
        assert_eq!(merged.unwrap().state, ActivityState::Idle);
        // 살아 있지만 오래전에 끝난 세션은 막 끝남이 아니라 쉬는 중이다.
        let old = hook("done", NOW - SIX_MIN);
        assert_eq!(resolve(&old, NOW, &ev(Some(true), None, false)), Some(SessionState::Idle));
    }

    #[test]
    fn stop_failure_is_kept_as_failed() {
        let mut st = hook("done", NOW - 1_000);
        st.failed = true;
        let a = merge_sessions("Claude Code", &sessions_of(&[st], || ev(None, None, false))).unwrap();
        assert_eq!(a.state, ActivityState::JustDone);
        assert!(a.failed);
    }

    #[test]
    fn session_row_carries_project_and_landing() {
        let mut st = hook("working", NOW - 1_000);
        st.target = Some(serde_json::json!({
            "kind": "orca", "worktreeId": "r::/w/feature-x", "worktreePath": "/w/feature-x", "tabId": "t1"
        }));
        st.label = Some("feature-x".into());
        let row = to_session("Claude Code", &st, SessionState::Working, NOW);
        assert_eq!(row.project.as_deref(), Some("feature-x"));
        assert!(matches!(row.target, crate::inbox::Target::Orca { ref tab_id, .. } if tab_id.as_deref() == Some("t1")));
        // 라벨이 없으면 cwd 마지막 마디.
        let row = to_session("Claude Code", &hook("idle", NOW), SessionState::Idle, NOW);
        assert_eq!(row.project.as_deref(), Some("my-project"));
        assert!(matches!(row.target, crate::inbox::Target::None));
    }

    #[test]
    fn hook_state_files_are_read_and_temp_files_skipped() {
        let dir = std::env::temp_dir().join(format!("trw-hook-states-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("codex-s1.json"),
            format!("{{\"tool\":\"codex\",\"state\":\"working\",\"at_ms\":{NOW},\"session\":\"s1\",\"home\":\"/h\"}}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("claude-c1.json"),
            format!("{{\"tool\":\"claude\",\"state\":\"waiting\",\"at_ms\":{NOW},\"session\":\"c1\",\"pid\":42,\"pid_start\":7}}\n"),
        )
        .unwrap();
        std::fs::write(dir.join(".codex-s2.json.1.tmp"), "{").unwrap();
        std::fs::write(dir.join("codex-broken.json"), "{").unwrap();
        let codex = read_hook_states(&dir, "codex");
        let claude = read_hook_states(&dir, "claude");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(codex.len(), 1);
        assert_eq!((codex[0].state.as_str(), codex[0].at_ms), ("working", NOW));
        assert_eq!(claude.len(), 1);
        assert_eq!((claude[0].pid, claude[0].pid_start), (Some(42), Some(7)));
    }

    #[test]
    fn interrupt_marker_is_read_from_the_last_turn_entry() {
        let user = |content: &str| format!(r#"{{"type":"user","message":{{"content":{content}}}}}"#);
        let lines = vec![
            r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#.to_string(),
            user(r#"[{"type":"text","text":"[Request interrupted by user for tool use]"}]"#),
            r#"{"type":"system","subtype":"x"}"#.to_string(),
        ];
        assert!(last_entry_is_interrupt(&lines));
        assert!(last_entry_is_interrupt(&[user(r#""[Request interrupted by user]""#)]));
        // 도구 결과 안에 같은 문구가 있어도(transcript를 grep한 결과 등) 중단이 아니다.
        let tool_result = user(r#"[{"type":"tool_result","content":"[Request interrupted by user]"}]"#);
        assert!(!last_entry_is_interrupt(&[tool_result]));
        let resumed = vec![
            user(r#""[Request interrupted by user]""#),
            r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#.to_string(),
        ];
        assert!(!last_entry_is_interrupt(&resumed));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn own_process_is_alive_and_wrong_start_is_not() {
        let me = std::process::id();
        let start = proc_info::start_sec(me).expect("자기 프로세스 시작 시각");
        assert!(proc_info::alive(me, Some(start)));
        assert!(!proc_info::alive(me, Some(start - 1)), "pid 재사용");
        assert!(!proc_info::alive(u32::MAX / 2, None));
    }

    #[test]
    fn claude_tool_use_is_working() {
        let lines = vec![
            r#"{"type":"assistant","timestamp":"2026-08-20T12:18:40.970Z","message":{"stop_reason":"tool_use"}}"#.to_string(),
            r#"{"type":"attachment","timestamp":"2026-08-20T12:18:41.202Z"}"#.to_string(),
        ];
        let a = detect_claude(&lines, NOW, NOW).unwrap();
        assert_eq!(a.state, ActivityState::Working);
    }

    #[test]
    fn claude_end_turn_just_done_then_idle() {
        let ts = "2026-08-20T12:18:40.970Z";
        let done_ms = rfc3339_ms(ts).unwrap();
        let lines = vec![format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"stop_reason":"end_turn"}}}}"#
        )];
        assert_eq!(
            detect_claude(&lines, done_ms + 1_000, done_ms)
                .unwrap()
                .state,
            ActivityState::JustDone
        );
        assert_eq!(
            detect_claude(&lines, done_ms + 60_000, done_ms)
                .unwrap()
                .state,
            ActivityState::Idle
        );
    }

    #[test]
    fn codex_reads_task_events() {
        let started = r#"{"type":"event_msg","timestamp":"2026-08-19T04:17:38.420Z","payload":{"type":"task_started"}}"#;
        let noise = r#"{"type":"event_msg","timestamp":"2026-08-19T04:17:40.478Z","payload":{"type":"token_count"}}"#;
        let lines = vec![started.to_string(), noise.to_string()];
        assert_eq!(
            detect_codex(&lines, NOW, NOW).unwrap().state,
            ActivityState::Working
        );
    }

    #[test]
    fn grok_ignores_post_turn_noise() {
        // turn_completed 뒤에 붙는 session_recap·hook_execution이 판정을 흐리지 않아야 한다.
        let done_ms = NOW - 1_000;
        let lines = vec![
            format!(
                r#"{{"params":{{"update":{{"sessionUpdate":"turn_completed"}},"_meta":{{"agentTimestampMs":{done_ms}}}}}}}"#
            ),
            r#"{"params":{"update":{"sessionUpdate":"session_recap"}}}"#.to_string(),
            r#"{"params":{"update":{"sessionUpdate":"hook_execution"}}}"#.to_string(),
        ];
        assert_eq!(
            detect_grok(&lines, NOW, NOW).unwrap().state,
            ActivityState::JustDone
        );
    }

    #[test]
    fn gemini_error_marks_failed() {
        let lines = vec![
            r#"{"type":"user","timestamp":"2026-06-21T02:31:55.585Z"}"#.to_string(),
            r#"{"type":"error","timestamp":"2026-06-21T02:31:57.088Z"}"#.to_string(),
            r#"{"$set":{"lastUpdated":"2026-06-21T02:31:57.089Z"}}"#.to_string(),
        ];
        let a = detect_gemini(&lines, NOW, NOW).unwrap();
        assert!(a.failed);
    }

    #[test]
    fn antigravity_heartbeat_alone_is_idle() {
        // 요청 줄 뒤로 하트비트만 30초 넘게 쌓이면 쉬는 중이어야 한다.
        let lines = vec![
            "I0621 12:15:02.026991 69385 http_helpers.go:198] URL: .../v1internal:streamGenerateContent?alt=sse".to_string(),
            "I0621 12:15:50.788164 69385 quota_manager.go:68] quotaRefreshLoop: skipped".to_string(),
        ];
        assert_eq!(
            detect_antigravity(&lines, NOW, NOW).unwrap().state,
            ActivityState::Idle
        );
    }

    #[test]
    fn antigravity_fresh_request_is_working() {
        let lines = vec![
            "I0621 12:15:02.026991 69385 http_helpers.go:198] URL: .../v1internal:streamGenerateContent?alt=sse".to_string(),
            "I0621 12:15:05.788164 69385 quota_manager.go:68] quotaRefreshLoop: skipped".to_string(),
        ];
        assert_eq!(
            detect_antigravity(&lines, NOW, NOW).unwrap().state,
            ActivityState::Working
        );
    }

    #[test]
    fn antigravity_shutdown_is_idle() {
        let lines = vec![
            "I0622 17:55:02.026991 69385 http_helpers.go:198] URL: .../v1internal:streamGenerateContent?alt=sse".to_string(),
            "I0622 17:56:07.565908 79551 common.go:508] Terminal gone, shutting down".to_string(),
        ];
        assert_eq!(
            detect_antigravity(&lines, NOW, NOW).unwrap().state,
            ActivityState::Idle
        );
    }

    /// 이 머신의 실제 로그로 판정 결과를 눈으로 확인한다. 도구 설치 여부에 결과가
    /// 달라 CI에서는 돌리지 않는다 — `cargo test -- --ignored --nocapture real_home`.
    #[test]
    #[ignore]
    fn real_home() {
        for a in detect_all() {
            println!("{:<12} {:?} failed={}", a.tool, a.state, a.failed);
        }
    }

    #[test]
    fn glog_head_parses() {
        assert_eq!(
            glog_tod_ms("I0622 17:55:06.968994 79551 manager.go:1300] x"),
            Some(((17 * 60 + 55) * 60 + 6) * 1000 + 968)
        );
        assert_eq!(glog_tod_ms("not a glog line"), None);
    }
}
