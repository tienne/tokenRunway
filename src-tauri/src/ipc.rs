//! `trw` CLI와 앱 사이의 통로 — Unix 도메인 소켓 + 스풀 폴백.
//!
//! 소켓: `~/.token-runway/trw.sock` (0600). 연결 하나가 요청 하나를 보내고
//! `{"ok":true}` 한 줄을 받고 끊는다.
//!
//! 앱이 꺼져 있을 때 온 알림은 `~/.token-runway/pending/`에 파일로 남는다.
//! `trw`는 에이전트 훅 안에서 도니까 앱 상태 때문에 실패해서는 안 된다.
//!
//! 한 줄의 `kind`로 메시지 종류를 가른다. 없거나 `notify`면 완료 알림, `statusline`이면
//! 잔여율 릴레이다. `kind`가 없는 예전 `trw notify` 페이로드도 그대로 알림으로 받는다.

use crate::inbox::NotifyRequest;
use crate::providers::claude_relay::RelayMessage;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// 스풀을 다시 훑는 주기. 앱이 켜져 있는 동안 `trw`는 소켓으로 바로 오므로
/// 이 경로는 앱이 꺼진 사이 쌓인 것만 걷어낸다.
const SPOOL_SCAN_SECS: u64 = 30;

pub fn socket_path() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("trw.sock"))
}

/// 다른 인스턴스가 이미 소켓을 잡고 있는지. 아직 bind하기 전에 부르므로 붙는 상대는
/// 우리 자신일 수 없다.
pub fn another_instance_alive() -> bool {
    socket_path().is_some_and(|p| UnixStream::connect(p).is_ok())
}

/// 소켓으로 들어오는 메시지.
#[derive(Debug)]
pub enum IpcMessage {
    Notify(NotifyRequest),
    Statusline(RelayMessage),
}

/// 한 줄을 종류에 맞춰 파싱한다.
fn parse_message(line: &str) -> Result<IpcMessage, String> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    match v.get("kind").and_then(|k| k.as_str()) {
        None | Some("notify") => serde_json::from_value(v)
            .map(IpcMessage::Notify)
            .map_err(|e| e.to_string()),
        Some("statusline") => serde_json::from_value(v)
            .map(IpcMessage::Statusline)
            .map_err(|e| e.to_string()),
        Some(other) => Err(format!("unknown kind: {other}")),
    }
}

pub fn spool_dir() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("pending"))
}

/// 한 줄을 읽고 바로 응답한다. 처리는 받은 쪽(`tx`)에서 따로 한다 — 처리를 다 끝내고
/// 응답하면 앱이 잠깐 느려진 사이 `trw`가 응답 대기 시간을 넘겨 앱이 꺼진 줄 알고
/// 앱을 하나 더 띄운다.
fn handle_conn(stream: UnixStream, tx: &mpsc::Sender<IpcMessage>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let mut out = stream;
    match parse_message(line.trim()) {
        Ok(msg) => {
            let _ = out.write_all(b"{\"ok\":true}\n");
            let _ = tx.send(msg);
        }
        Err(e) => {
            let msg = serde_json::json!({ "ok": false, "error": e });
            let _ = out.write_all(format!("{msg}\n").as_bytes());
        }
    }
}

/// 스풀에 쌓인 요청을 모두 소비한다. 읽은 파일은 지운다 — 파싱에 실패한 것도
/// 지운다(고칠 방법이 없는데 남기면 매 스캔마다 같은 걸 다시 읽는다).
/// 릴레이는 스풀에 남지 않으니 여기서는 완료 알림만 나온다.
fn drain_spool<F>(on_message: &F)
where
    F: Fn(IpcMessage),
{
    let Some(dir) = spool_dir() else { return };
    let Ok(entries) = fs::read_dir(&dir) else { return };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    // Stop 훅 줄은 세션마다 같은 파일을 덮어써서 파일명으로는 순서를 알 수 없다.
    // 보낸 시각으로 줄 세워야 같은 세션의 명시 알림과 훅 줄이 알림함에서 맞게 합쳐진다.
    let mut reqs: Vec<(i64, PathBuf, NotifyRequest)> = Vec::new();
    files.sort();
    for path in files {
        if let Some(req) = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<NotifyRequest>(&raw).ok())
        {
            reqs.push((req.sent_ms.unwrap_or(0), path.clone(), req));
        }
        let _ = fs::remove_file(&path);
    }
    reqs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, _, req) in reqs {
        on_message(IpcMessage::Notify(req));
    }
}

