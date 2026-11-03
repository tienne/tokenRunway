//! `trw` CLI와 앱 사이의 통로 — Unix 도메인 소켓 + 스풀 폴백.
//!
//! 소켓: `~/.token-runway/trw.sock` (0600). 연결 하나가 요청 하나를 보내고
//! `{"ok":true}` 한 줄을 받고 끊는다.
//!
//! 앱이 꺼져 있을 때 온 알림은 `~/.token-runway/pending/`에 파일로 남는다.
//! `trw`는 에이전트 훅 안에서 도니까 앱 상태 때문에 실패해서는 안 된다.

use crate::inbox::NotifyRequest;
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

pub fn spool_dir() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("pending"))
}

/// 한 줄을 읽고 바로 응답한다. 처리는 받은 쪽(`tx`)에서 따로 한다 — 처리를 다 끝내고
/// 응답하면 앱이 잠깐 느려진 사이 `trw`가 응답 대기 시간을 넘겨 앱이 꺼진 줄 알고
/// 앱을 하나 더 띄운다.
fn handle_conn(stream: UnixStream, tx: &mpsc::Sender<NotifyRequest>) {
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
    match serde_json::from_str::<NotifyRequest>(line.trim()) {
        Ok(req) => {
            let _ = out.write_all(b"{\"ok\":true}\n");
            let _ = tx.send(req);
        }
        Err(e) => {
            let msg = serde_json::json!({ "ok": false, "error": e.to_string() });
            let _ = out.write_all(format!("{msg}\n").as_bytes());
        }
    }
}

/// 스풀에 쌓인 요청을 모두 소비한다. 읽은 파일은 지운다 — 파싱에 실패한 것도
/// 지운다(고칠 방법이 없는데 남기면 매 스캔마다 같은 걸 다시 읽는다).
fn drain_spool<F>(on_notify: &F)
where
    F: Fn(NotifyRequest),
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
        on_notify(req);
    }
}

/// 소켓 리스너와 스풀 스캐너를 띄운다. 앱 시작 시 한 번 부른다.
pub fn start<F>(on_notify: F)
where
    F: Fn(NotifyRequest) + Send + Sync + 'static,
{
    let cb = Arc::new(on_notify);

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
    // 이전 실행이 남긴 소켓 파일은 bind를 막는다. 살아 있는 리스너가 있는지
    // 확인할 방법이 마땅치 않아(연결해봐도 우리 자신일 수 있다) 그냥 치운다 —
    // 앱은 한 번에 하나만 뜨는 트레이 앱이다.
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
    let (tx, rx) = mpsc::channel::<NotifyRequest>();
    std::thread::spawn(move || {
        for req in rx {
            cb(req);
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
