//! `codex app-server`로 계정 사용량을 다시 묻는 폴백.
//!
//! 사용량 API(wham/usage)는 auth.json의 access token을 그대로 쓰는데, 오래 안 쓴 홈은
//! 토큰이 만료돼 401이 난다. 우리가 토큰을 갱신하면 CLI가 쥔 refresh token이 죽어서
//! 직접 갱신하지 않고, 갱신 규칙을 아는 Codex 바이너리에게 맡긴다.

use super::{RateLimits, RateWindow};
use serde::Deserialize;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// `account/rateLimits/read`의 `result`. app-server는 experimental이라 필드가 바뀔 수
/// 있어 쓰는 것만 받고 전부 Option으로 둔다.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadResult {
    rate_limits: Option<Snapshot>,
    rate_limit_reset_credits: Option<ResetCredits>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    plan_type: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    #[serde(default)]
    used_percent: f64,
    resets_at: Option<i64>,
    window_duration_mins: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetCredits {
    #[serde(default)]
    available_count: i64,
}

/// 이보다 크면 밀리초로 본다. 초 단위로는 5138년이라 섞일 일이 없다.
const MS_THRESHOLD: i64 = 100_000_000_000;

impl From<Window> for RateWindow {
    fn from(w: Window) -> Self {
        let resets_at = w.resets_at.unwrap_or(0);
        RateWindow {
            used_percent: w.used_percent,
            resets_at: if resets_at > MS_THRESHOLD {
                resets_at / 1000
            } else {
                resets_at
            },
            window_minutes: w.window_duration_mins.filter(|m| *m > 0),
        }
    }
}

/// `result` JSON → rate_limits. primary가 없으면 잔여율을 못 만드니 실패로 본다.
fn parse_result(result: serde_json::Value) -> Option<RateLimits> {
    let r: ReadResult = serde_json::from_value(result).ok()?;
    let snap = r.rate_limits?;
    Some(RateLimits {
        primary: Some(snap.primary?.into()),
        secondary: snap.secondary.map(Into::into),
        plan_type: snap.plan_type,
        reset_credits: r
            .rate_limit_reset_credits
            .and_then(|c| u32::try_from(c.available_count).ok()),
    })
}

/// 고정 위치 후보. 앱은 launchd로 떠서 PATH가 `/usr/bin:/bin` 정도뿐이라
/// 사용자 셸에서 잡히는 설치 경로를 직접 훑어야 한다.
fn fixed_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        home.join(".local/bin/codex"),
        home.join(".npm-global/bin/codex"),
        home.join(".npm-packages/bin/codex"),
        home.join(".volta/bin/codex"),
        home.join(".bun/bin/codex"),
        home.join("Library/pnpm/codex"),
        home.join(".local/share/pnpm/codex"),
    ]
}

/// nvm은 버전마다 bin이 따로라 최신 버전부터 본다.
fn nvm_candidates(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join(".nvm/versions/node")) else {
        return Vec::new();
    };
    let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    versions.sort_by_key(|p| std::cmp::Reverse(node_version(p)));
    versions.into_iter().map(|v| v.join("bin/codex")).collect()
}

/// `v22.11.0` → (22, 11, 0). 문자열 정렬이면 v9가 v22보다 앞선다.
fn node_version(dir: &Path) -> Vec<u64> {
    dir.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .trim_start_matches('v')
        .split('.')
        .map(|n| n.parse().unwrap_or(0))
        .collect()
}

fn path_candidates(path_env: Option<&OsStr>) -> Vec<PathBuf> {
    path_env
        .map(|p| std::env::split_paths(p).map(|d| d.join("codex")).collect())
        .unwrap_or_default()
}

/// 탐색 순서 전체.
fn candidates(home: &Path, path_env: Option<&OsStr>) -> Vec<PathBuf> {
    let mut all = fixed_candidates(home);
    all.extend(nvm_candidates(home));
    all.extend(path_candidates(path_env));
    all
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 첫 실행 파일. 못 찾으면 None — 폴백을 건너뛰고 원래 실패 사유를 그대로 둔다.
pub(super) fn find_codex_binary() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path_env = std::env::var_os("PATH");
    candidates(&home, path_env.as_deref())
        .into_iter()
        .find(|p| is_executable(p))
}

#[cfg(test)]
mod tests {
    use super::super::to_usage;
    use super::*;

