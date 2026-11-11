//! 토큰이 만료된 Claude 계정을 CLI에게 갱신시키는 폴백.
//!
//! 안 쓰는 계정은 CLI가 토큰을 갱신하지 않아 몇 시간 뒤 사용량 조회가 401로 막힌다.
//! 우리가 refresh token으로 직접 갱신하면 CLI가 쥔 토큰과 어긋날 수 있어서, 그 계정의
//! config dir로 `claude -p /usage`를 한 번 돌린다. CLI의 사용량 조회 경로는 만료된
//! 토큰을 먼저 갱신하고(2.1.322 바이너리에서 확인) 그 결과를 Keychain에 다시 쓴다.
//! 우리는 출력을 읽지 않는다 — 갱신만 일으키고 다음 폴링에서 Keychain을 다시 읽는다.
//!
//! `/usage`는 로컬 명령이라 모델을 부르지 않는다(실행 결과 `num_turns` 0, 비용 0).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 실측 10초 안팎. 네트워크가 느릴 때를 감안해 두 배를 준다.
pub const CLI_TIMEOUT: Duration = Duration::from_secs(20);

/// 같은 계정을 다시 시도하기까지 기다리는 시간. 성공하면 토큰이 몇 시간은 가고,
/// 실패하면(refresh token까지 죽음) 몇 번을 돌려도 같은 결과라 길게 잡는다.
const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

/// 계정별 마지막 시도 시각.
static LAST_ATTEMPT: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 한 번에 하나만 돌린다. 계정이 여럿 만료돼도 CLI를 동시에 여러 개 띄우지 않는다.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// CLI를 띄울 환경. 접미사 없는 레거시 Keychain 항목은 `CLAUDE_CONFIG_DIR`이 없을 때만
/// 읽히므로 그 경우를 따로 표시한다 — `~/.claude`를 명시하면 해시가 붙은 이름을 읽는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliHome {
    Default,
    ConfigDir(PathBuf),
}

/// 만료된 계정 하나의 갱신을 백그라운드로 요청한다. 실제로 띄웠으면 true.
///
/// 부르는 쪽(`FETCH_GUARD`를 쥔 갱신 루프)을 기다리게 하지 않으려고 스레드로 뺀다.
/// 성공하면 `on_refreshed`를 부른다 — 계정 목록과 사용률 캐시를 비워 다음 폴링이
/// 새 토큰으로 다시 묻게 하는 자리다.
pub fn request(
    account_id: &str,
    homes: &[CliHome],
    enabled: bool,
    on_refreshed: impl FnOnce() + Send + 'static,
) -> bool {
    request_with(
        account_id,
        homes,
        enabled,
        Instant::now(),
        |homes| {
            let Some(bin) = find_claude_binary() else {
                return false;
            };
            homes.iter().any(|h| run_usage(&bin, h, CLI_TIMEOUT).is_ok())
        },
        on_refreshed,
    )
}

fn request_with(
    account_id: &str,
    homes: &[CliHome],
    enabled: bool,
    now: Instant,
    run: impl FnOnce(&[CliHome]) -> bool + Send + 'static,
    on_refreshed: impl FnOnce() + Send + 'static,
) -> bool {
    if !enabled || homes.is_empty() {
        return false;
    }
    {
        let Ok(mut last) = LAST_ATTEMPT.lock() else {
            return false;
        };
        if last
            .get(account_id)
            .is_some_and(|t| now.saturating_duration_since(*t) < RETRY_AFTER)
        {
            return false;
        }
        if IN_FLIGHT.swap(true, Ordering::AcqRel) {
            return false;
        }
        last.insert(account_id.to_string(), now);
    }
    let homes = homes.to_vec();
    std::thread::spawn(move || {
        let ok = run(&homes);
        IN_FLIGHT.store(false, Ordering::Release);
        if ok {
            on_refreshed();
        }
    });
    true
}

/// `claude -p /usage`를 한 번 돌린다. 종료 코드 0이면 Ok.
///
/// - `--no-session-persistence`: 없으면 `<config>/projects`에 세션 JSONL이 남고, 마지막
///   엔트리가 `user`라 활동 감지가 그 세션을 도는 중으로 본다
/// - `disableAllHooks`: 사용자 훅이 돌면 우리 `trw hook`까지 상태 파일을 남긴다
/// - `--strict-mcp-config`: MCP 서버를 띄우지 않는다. 띄우면 느리고 OAuth 창이 뜰 수 있다
/// - `--bare`는 못 쓴다. OAuth와 Keychain을 아예 안 읽어서 갱신이 일어나지 않는다
///
/// 출력은 버린다. 사용률을 거기서 파싱하지 않고, 실패 메시지에 계정 정보가 섞일 수 있다.
pub fn run_usage(bin: &Path, home: &CliHome, timeout: Duration) -> Result<(), &'static str> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-p",
        "/usage",
        "--output-format",
        "json",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ])
    .current_dir(std::env::temp_dir())
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    // 앱이 Claude 세션 안에서 떴으면 이 값들이 상속된다. 남기면 엉뚱한 계정을 갱신하거나
    // API 키 경로를 타서 OAuth 갱신이 안 일어난다.
    for key in [
        "CLAUDE_CONFIG_DIR",
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ] {
        cmd.env_remove(key);
    }
    if let CliHome::ConfigDir(dir) = home {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = cmd.spawn().map_err(|_| "spawn")?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err("exit"),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timeout");
            }
        }
    }
}