/// 소켓 리스너와 스풀 스캐너를 띄운다. 앱 시작 시 한 번 부른다.
pub fn start<F>(on_message: F)
where
    F: Fn(IpcMessage) + Send + Sync + 'static,
{
    let cb = Arc::new(on_message);

    // 앱이 꺼진 사이 쌓인 것부터 걷어낸다.
    {
        let cb = Arc::clone(&cb);
        std::thread::spawn(move || {
            loop {
                drain_spool(&*cb);
                std::thread::sleep(Duration::from_secs(SPOOL_SCAN_SECS));
            }
        });
    }

    let Some(path) = socket_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // 이전 실행이 남긴 소켓 파일은 bind를 막아서 치운다. 살아 있는 인스턴스가 있으면
    // `run()`이 이미 여기까지 오지 않고 빠졌다.
    let _ = fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("trw 소켓 bind 실패: {e}");
            return;
        }
    };
    // 같은 사용자만 붙을 수 있게. 로컬 소켓이지만 다른 계정에 열어둘 이유가 없다.
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));

    // 처리는 스레드 하나가 들어온 순서대로 한다. 같은 세션 줄을 합치는 판정이 순서에 기댄다.
    let (tx, rx) = mpsc::channel::<IpcMessage>();
    std::thread::spawn(move || {
        for msg in rx {
            cb(msg);
        }
    });
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(s) = stream else { continue };
            // 붙기만 하고 안 쓰는 클라이언트가 읽기 대기 5초 동안 다음 연결을 막지 않게.
            let tx = tx.clone();
            std::thread::spawn(move || handle_conn(s, &tx));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_payload_without_kind_still_parses() {
        // 지금 배포된 trw notify가 보내는 모양 그대로다.
        let line = r#"{"title":"리팩터링 끝","body":"테스트 12개 통과","level":"done","context":"wt","target":{"kind":"orca","worktreeId":"r::/p","worktreePath":"/p","tabId":null},"key":"claude:s1","sentMs":1}"#;
        match parse_message(line).unwrap() {
            IpcMessage::Notify(req) => {
                assert_eq!(req.title, "리팩터링 끝");
                assert_eq!(req.key.as_deref(), Some("claude:s1"));
            }
            other => panic!("알림으로 파싱돼야 한다: {other:?}"),
        }
    }

    #[test]
    fn explicit_notify_kind_parses_as_notify() {
        let line = r#"{"kind":"notify","title":"t","quiet":true}"#;
        assert!(matches!(parse_message(line), Ok(IpcMessage::Notify(_))));
    }

    #[test]
    fn statusline_kind_parses_as_relay() {
        let line = r#"{"kind":"statusline","configDir":"/Users/me/.claude","rateLimits":{"fiveHour":{"usedPercentage":42,"resetsAt":1786000000},"sevenDay":null},"sentMs":5}"#;
        match parse_message(line).unwrap() {
            IpcMessage::Statusline(r) => {
                assert_eq!(r.config_dir, "/Users/me/.claude");
                assert_eq!(r.rate_limits.five_hour.unwrap().used_percentage, 42.0);
                assert!(r.rate_limits.seven_day.is_none());
            }
            other => panic!("릴레이로 파싱돼야 한다: {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_is_rejected() {
        assert!(parse_message(r#"{"kind":"exec","title":"t"}"#).is_err());
    }
}
