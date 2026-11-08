//! `trw` — 아무 세션에서나 Token Runway로 알림을 보내는 CLI.
//!
//! 에이전트 훅이나 스크립트 안에서 도니까 **절대 실패로 종료하지 않는다**.
//! 앱이 꺼져 있으면 스풀에 남겨두고 0으로 빠진다(`--strict`로만 실패를 알린다).
//!
//! 앱 코드(`token_runway_lib`)를 참조하지 않는다 — 참조하면 tauri 전체가 이 작은
//! 바이너리에 딸려 들어온다. 주고받는 건 JSON 한 줄이라 타입을 공유할 필요가 없다.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "\
trw — Token Runway 알림 보내기

사용법:
  trw notify <제목> [옵션]
  trw ls [--json]
  trw where
  trw hook stop        Claude Code Stop 훅용 — stdin의 훅 입력을 읽어 알림함에 조용히 남긴다
  trw hook codex <이벤트> --home <CODEX_HOME>
                       Codex 훅용 — SessionStart | UserPromptSubmit | Stop | Interrupt를
                       활동 상태 파일로 남긴다
  trw statusline       Claude Code statusLine용 — rate_limits만 앱에 넘기고 원래
                       statusLine 명령의 출력을 그대로 찍는다

옵션 (notify):
  --body <문구>        알림 본문
  --level <레벨>       done | blocked | failed  (기본 done)
  --context <라벨>     리스트에 뜰 출처. 생략하면 워크트리 이름을 쓴다
  --no-target          랜딩 정보를 붙이지 않는다 (눌러도 이동하지 않음)
  --strict             전달 실패 시 종료 코드 1

예:
  trw notify \"리팩터링 끝\" --body \"테스트 12개 통과\"
  trw notify \"권한 대기 중\" --level blocked
";

fn app_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".token-runway"))
}

/// 지금 이 세션이 어디서 도는지 알아내 랜딩 정보를 만든다.
///
/// Orca는 `ORCA_WORKTREE_ID`/`ORCA_TAB_ID`를 환경변수로 넣어준다. Paseo는
/// `PASEO_AGENT_ID`가 없을 수도 있어서 cwd로 역매칭까지 시도한다.
fn detect_target() -> (serde_json::Value, Option<String>) {
    if let Ok(worktree_id) = std::env::var("ORCA_WORKTREE_ID") {
        return orca_target(worktree_id, std::env::var("ORCA_TAB_ID").ok());
    }
    if std::env::var_os("ORCA_AGENT_SESSION_ID").is_some() {
        if let Some(worktree_id) = orca_current_worktree() {
            return orca_target(worktree_id, None);
        }
    }

    let cwd = std::env::current_dir().ok();
    if let Ok(agent_id) = std::env::var("PASEO_AGENT_ID") {
        let label = std::env::var("PASEO_BRANCH_NAME")
            .ok()
            .or_else(|| cwd.as_deref().and_then(|p| last_segment(&p.to_string_lossy())));
        return (
            serde_json::json!({
                "kind": "paseo",
                "agentId": agent_id,
                "cwd": cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
            }),
            label,
        );
    }

    if let Some(cwd) = cwd.as_deref() {
        if let Some((agent_id, title)) = paseo_agent_for_cwd(cwd) {
            return (
                serde_json::json!({
                    "kind": "paseo",
                    "agentId": agent_id,
                    "cwd": cwd.to_string_lossy(),
                }),
                title.or_else(|| last_segment(&cwd.to_string_lossy())),
            );
        }
        return (
            serde_json::json!({ "kind": "path", "path": cwd.to_string_lossy() }),
            last_segment(&cwd.to_string_lossy()),
        );
    }

    (serde_json::json!({ "kind": "none" }), None)
}