/// 고정 위치 후보. 앱은 launchd로 떠서 PATH가 짧아 설치 경로를 직접 훑는다.
/// 공식 설치 스크립트는 `~/.local/bin`, 예전 로컬 설치는 `~/.claude/local`이다.
fn fixed_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
        home.join(".npm-global/bin/claude"),
        home.join(".volta/bin/claude"),
        home.join(".bun/bin/claude"),
        home.join("Library/pnpm/claude"),
    ]
}

/// nvm은 버전마다 bin이 따로라 최신 버전부터 본다.
fn nvm_candidates(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join(".nvm/versions/node")) else {
        return Vec::new();
    };
    let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    versions.sort_by_key(|p| {
        std::cmp::Reverse(
            p.file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("")
                .trim_start_matches('v')
                .split('.')
                .map(|n| n.parse::<u64>().unwrap_or(0))
                .collect::<Vec<_>>(),
        )
    });
    versions.into_iter().map(|v| v.join("bin/claude")).collect()
}

fn candidates(home: &Path, path_env: Option<&OsStr>) -> Vec<PathBuf> {
    let mut all = fixed_candidates(home);
    all.extend(nvm_candidates(home));
    if let Some(p) = path_env {
        all.extend(std::env::split_paths(p).map(|d| d.join("claude")));
    }
    all
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 첫 실행 파일. 못 찾으면 None — 갱신을 건너뛰고 원래 실패 사유를 그대로 둔다.
pub fn find_claude_binary() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path_env = std::env::var_os("PATH");
    candidates(&home, path_env.as_deref())
        .into_iter()
        .find(|p| is_executable(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trw-claude-cli-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_bin(name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = scratch(name).join("claude");
        fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[test]
    fn runs_usage_without_persisting_or_hooks() {
        let bin = fake_bin(
            "args",
            r#"[ "$1" = "-p" ] && [ "$2" = "/usage" ] || exit 2
case "$*" in *--no-session-persistence*) ;; *) exit 3;; esac
case "$*" in *disableAllHooks*) ;; *) exit 4;; esac
case "$*" in *--strict-mcp-config*) ;; *) exit 5;; esac
exit 0"#,
        );
        assert_eq!(run_usage(&bin, &CliHome::Default, Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn config_dir_is_set_or_removed_per_home() {
        let bin = fake_bin(
            "env",
            r#"[ "$CLAUDE_CONFIG_DIR" = "/acct/conf" ] && exit 0
[ -z "${CLAUDE_CONFIG_DIR+x}" ] && exit 7
exit 1"#,
        );
        let dir = CliHome::ConfigDir(PathBuf::from("/acct/conf"));
        assert_eq!(run_usage(&bin, &dir, Duration::from_secs(5)), Ok(()));
        // 레거시 항목은 변수가 아예 없어야 읽힌다 — 비어 있어도 안 된다.
        assert_eq!(
            run_usage(&bin, &CliHome::Default, Duration::from_secs(5)),
            Err("exit")
        );
    }

    #[test]
    fn hung_cli_is_killed_at_timeout() {
        let bin = fake_bin("hang", "exec sleep 30");
        let start = Instant::now();
        let r = run_usage(&bin, &CliHome::Default, Duration::from_millis(500));
        assert_eq!(r, Err("timeout"));
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    }

    #[test]
    fn request_backs_off_per_account_and_respects_toggle() {
        let homes = [CliHome::Default];
        let now = Instant::now();
        assert!(!request_with("off", &homes, false, now, |_| true, || {}));
        assert!(!request_with("none", &[], true, now, |_| true, || {}));

        let (tx, rx) = mpsc::channel();
        assert!(request_with("acct-a", &homes, true, now, |_| true, move || {
            tx.send(()).unwrap();
        }));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // 성공해도 RETRY_AFTER 안에는 다시 띄우지 않는다.
        assert!(!request_with("acct-a", &homes, true, now + Duration::from_secs(60), |_| true, || {}));
        // 끝난 뒤면 다른 계정은 바로 된다.
        let (tx2, rx2) = mpsc::channel();
        assert!(request_with("acct-b", &homes, true, now, |_| false, move || {
            tx2.send(()).unwrap();
        }));
        // 실패하면 콜백을 부르지 않는다.
        assert!(rx2.recv_timeout(Duration::from_millis(500)).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert!(request_with("acct-a", &homes, true, now + RETRY_AFTER, |_| false, || {}));
    }

    #[test]
    fn local_install_comes_first() {
        let home = Path::new("/h");
        let all = candidates(home, Some(OsStr::new("/p/bin")));
        assert_eq!(all[0], PathBuf::from("/h/.local/bin/claude"));
        assert_eq!(all.last().unwrap(), &PathBuf::from("/p/bin/claude"));
    }
}