    const SAMPLE: &str = r#"{
        "rateLimits": {
            "limitId": "codex",
            "planType": "plus",
            "primary": {"usedPercent": 42, "resetsAt": 1900000000, "windowDurationMins": 300},
            "secondary": {"usedPercent": 17, "resetsAt": 1900500000, "windowDurationMins": 10080},
            "credits": {"hasCredits": false, "unlimited": false}
        },
        "rateLimitResetCredits": {
            "availableCount": 2,
            "credits": [{"id": "c1", "grantedAt": 1800000000, "resetType": "codexRateLimits", "status": "available"}]
        },
        "rateLimitsByLimitId": null
    }"#;

    #[test]
    fn app_server_sample_fills_same_usage_as_rollout() {
        let rl = parse_result(serde_json::from_str(SAMPLE).unwrap()).unwrap();
        assert_eq!(rl.reset_credits, Some(2));

        let rollout_line = r#"{"primary":{"used_percent":42.0,"resets_at":1900000000,"window_minutes":300},
            "secondary":{"used_percent":17.0,"resets_at":1900500000,"window_minutes":10080},
            "plan_type":"plus"}"#;
        let from_rollout: RateLimits = serde_json::from_str(rollout_line).unwrap();

        let now_ms = 1_899_990_000_000;
        let (a, aw) = to_usage(&rl, now_ms).unwrap();
        let (b, bw) = to_usage(&from_rollout, now_ms).unwrap();
        assert_eq!(aw, 5 * 3600);
        assert_eq!(aw, bw);
        assert_eq!(a.five_hour_utilization, 42.0);
        assert_eq!(a.five_hour_utilization, b.five_hour_utilization);
        assert_eq!(a.five_hour_resets_at, b.five_hour_resets_at);
        assert_eq!(a.seven_day_utilization, b.seven_day_utilization);
        assert_eq!(a.seven_day_resets_at, b.seven_day_resets_at);
        assert_eq!(a.plan.as_deref(), Some("Plus"));
        assert_eq!(a.plan, b.plan);
        assert_eq!(a.is_estimate, b.is_estimate);
    }

    #[test]
    fn millisecond_reset_is_normalized() {
        let w: Window = serde_json::from_str(
            r#"{"usedPercent": 5, "resetsAt": 1900000000123, "windowDurationMins": 10080}"#,
        )
        .unwrap();
        let rw: RateWindow = w.into();
        assert_eq!(rw.resets_at, 1_900_000_000);
        assert_eq!(rw.window_minutes, Some(10080));
    }

    #[test]
    fn missing_or_zero_credits() {
        let mut v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        v.as_object_mut().unwrap().remove("rateLimitResetCredits");
        assert_eq!(parse_result(v.clone()).unwrap().reset_credits, None);
        v["rateLimitResetCredits"] = serde_json::json!({"availableCount": 0, "credits": null});
        assert_eq!(parse_result(v).unwrap().reset_credits, Some(0));
    }

    #[test]
    fn unexpected_shape_is_none_not_panic() {
        for bad in [
            r#"{}"#,
            r#"{"rateLimits": {}}"#,
            r#"{"rateLimits": {"primary": {"usedPercent": "lots"}}}"#,
            r#"[1, 2]"#,
            r#"null"#,
        ] {
            assert!(parse_result(serde_json::from_str(bad).unwrap()).is_none(), "{bad}");
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trw-codex-bin-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fixed_candidates_cover_known_installers() {
        let home = Path::new("/Users/u");
        let got: Vec<String> = fixed_candidates(home)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(
            got,
            vec![
                "/opt/homebrew/bin/codex",
                "/usr/local/bin/codex",
                "/Users/u/.local/bin/codex",
                "/Users/u/.npm-global/bin/codex",
                "/Users/u/.npm-packages/bin/codex",
                "/Users/u/.volta/bin/codex",
                "/Users/u/.bun/bin/codex",
                "/Users/u/Library/pnpm/codex",
                "/Users/u/.local/share/pnpm/codex",
            ]
        );
    }

    #[test]
    fn nvm_newest_version_first_then_path() {
        let home = scratch("nvm");
        for v in ["v9.1.0", "v22.11.0", "v20.3.1"] {
            fs::create_dir_all(home.join(".nvm/versions/node").join(v).join("bin")).unwrap();
        }
        let path = std::env::join_paths(["/a/bin", "/b/bin"]).unwrap();
        let all = candidates(&home, Some(&path));
        let fixed = fixed_candidates(&home).len();
        let tail: Vec<String> = all[fixed..]
            .iter()
            .map(|p| p.strip_prefix(&home).unwrap_or(p).display().to_string())
            .collect();
        assert_eq!(
            tail,
            vec![
                ".nvm/versions/node/v22.11.0/bin/codex",
                ".nvm/versions/node/v20.3.1/bin/codex",
                ".nvm/versions/node/v9.1.0/bin/codex",
                "/a/bin/codex",
                "/b/bin/codex",
            ]
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn only_executable_files_count() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("exec");
        let plain = dir.join("plain");
        fs::write(&plain, "").unwrap();
        assert!(!is_executable(&plain));
        assert!(!is_executable(&dir));
        assert!(!is_executable(&dir.join("missing")));
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&plain));
        let _ = fs::remove_dir_all(&dir);
    }
}