fn orca_target(worktree_id: String, tab_id: Option<String>) -> (serde_json::Value, Option<String>) {
    // `<repo-uuid>::<절대경로>` 꼴이라 뒤쪽이 워크트리 경로다.
    let path = worktree_id.split("::").nth(1).map(|s| s.to_string());
    let label = path.as_deref().and_then(last_segment);
    (
        serde_json::json!({
            "kind": "orca",
            "worktreeId": worktree_id,
            "worktreePath": path,
            "tabId": tab_id,
        }),
        label,
    )
}

/// 오르카 채팅 세션에는 `ORCA_WORKTREE_ID`가 안 들어와서 CLI로 cwd의 워크트리를 묻는다.
/// 채팅 세션 하나를 앞으로 띄우는 CLI 명령이나 딥링크는 없어(오르카 1.4.243 확인) 워크트리까지만 데려간다.
/// 훅 안에서 도는 trw가 런타임 응답을 기다리다 매달리면 안 돼서 시간을 끊는다.
fn orca_current_worktree() -> Option<String> {
    const TIMEOUT: Duration = Duration::from_secs(3);
    let bin = std::env::var_os("ORCA_CLI_COMMAND")?;
    let mut child = Command::new(bin)
        .args(["worktree", "current", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    v.get("result")?
        .get("worktree")?
        .get("id")?
        .as_str()
        .map(|s| s.to_string())
}

fn last_segment(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

/// `PASEO_AGENT_ID`가 안 들어오는 환경을 위한 폴백 — cwd가 같은 에이전트를 찾는다.
///
/// 같은 워크트리에 에이전트가 여러 개 있으면 가장 최근에 움직인 쪽을 고른다.
fn paseo_agent_for_cwd(cwd: &Path) -> Option<(String, Option<String>)> {
    let root = dirs::home_dir()?.join(".paseo/agents");
    let cwd_s = cwd.to_string_lossy().to_string();
    let mut best: Option<(String, Option<String>, String)> = None;
    for ws in std::fs::read_dir(&root).ok()? {
        let Ok(ws) = ws else { continue };
        let Ok(files) = std::fs::read_dir(ws.path()) else {
            continue;
        };
        for f in files.filter_map(|f| f.ok()) {
            let Ok(raw) = std::fs::read_to_string(f.path()) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if v.get("cwd").and_then(|s| s.as_str()) != Some(cwd_s.as_str()) {
                continue;
            }
            let id = v.get("id").and_then(|s| s.as_str())?.to_string();
            let title = v
                .get("title")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            let seen = v
                .get("lastActivityAt")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if best.as_ref().map(|(_, _, s)| seen > *s).unwrap_or(true) {
                best = Some((id, title, seen));
            }
        }
    }
    best.map(|(id, title, _)| (id, title))
}

enum SendError {
    /// 소켓에 붙지도 못했다 — 앱이 꺼져 있다.
    NotRunning(String),
    /// 붙어서 보냈지만 응답이 늦거나 이상했다. 앱은 떠 있다.
    AfterConnect(String),
}

/// 소켓으로 보낸다.
fn send_socket(payload: &serde_json::Value) -> Result<(), SendError> {
    let path = app_dir()
        .ok_or_else(|| SendError::NotRunning("홈 디렉토리를 찾을 수 없습니다".into()))?
        .join("trw.sock");
    let mut stream = UnixStream::connect(&path).map_err(|e| SendError::NotRunning(e.to_string()))?;
    let after = |e: String| SendError::AfterConnect(e);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    stream
        .write_all(format!("{payload}\n").as_bytes())
        .map_err(|e| after(e.to_string()))?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| after(e.to_string()))?;
    let resp: serde_json::Value = serde_json::from_str(line.trim()).map_err(|e| after(e.to_string()))?;
    if resp.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(after(
            resp.get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("알 수 없는 오류")
                .to_string(),
        ))
    }
}

/// 앱이 꺼져 있을 때 — 파일로 남긴다.
///
/// Stop 훅 줄은 세션마다 파일 하나를 덮어쓴다. 앱을 며칠 꺼두면 턴마다 파일이 쌓이는데
/// 알림함은 어차피 세션당 마지막 한 줄만 남긴다. 명시 알림은 하나하나 남긴다.
fn spool(payload: &serde_json::Value) -> Result<(), String> {
    let dir = app_dir().ok_or("홈 디렉토리를 찾을 수 없습니다")?.join("pending");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let quiet_key = payload
        .get("quiet")
        .and_then(|q| q.as_bool())
        .unwrap_or(false)
        .then(|| payload.get("key").and_then(|k| k.as_str()))
        .flatten();
    let name = match quiet_key {
        Some(key) => {
            let safe: String = key
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
                .collect();
            format!("quiet-{safe}.json")
        }
        None => format!("{:013}-{}.json", now_ms(), std::process::id()),
    };
    // 앱이 쓰는 중인 파일을 읽어 파싱에 실패하면 지워버리므로 다 쓴 뒤 이름을 바꾼다.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, payload.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(name)).map_err(|e| e.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const APP_BUNDLE_ID: &str = "co.tienne.token-runway";

/// 앱을 백그라운드로 띄운다. 앱은 뜨자마자 스풀을 읽어 배너와 진동까지 평소처럼 낸다.
/// 끝나길 기다리지 않는다 — 훅 안에서 돌고 있어서 세션을 붙잡으면 안 된다.
fn wake_app() {
    let enabled = app_dir()
        .and_then(|d| std::fs::read_to_string(d.join("settings.json")).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("wakeOnNotify").and_then(|b| b.as_bool()))
        .unwrap_or(true);
    if !enabled {
        return;
    }
    let _ = Command::new("/usr/bin/open")
        .args(["-g", "-b", APP_BUNDLE_ID])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn cmd_notify(args: &[String]) -> ExitCode {
    let mut title: Option<String> = None;
    let mut body: Option<String> = None;
    let mut level = "done".to_string();
    let mut context: Option<String> = None;
    let mut no_target = false;
    let mut strict = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--body" => {
                body = args.get(i + 1).cloned();
                i += 2;
            }
            "--level" => {
                if let Some(v) = args.get(i + 1) {
                    level = v.clone();
                }
                i += 2;
            }
            "--context" => {
                context = args.get(i + 1).cloned();
                i += 2;
            }
            "--no-target" => {
                no_target = true;
                i += 1;
            }
            "--strict" => {
                strict = true;
                i += 1;
            }
            other => {
                if title.is_none() && !other.starts_with("--") {
                    title = Some(other.to_string());
                }
                i += 1;
            }
        }
    }

    let Some(title) = title else {
        eprintln!("제목이 필요합니다.\n\n{USAGE}");
        return ExitCode::from(if strict { 1 } else { 0 });
    };
    if !matches!(level.as_str(), "done" | "blocked" | "failed") {
        eprintln!("--level 은 done | blocked | failed 중 하나여야 합니다");
        return ExitCode::from(if strict { 1 } else { 0 });
    }

    let (target, auto_label) = if no_target {
        (serde_json::json!({ "kind": "none" }), None)
    } else {
        detect_target()
    };

    let payload = serde_json::json!({
        "title": title,
        "body": body,
        "level": level,
        "context": context.or(auto_label),
        "target": target,
        "key": session_key(None),
        "sentMs": now_ms(),
    });
    deliver(&payload, strict)
}

