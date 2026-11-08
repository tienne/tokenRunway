//! `trw`는 에이전트 훅 안에서 돌아서 어떤 입력에도 0으로 끝나야 한다. `--strict`만 예외다.
//!
//! 매번 임시 디렉토리를 HOME으로 잡는다. 실제 `~/.token-runway`의 스풀과 소켓을 건드리지 않고,
//! `wakeOnNotify`를 꺼서 테스트 중에 앱이 뜨지 않게 한다.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempHome(PathBuf);

impl TempHome {
    /// `with_app_dir`가 false면 `.token-runway`도 없는 HOME — 소켓도 스풀 디렉토리도 없다.
    fn new(with_app_dir: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "trw-exit-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if with_app_dir {
            let app = dir.join(".token-runway");
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(app.join("settings.json"), r#"{"wakeOnNotify":false}"#).unwrap();
        }
        Self(dir)
    }

    fn app_dir(&self) -> PathBuf {
        self.0.join(".token-runway")
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(home: &Path, args: &[&str], stdin: Vec<u8>) -> (i32, Vec<u8>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trw"))
        .args(args)
        .env("HOME", home)
        .env_remove("ORCA_WORKTREE_ID")
        .env_remove("ORCA_TAB_ID")
        .env_remove("ORCA_AGENT_SESSION_ID")
        .env_remove("ORCA_CLI_COMMAND")
        .env_remove("PASEO_AGENT_ID")
        .env_remove("PASEO_BRANCH_NAME")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("trw 실행");
    // 입력을 안 읽는 서브커맨드는 파이프가 차면 쓰기가 막히므로 따로 쓰고 에러는 무시한다.
    let mut pipe = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = pipe.write_all(&stdin);
    });
    let out = child.wait_with_output().expect("trw 대기");
    let _ = writer.join();
    (out.status.code().unwrap_or(-1), out.stdout)
}

const SUBCOMMANDS: &[&[&str]] = &[
    &["notify", "테스트 알림"],
    &["hook", "stop"],
    &["hook", "codex", "Stop", "--home", "/tmp/codex-home"],
    &["statusline"],
];

fn inputs() -> Vec<(&'static str, Vec<u8>)> {
    let mut big = b"{\"rate_limits\":{\"five_hour\":{\"used_percentage\":10},\"pad\":\"".to_vec();
    big.extend(vec![b'x'; 1_500_000]);
    big.extend(b"\"}}");
    vec![
        ("빈 stdin", Vec::new()),
        ("깨진 JSON", b"{\"session_id\": \"abc\", \"rate_limits\": {".to_vec()),
        ("1MB 초과", big),
    ]
}

#[test]
fn every_subcommand_exits_zero_on_bad_input() {
    for args in SUBCOMMANDS {
        for (label, input) in inputs() {
            let home = TempHome::new(true);
            let (code, _) = run(&home.0, args, input);
            assert_eq!(code, 0, "{args:?} / {label}");
        }
    }
}

#[test]
fn every_subcommand_exits_zero_without_socket() {
    for args in SUBCOMMANDS {
        let home = TempHome::new(false);
        let (code, _) = run(&home.0, args, b"{}".to_vec());
        assert_eq!(code, 0, "{args:?} / 소켓 없음");
    }
}

#[test]
fn only_strict_reports_failure() {
    // 제목이 없으면 strict만 1이다.
    let home = TempHome::new(true);
    assert_eq!(run(&home.0, &["notify"], Vec::new()).0, 0);
    assert_eq!(run(&home.0, &["notify", "--strict"], Vec::new()).0, 1);

    // 소켓도 없고 스풀 자리에 파일이 있어 남길 수도 없을 때.
    let home = TempHome::new(true);
    std::fs::write(home.app_dir().join("pending"), "막힘").unwrap();
    assert_eq!(run(&home.0, &["notify", "제목"], Vec::new()).0, 0);
    assert_eq!(run(&home.0, &["notify", "제목", "--strict"], Vec::new()).0, 1);
}

#[test]
fn statusline_relay_is_never_spooled() {
    let home = TempHome::new(true);
    let input = br#"{"rate_limits":{"five_hour":{"used_percentage":30,"resets_at":1786000000}}}"#;
    let (code, stdout) = run(&home.0, &["statusline"], input.to_vec());
    assert_eq!(code, 0);
    // 원래 statusLine 명령이 없으면 아무것도 찍지 않는다.
    assert!(stdout.is_empty());
    assert!(!home.app_dir().join("pending").exists());
}

#[test]
fn statusline_runs_the_original_command() {
    let home = TempHome::new(true);
    let config_dir = home.0.join(".claude");
    let originals = serde_json::json!({
        "originals": { config_dir.to_string_lossy(): { "command": "cat; printf ' |done'" } }
    });
    std::fs::write(home.app_dir().join("statusline.json"), originals.to_string()).unwrap();
    let (code, stdout) = run(&home.0, &["statusline"], b"{\"x\":1}".to_vec());
    assert_eq!(code, 0);
    assert_eq!(stdout, b"{\"x\":1} |done");
}

#[test]
fn hook_codex_writes_one_state_line() {
    let home = TempHome::new(true);
    let (code, _) = run(
        &home.0,
        &["hook", "codex", "UserPromptSubmit", "--home", "/tmp/codex-a"],
        br#"{"session_id":"019a-test","prompt":"hi"}"#.to_vec(),
    );
    assert_eq!(code, 0);
    let path = home.app_dir().join("hooks").join("codex-019a-test.json");
    let raw = std::fs::read_to_string(path).unwrap();
    assert_eq!(raw.lines().count(), 1);
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(v["tool"], "codex");
    assert_eq!(v["state"], "working");
    assert_eq!(v["session"], "019a-test");
    assert_eq!(v["home"], "/tmp/codex-a");
    assert!(v["at_ms"].as_i64().unwrap() > 0);
    assert!(v.get("prompt").is_none());
}
