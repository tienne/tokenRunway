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
//! - Claude Code: 마지막 엔트리 종류로 추론 (`assistant`+`tool_use`면 도는 중)
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
const STALE_MS: i64 = 5 * 60 * 1000;
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
    /// 실패로 끝났는지. 현재 Gemini의 `error` 엔트리에서만 판별된다.
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

/// `root` 아래에서 `matches`를 만족하는 파일 중 mtime이 가장 최근인 것.
fn newest_file(root: &Path, matches: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, i64)> = None;
    walk(root, matches, 0, &mut best);
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
    DateTime::parse_from_rfc3339(raw).ok().map(|d| d.timestamp_millis())
}

// --- 도구별 판정 ---

/// 판정 대상 도구 목록 — 표시명, 탐색 루트, 파일명 조건, 꼬리 해석 함수.
/// 순서가 `SCAN_CACHE`의 인덱스라서 바꾸면 캐시 의미도 바뀐다(추가는 뒤에).
type Detector = fn(&[String], i64, i64) -> Option<AgentActivity>;
/// `(표시명, 탐색 루트, 파일명 조건, 해석 함수)`.
type Target = (&'static str, Option<PathBuf>, fn(&str) -> bool, Detector);

fn targets() -> Vec<Target> {
    let home = dirs::home_dir();
    let grok_root = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".grok")));
    vec![
        (
            "Claude Code",
            home.as_ref().map(|h| h.join(".claude").join("projects")),
            |n: &str| n.ends_with(".jsonl"),
            detect_claude as Detector,
        ),
        (
            "Codex",
            home.as_ref().map(|h| h.join(".codex").join("sessions")),
            |n: &str| n.starts_with("rollout-") && n.ends_with(".jsonl"),
            detect_codex as Detector,
        ),
        (
            "Grok",
            grok_root.map(|r| r.join("sessions")),
            |n: &str| n == "updates.jsonl",
            detect_grok as Detector,
        ),
        (
            "Gemini",
            home.as_ref().map(|h| h.join(".gemini").join("tmp")),
            |n: &str| n.starts_with("session-") && n.ends_with(".jsonl"),
            detect_gemini as Detector,
        ),
        (
            "Antigravity",
            home.as_ref()
                .map(|h| h.join(".gemini").join("antigravity-cli").join("log")),
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
                return Some(AgentActivity::new("Claude Code", ActivityState::Working, ts));
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
            return Some(AgentActivity::new("Antigravity", ActivityState::Idle, mtime_ms));
        }
        if !line.contains("streamGenerateContent")
            && !line.contains("Sending user message to conversation")
        {
            continue;
        }
        let Some(tod) = glog_tod_ms(line) else { continue };
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
                        root.as_deref().and_then(|r| newest_file(r, matches))
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
            detect_claude(&lines, done_ms + 1_000, done_ms).unwrap().state,
            ActivityState::JustDone
        );
        assert_eq!(
            detect_claude(&lines, done_ms + 60_000, done_ms).unwrap().state,
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
            format!(r#"{{"params":{{"update":{{"sessionUpdate":"turn_completed"}},"_meta":{{"agentTimestampMs":{done_ms}}}}}}}"#),
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