fn deliver(payload: &serde_json::Value, strict: bool) -> ExitCode {
    match send_socket(payload) {
        Ok(()) => ExitCode::SUCCESS,
        // 앱은 떠 있는데 응답만 늦은 경우 — 이미 받았을 수 있어서 스풀에 또 남기면 두 번
        // 쌓이고, 깨우면 앱이 하나 더 뜬다.
        Err(SendError::AfterConnect(e)) => {
            if strict {
                eprintln!("앱이 응답하지 않았습니다: {e}");
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(SendError::NotRunning(sock_err)) => match spool(payload) {
            Ok(()) => {
                wake_app();
                // 앱이 꺼져 있는 건 정상 상황이다. 훅을 깨지 않게 조용히 넘긴다.
                if strict {
                    eprintln!("앱에 바로 못 보내 스풀에 남겼습니다: {sock_err}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("알림 전달 실패: {sock_err} / 스풀도 실패: {e}");
                ExitCode::from(if strict { 1 } else { 0 })
            }
        },
    }
}

/// 같은 세션에서 온 알림을 앱이 한 줄로 묶게 하는 키. 스킬이 부른 `trw notify`와
/// Stop 훅이 같은 키를 내야 둘이 겹쳐 두 줄로 쌓이지 않는다.
fn session_key(from_hook: Option<&str>) -> Option<String> {
    from_hook
        .map(|s| s.to_string())
        .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
        .filter(|s| !s.is_empty())
        .map(|s| format!("claude:{s}"))
}

/// Claude Code Stop 훅 — 턴이 끝날 때마다 그 세션을 알림함에 한 줄로 남긴다.
///
/// 에이전트가 `trw notify`를 부를지 판단하게 두면 평범한 턴은 전부 빠진다. 그래서 훅이
/// 턴마다 남기되 OS 알림과 진동은 없이(`quiet`) 세션당 한 줄로 덮어쓴다.
fn cmd_hook_stop() -> ExitCode {
    let raw = read_stdin_limited();
    let input: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let field = |k: &str| input.get(k).and_then(|v| v.as_str());

    // 훅은 세션 cwd에서 돌지만 보장된 건 아니라 입력의 cwd로 맞춘다 — 랜딩이 cwd를 본다.
    if let Some(cwd) = field("cwd") {
        let _ = std::env::set_current_dir(cwd);
    }
    let text = field("last_assistant_message")
        .map(|s| s.to_string())
        .or_else(|| field("transcript_path").and_then(|p| last_assistant_text(Path::new(p))));
    let title = text
        .as_deref()
        .and_then(headline)
        .unwrap_or_else(|| "턴 종료".to_string());
    let (target, label) = detect_target();
    let payload = serde_json::json!({
        "title": title,
        "level": "done",
        "context": label,
        "target": target,
        "key": session_key(field("session_id")),
        "quiet": true,
        "sentMs": now_ms(),
    });
    deliver(&payload, false);
    ExitCode::SUCCESS
}

/// 훅 입력 상한. 훅 입력은 몇 KB라 이걸 넘으면 뒤를 버리고, 잘린 JSON은 빈 입력처럼 다룬다.
const STDIN_LIMIT: u64 = 1 << 20;

fn read_stdin_limited() -> String {
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(STDIN_LIMIT).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// 훅 상태 파일 디렉토리. 앱 `activity.rs`가 같은 경로를 읽는다.
fn hooks_dir() -> Option<PathBuf> {
    app_dir().map(|d| d.join("hooks"))
}

/// 하루 넘게 안 바뀐 상태 파일. 세션마다 파일이 생겨서 지우지 않으면 계속 쌓인다.
const HOOK_STATE_KEEP: Duration = Duration::from_secs(24 * 3600);

/// Codex 훅 — 턴 경계를 rollout 추론 대신 이벤트로 남긴다.
///
/// 상태 파일: `~/.token-runway/hooks/codex-<세션>.json`, 내용은 JSON 한 줄
/// `{"tool":"codex","state":"working|done|idle|interrupted","at_ms":..,"session":..,"home":..}`.
/// 앱 `activity::read_hook_states`가 이 모양을 그대로 읽으니 바꿀 땐 둘을 같이 바꾼다.
///
/// 홈은 `--home` 인자로 받는다. 오르카가 계정마다 `CODEX_HOME`을 따로 잡는데, 훅
/// 프로세스 환경변수만 믿으면 어느 홈의 세션인지 못 가르는 경우가 있다.
fn cmd_hook_codex(args: &[String]) -> ExitCode {
    let Some(event) = args.first() else {
        return ExitCode::SUCCESS;
    };
    let state = match event.as_str() {
        "SessionStart" => "idle",
        "UserPromptSubmit" => "working",
        "Stop" => "done",
        "Interrupt" => "interrupted",
        _ => return ExitCode::SUCCESS,
    };
    let home = args
        .iter()
        .position(|a| a == "--home")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("CODEX_HOME").ok())
        .filter(|s| !s.is_empty());
    let raw = read_stdin_limited();
    let input: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let session = ["session_id", "sessionId"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let line = serde_json::json!({
        "tool": "codex",
        "state": state,
        "at_ms": now_ms(),
        "session": session,
        "home": home,
    });
    let _ = write_hook_state(session.as_deref(), &line);
    ExitCode::SUCCESS
}

fn write_hook_state(session: Option<&str>, line: &serde_json::Value) -> Result<(), String> {
    let dir = hooks_dir().ok_or("홈 디렉토리를 찾을 수 없습니다")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = session
        .unwrap_or("unknown")
        .chars()
        .take(128)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    let name = format!("codex-{safe}.json");
    // 앱 atomicfile과 같은 규칙. 앱이 반쯤 쓰인 파일을 읽으면 상태를 놓친다.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{line}\n")).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, dir.join(&name)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    prune_hook_states(&dir);
    Ok(())
}

fn prune_hook_states(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > HOOK_STATE_KEEP);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// 원래 statusLine 명령을 기다리는 상한. statusLine은 매 응답마다 불려서 오래 붙잡으면 화면이 굳는다.
const ORIGINAL_TIMEOUT: Duration = Duration::from_secs(10);

/// 앱으로 보낼 statusline 릴레이 메시지. 남은 비율 계산에 쓰는 `rate_limits`만 뽑는다 —
/// 같은 입력에 세션 내용, 프로젝트 경로, 모델명, 비용이 함께 오지만 앱은 그걸 받을 이유가 없다.
fn relay_payload(input: &serde_json::Value, config_dir: &str, sent_ms: i64) -> Option<serde_json::Value> {
    let limits = input.get("rate_limits")?;
    let window = |key: &str| -> Option<serde_json::Value> {
        let w = limits.get(key)?;
        // Claude Code는 0~200 범위로 준다. 한도 초과분은 잔여율 계산에서 의미가 없어 100으로 자른다.
        let used = w.get("used_percentage")?.as_f64()?.clamp(0.0, 100.0);
        let resets = w
            .get("resets_at")
            .filter(|v| v.is_number() || v.is_string())
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Some(serde_json::json!({ "usedPercentage": used, "resetsAt": resets }))
    };
    let five = window("five_hour");
    let seven = window("seven_day");
    if five.is_none() && seven.is_none() {
        return None;
    }
    Some(serde_json::json!({
        "kind": "statusline",
        "configDir": config_dir,
        "rateLimits": { "fiveHour": five, "sevenDay": seven },
        "sentMs": sent_ms,
    }))
}

/// 이 세션의 config dir. 앱이 같은 문자열로 Keychain 서비스 이름을 만들어 계정을 찾는다.
fn claude_config_dir() -> Option<String> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude").to_string_lossy().into_owned()))
        .map(|s| {
            let trimmed = s.trim_end_matches('/');
            if trimmed.is_empty() { s } else { trimmed.to_string() }
        })
}

/// 설치할 때 치워둔 원래 statusLine 명령. 앱 installer가 `statusline.json`에 config dir별로 남긴다.
fn original_statusline(config_dir: &str) -> Option<String> {
    let raw = std::fs::read_to_string(app_dir()?.join("statusline.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("originals")?
        .get(config_dir)?
        .get("command")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
}

/// 앱이 꺼져 있으면 버린다. 잔여율은 시간이 지나면 의미가 없어 스풀에 남기지 않고 앱을 깨우지도 않는다.
fn send_relay(payload: &serde_json::Value) {
    let Some(path) = app_dir().map(|d| d.join("trw.sock")) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(path) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = stream.write_all(format!("{payload}\n").as_bytes());
}

/// `sh -c`로 원래 명령을 돌려 stdout을 모은다. 상한을 넘기면 프로세스 그룹째 끊고 그때까지 모은 것만 돌려준다.
fn run_with_timeout(command: &str, input: &[u8], timeout: Duration) -> Vec<u8> {
    use std::os::unix::process::CommandExt;
    use std::sync::{mpsc, Arc, Mutex};
    let Ok(mut child) = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // 셸이 띄운 손자 프로세스까지 한 번에 끊으려고 그룹을 따로 잡는다.
        .process_group(0)
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let data = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let out = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    if let Some(mut stdout) = child.stdout.take() {
        let out = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut o) = out.lock() {
                    o.extend_from_slice(&buf[..n]);
                }
            }
            let _ = done_tx.send(());
        });
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    // 손자 프로세스가 stdout을 쥐고 있으면 읽기가 안 끝난다. 그때는 모은 데까지만 쓴다.
    let _ = done_rx.recv_timeout(Duration::from_millis(300));
    out.lock().map(|o| o.clone()).unwrap_or_default()
}

/// Claude Code statusLine — 잔여율을 OAuth 폴링보다 먼저 앱에 넘기는 보조 경로.
///
/// statusLine 자리를 차지하므로 원래 명령이 있으면 대신 돌려 그 출력을 그대로 찍는다.
/// 무엇이 실패해도 0으로 끝난다 — 여기서 실패하면 사용자 화면의 상태 줄이 빈다.
fn cmd_statusline() -> ExitCode {
    let mut raw = Vec::new();
    let _ = std::io::stdin().take(STDIN_LIMIT).read_to_end(&mut raw);
    let config_dir = claude_config_dir();
    if let Some(dir) = config_dir.as_deref() {
        let input: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
        if let Some(payload) = relay_payload(&input, dir, now_ms()) {
            send_relay(&payload);
        }
    }
    if let Some(command) = config_dir.as_deref().and_then(original_statusline) {
        let out = run_with_timeout(&command, &raw, ORIGINAL_TIMEOUT);
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&out);
        let _ = stdout.flush();
    }
    ExitCode::SUCCESS
}

/// 트랜스크립트 꼬리에서 마지막 assistant 응답의 텍스트를 꺼낸다. 세션이 길면 파일이
/// 수백 MB라 끝 2MB만 읽는다.
fn last_assistant_text(path: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    const TAIL: u64 = 2 * 1024 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let tail = String::from_utf8_lossy(&buf);
    tail.lines().rev().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("type")?.as_str()? != "assistant" {
            return None;
        }
        let text: Vec<&str> = v
            .get("message")?
            .get("content")?
            .as_array()?
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect();
        let joined = text.join("\n");
        (!joined.trim().is_empty()).then_some(joined)
    })
}

