//! `trw` — 아무 세션에서나 Token Runway로 알림을 보내는 CLI.
//!
//! 에이전트 훅이나 스크립트 안에서 도니까 **절대 실패로 종료하지 않는다**.
//! 앱이 꺼져 있으면 스풀에 남겨두고 0으로 빠진다(`--strict`로만 실패를 알린다).
//!
//! 앱 코드(`token_runway_lib`)를 참조하지 않는다 — 참조하면 tauri 전체가 이 작은
//! 바이너리에 딸려 들어온다. 주고받는 건 JSON 한 줄이라 타입을 공유할 필요가 없다.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
trw — Token Runway 알림 보내기

사용법:
  trw notify <제목> [옵션]
  trw ls [--json]
  trw where

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
        // `<repo-uuid>::<절대경로>` 꼴이라 뒤쪽이 워크트리 경로다.
        let path = worktree_id.split("::").nth(1).map(|s| s.to_string());
        let label = path.as_deref().and_then(last_segment);
        let tab_id = std::env::var("ORCA_TAB_ID").ok();
        return (
            serde_json::json!({
                "kind": "orca",
                "worktreeId": worktree_id,
                "worktreePath": path,
                "tabId": tab_id,
            }),
            label,
        );
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

/// 소켓으로 보낸다. 앱이 안 떠 있으면 Err.
fn send_socket(payload: &serde_json::Value) -> Result<(), String> {
    let path = app_dir().ok_or("홈 디렉토리를 찾을 수 없습니다")?.join("trw.sock");
    let mut stream = UnixStream::connect(&path).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    stream
        .write_all(format!("{payload}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let resp: serde_json::Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    if resp.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(resp
            .get("error")
            .and_then(|s| s.as_str())
            .unwrap_or("알 수 없는 오류")
            .to_string())
    }
}

/// 앱이 꺼져 있을 때 — 파일로 남긴다. 파일명이 도착 순서를 담는다.
fn spool(payload: &serde_json::Value) -> Result<(), String> {
    let dir = app_dir().ok_or("홈 디렉토리를 찾을 수 없습니다")?.join("pending");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{ms:013}-{}.json", std::process::id());
    std::fs::write(dir.join(name), payload.to_string()).map_err(|e| e.to_string())
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
    });

    match send_socket(&payload) {
        Ok(()) => ExitCode::SUCCESS,
        Err(sock_err) => match spool(&payload) {
            Ok(()) => {
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