/// 응답 첫 줄을 알림 제목으로 다듬는다. 마크다운 머리 기호와 굵게 표시는 뗀다.
fn headline(text: &str) -> Option<String> {
    const MAX_CHARS: usize = 80;
    let line = text
        .lines()
        .map(|l| l.trim_start_matches(['#', '>', '-', '*', ' ', '\t']).replace("**", ""))
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())?;
    if line.chars().count() <= MAX_CHARS {
        return Some(line);
    }
    let cut: String = line.chars().take(MAX_CHARS - 1).collect();
    Some(format!("{cut}…"))
}

fn cmd_ls(args: &[String]) -> ExitCode {
    let as_json = args.iter().any(|a| a == "--json");
    let Some(path) = app_dir().map(|d| d.join("inbox.json")) else {
        return ExitCode::SUCCESS;
    };
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{\"items\":[]}".into());
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
    let empty = vec![];
    let items = v.get("items").and_then(|i| i.as_array()).unwrap_or(&empty);

    if as_json {
        println!("{}", serde_json::to_string_pretty(items).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    if items.is_empty() {
        println!("알림이 없습니다.");
        return ExitCode::SUCCESS;
    }
    for it in items.iter().rev() {
        let unread = if it.get("read").and_then(|b| b.as_bool()) == Some(true) {
            " "
        } else {
            "●"
        };
        let ctx = it.get("context").and_then(|s| s.as_str()).unwrap_or("-");
        let title = it.get("title").and_then(|s| s.as_str()).unwrap_or("");
        println!("{unread} {ctx:24} {title}");
    }
    ExitCode::SUCCESS
}

/// 지금 세션이 어디로 인식되는지 — 훅을 붙이기 전 확인용.
fn cmd_where() -> ExitCode {
    let (target, label) = detect_target();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "target": target,
            "context": label,
        }))
        .unwrap_or_default()
    );
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("notify") => cmd_notify(&args[1..]),
        Some("ls") => cmd_ls(&args[1..]),
        Some("where") => cmd_where(),
        Some("statusline") => cmd_statusline(),
        Some("hook") => match args.get(1).map(|s| s.as_str()) {
            Some("stop") => cmd_hook_stop(),
            Some("codex") => cmd_hook_codex(&args[2..]),
            // 훅 안에서 도는 명령이라 모르는 이벤트여도 세션을 깨지 않는다.
            _ => ExitCode::SUCCESS,
        },
        Some("--help") | Some("-h") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("알 수 없는 명령: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_payload_keeps_only_rate_limits() {
        let input = serde_json::json!({
            "session_id": "s-1",
            "cwd": "/Users/me/secret-project",
            "model": { "id": "claude-opus", "display_name": "Opus" },
            "cost": { "total_cost_usd": 3.2 },
            "workspace": { "project_dir": "/Users/me/secret-project" },
            "transcript_path": "/Users/me/.claude/projects/x.jsonl",
            "rate_limits": {
                "five_hour": { "used_percentage": 150.0, "resets_at": 1_786_000_000, "extra": 1 },
                "seven_day": { "used_percentage": 42.5, "resets_at": "2026-11-10T00:00:00Z" }
            }
        });
        let p = relay_payload(&input, "/Users/me/.claude", 7).unwrap();
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 4);
        for k in ["kind", "configDir", "rateLimits", "sentMs"] {
            assert!(p.get(k).is_some(), "{k} 빠짐");
        }
        assert_eq!(p["kind"], "statusline");
        assert_eq!(p["rateLimits"]["fiveHour"]["usedPercentage"], 100.0);
        assert_eq!(p["rateLimits"]["fiveHour"]["resetsAt"], 1_786_000_000);
        assert!(p["rateLimits"]["fiveHour"].get("extra").is_none());
        assert_eq!(p["rateLimits"]["sevenDay"]["usedPercentage"], 42.5);
        let text = p.to_string();
        for leaked in ["secret-project", "Opus", "claude-opus", "3.2", "s-1", "projects/x.jsonl"] {
            assert!(!text.contains(leaked), "{leaked}가 새어 나갔다");
        }
    }

    #[test]
    fn relay_payload_needs_rate_limits() {
        assert!(relay_payload(&serde_json::json!({ "model": {} }), "/d", 0).is_none());
        let empty = serde_json::json!({ "rate_limits": { "five_hour": {} } });
        assert!(relay_payload(&empty, "/d", 0).is_none());
    }

    #[test]
    fn original_command_gets_stdin_and_output_is_passed_through() {
        let out = run_with_timeout("cat; printf ' ok'", b"{\"a\":1}", Duration::from_secs(5));
        assert_eq!(out, b"{\"a\":1} ok");
    }

    #[test]
    fn slow_original_command_is_cut_off() {
        let started = Instant::now();
        let out = run_with_timeout("printf early; sleep 30; printf late", b"", Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(out, b"early");
    }
}
