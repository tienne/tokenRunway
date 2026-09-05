//! Claude Code 계정 발견 — 한 머신에 계정이 여럿일 때 전부 찾아낸다.
//!
//! Claude Code 자체는 계정 하나만 활성으로 들고 있다. 오르카 같은 래퍼는 계정을
//! 바꿀 때 Keychain 항목을 통째로 갈아끼우고 원래 것을 자기 저장소에 백업해둔다.
//! 그래서 활성 계정만 읽으면 나머지 계정의 잔여율이 통째로 안 보인다.
//!
//! 훑는 곳은 세 군데다.
//! 1. Keychain `Claude Code-credentials[-<sha256(config_dir) 앞 8자>]` — `CLAUDE_CONFIG_DIR`로
//!    계정을 나누면 여기에 항목이 여러 개 생긴다. 해시는 되돌릴 수 없으니 후보 디렉토리를
//!    해싱해 서비스 이름을 만들고 그 이름으로 직접 조회한다
//! 2. 오르카 `claude-runtime-auth/system-default-auth.json` — 오르카 계정으로 전환되면서
//!    밀려난 원래 로그인
//! 3. 오르카 `claude-accounts/<uuid>/` + Keychain `Orca Claude Code Managed Credentials`
//!
//! 2번과 3번은 오르카 내부 구조라 포맷이 바뀌면 못 읽는다. 실패하면 조용히 건너뛰고
//! 나머지 계정만 쓴다 — 계정 하나라도 보이는 게 아무것도 안 보이는 것보다 낫다.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 활성 계정이 쓰는 Keychain 서비스 접두어. 뒤에 config dir 해시가 붙기도 한다.
const KEYCHAIN_PREFIX: &str = "Claude Code-credentials";

/// 오르카가 비활성 계정 토큰을 보관하는 Keychain 서비스.
const ORCA_KEYCHAIN_SERVICE: &str = "Orca Claude Code Managed Credentials";

/// 찾아낸 계정 하나.
#[derive(Clone)]
pub struct ClaudeAccount {
    /// 조직 UUID. 이메일이 같아도 조직이 다르면 쿼터가 따로 돌아서 이걸로 가른다.
    /// 조직을 못 읽으면 토큰 지문으로 대신한다.
    pub id: String,
    /// 표시용 이름 (조직명 → 이메일 → 플랜 순으로 폴백).
    pub label: String,
    pub token: String,
    pub plan: Option<String>,
    pub rate_mult: Option<f64>,
    /// 지금 Claude Code가 실제로 쓰는 계정인지.
    ///
    /// 로컬 JSONL(`~/.claude/projects`)의 주인이 이 계정 하나뿐이라, 한도 역산이
    /// 성립하는지도 이 값으로 가른다.
    pub is_active: bool,
}

/// 토큰을 든 구조체라 Debug를 파생하지 않는다.
///
/// 파생해두면 나중에 디버그 출력이나 panic 메시지 한 줄이 베어러 토큰을 그대로
/// 로그에 남긴다. 실수로 `{:?}`를 써도 안전하도록 여기서 가린다.
impl fmt::Debug for ClaudeAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeAccount")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("token", &"<redacted>")
            .field("plan", &self.plan)
            .field("rate_mult", &self.rate_mult)
            .field("is_active", &self.is_active)
            .finish()
    }
}

/// 계정 이름표와 활성 판정에 쓰는 프로필 조각.
#[derive(Debug, Clone, Deserialize)]
struct Profile {
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
    #[serde(rename = "organizationName")]
    organization_name: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeConfig {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<Profile>,
}

/// 오르카가 밀어낸 시스템 기본 로그인을 백업해두는 파일.
#[derive(Deserialize)]
struct OrcaSystemAuth {
    /// 접미사 붙은 Keychain 항목에서 캡처한 credential JSON 문자열.
    #[serde(rename = "keychainCredentialsJson")]
    keychain: Option<String>,
    /// 접미사 없는 레거시 항목 쪽. 위가 비면 이걸 쓴다.
    #[serde(rename = "legacyKeychainCredentialsJson")]
    legacy: Option<String>,
    #[serde(rename = "configOauthAccount")]
    profile: Option<Profile>,
}

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeAiOauth,
}

#[derive(Deserialize)]
struct ClaudeAiOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

/// 이 머신에서 찾을 수 있는 모든 Claude 계정. 활성 계정이 항상 앞에 온다.
///
/// 계정을 못 찾으면 빈 목록 — 호출자가 기존 단일 계정 경로로 돌아간다.
pub fn discover() -> Vec<ClaudeAccount> {
    // 지금 Claude Code가 쓰는 조직. 활성 판정의 기준점이다.
    let active_org = dirs::home_dir()
        .and_then(|h| read_profile(&h.join(".claude.json")))
        .and_then(|p| p.organization_uuid)
        .filter(|s| !s.is_empty());

    // 홈 스캔은 한 번만 한다 — 서비스 후보마다 다시 훑으면 계정 수만큼 반복된다.
    let candidates = config_dir_candidates();

    let mut out = keychain_accounts(&candidates, active_org.as_deref());
    out.extend(orca_system_account(active_org.as_deref()));
    out.extend(orca_managed_accounts(active_org.as_deref()));

    // 같은 조직이 여러 소스에 걸쳐 나온다. 먼저 들어온 쪽이 이긴다.
    let mut seen: HashSet<String> = HashSet::new();
    out.retain(|a| seen.insert(a.id.clone()));
    out.sort_by_key(|a| !a.is_active);
    out
}

/// Keychain의 Claude Code credential 항목들.
fn keychain_accounts(candidates: &[PathBuf], active_org: Option<&str>) -> Vec<ClaudeAccount> {
    let mut out = Vec::new();
    for (service, config_dir) in keychain_services(candidates) {
        let Some(raw) = keychain_password(&service, None) else {
            continue;
        };
        // 프로필을 못 읽으면 홈 것으로 때우지 않는다. 다른 config dir의 계정이
        // 홈 조직 UUID를 얻으면 중복 제거에 삼켜져 목록에서 통째로 사라진다.
        let profile = profile_paths(&config_dir)
            .iter()
            .find_map(|p| read_profile(p));
        if let Some(acc) = from_credentials(&raw, profile, active_org) {
            out.push(acc);
        }
    }
    out
}

/// 조회할 Keychain 서비스 이름과 그 이름이 가리키는 config dir.
///
/// 예전에는 `security dump-keychain`으로 이름을 긁었는데, 그러면 Claude와 무관한
/// 항목까지 키체인 전체를 훑고 출력 형식(hex blob, 이스케이프된 따옴표)에도 약했다.
/// 후보 디렉토리를 해싱해 이름을 만들면 그 두 문제가 함께 없어진다.
fn keychain_services(candidates: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let home_claude = dirs::home_dir()
        .map(|h| h.join(".claude"))
        .unwrap_or_default();
    // 접미사 없는 레거시 항목은 기본 config dir 것이다.
    let mut out = vec![(KEYCHAIN_PREFIX.to_string(), home_claude)];
    for dir in candidates {
        if let Some(hash) = dir_hash(dir) {
            out.push((format!("{KEYCHAIN_PREFIX}-{hash}"), dir.clone()));
        }
    }
    out
}

/// 오르카가 자기 계정으로 전환하면서 밀어낸 원래 로그인.
fn orca_system_account(active_org: Option<&str>) -> Option<ClaudeAccount> {
    let path = orca_dir()?
        .join("claude-runtime-auth")
        .join("system-default-auth.json");
    let auth: OrcaSystemAuth = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let creds = auth.keychain.or(auth.legacy)?;
    from_credentials(&creds, auth.profile, active_org)
}

/// 오르카가 관리하는 계정들 — 디렉토리에서 이름표, Keychain에서 토큰을 얻는다.
fn orca_managed_accounts(active_org: Option<&str>) -> Vec<ClaudeAccount> {
    let Some(root) = orca_dir().map(|d| d.join("claude-accounts")) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(uuid) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(raw) = keychain_password(ORCA_KEYCHAIN_SERVICE, Some(&uuid)) else {
            continue;
        };
        let profile = read_profile(&entry.path().join("auth").join("oauth-account.json"));
        if let Some(acc) = from_credentials(&raw, profile, active_org) {
            out.push(acc);
        }
    }
    out
}

/// credential JSON + 프로필 → 계정. 토큰이 없으면 쓸모가 없어 버린다.
fn from_credentials(
    raw: &str,
    profile: Option<Profile>,
    active_org: Option<&str>,
) -> Option<ClaudeAccount> {
    let creds: Credentials = serde_json::from_str(raw.trim()).ok()?;
    let o = creds.claude_ai_oauth;
    if o.access_token.is_empty() {
        return None;
    }
    let plan = super::claude_code::format_claude_plan(
        o.rate_limit_tier.as_deref(),
        o.subscription_type.as_deref(),
    );
    let org = profile
        .as_ref()
        .and_then(|p| p.organization_uuid.clone())
        .filter(|s| !s.is_empty());
    let id = org.clone().unwrap_or_else(|| fingerprint(&o.access_token));
    // Keychain에 항목이 있다는 것만으로 활성이라고 보면 config dir을 나눈 환경에서
    // 활성이 여럿이 된다. 로컬 JSONL의 주인은 `~/.claude.json`이 가리키는 계정
    // 하나뿐이라 그 조직과 맞는 계정만 활성이다.
    let is_active = matches!((org.as_deref(), active_org), (Some(a), Some(b)) if a == b);
    let label = profile
        .as_ref()
        .and_then(|p| {
            p.organization_name
                .clone()
                .or_else(|| p.email_address.clone())
        })
        .filter(|s| !s.is_empty())
        .or_else(|| plan.clone())
        .unwrap_or_else(|| id.chars().take(8).collect());

    Some(ClaudeAccount {
        id,
        label,
        rate_mult: super::claude_code::tier_multiplier(o.rate_limit_tier.as_deref()),
        plan,
        token: o.access_token,
        is_active,
    })
}

/// Keychain 항목의 비밀 값. account를 주면 같은 서비스 안에서 그 계정 것만 집는다.
#[cfg(target_os = "macos")]
fn keychain_password(service: &str, account: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("security");
    cmd.args(["find-generic-password", "-s", service]);
    if let Some(a) = account {
        cmd.args(["-a", a]);
    }
    let out = cmd.arg("-w").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(not(target_os = "macos"))]
fn keychain_password(_service: &str, _account: Option<&str>) -> Option<String> {
    None
}

/// config dir의 계정 프로필 파일 후보. `~/.claude` → `~/.claude.json` 규칙을 먼저 본다.
fn profile_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(s) = dir.to_str() {
        out.push(PathBuf::from(format!("{s}.json")));
    }
    out.push(dir.join(".claude.json"));
    out
}

/// `CLAUDE_CONFIG_DIR`로 쓸 만한 디렉토리들. 서비스 이름을 만들 후보다.
fn config_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    out.push(home.join(".claude"));
    // 계정을 나눠 쓸 때 홈 아래 `.claude-work` 같은 이름이 흔하다.
    if let Ok(entries) = fs::read_dir(&home) {
        for e in entries.flatten() {
            let p = e.path();
            let is_claude_dir = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".claude") && n != ".claude");
            if is_claude_dir && p.is_dir() {
                out.push(p);
            }
        }
    }
    if let Ok(entries) = fs::read_dir(home.join(".config")) {
        for e in entries.flatten() {
            let p = e.path();
            let is_claude_dir = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("claude"));
            if is_claude_dir && p.is_dir() {
                out.push(p);
            }
        }
    }
    out
}

fn read_profile(path: &Path) -> Option<Profile> {
    let text = fs::read_to_string(path).ok()?;
    // config 파일은 oauthAccount로 감싸고, 오르카 계정 파일은 프로필 자체를 담는다.
    if let Ok(cfg) = serde_json::from_str::<ClaudeConfig>(&text) {
        if let Some(p) = cfg.oauth_account {
            return Some(p);
        }
    }
    serde_json::from_str::<Profile>(&text).ok()
}

fn orca_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join("Library")
            .join("Application Support")
            .join("orca"),
    )
}

/// 경로의 sha256 앞 8자 — Keychain 서비스 접미사와 같은 규칙.
fn dir_hash(dir: &Path) -> Option<String> {
    Some(sha8(dir.to_str()?))
}

fn sha8(s: &str) -> String {
    hex(s).chars().take(8).collect()
}

/// 토큰 지문 — 조직을 모를 때 계정을 가르는 대체 키. 토큰 자체는 남기지 않는다.
fn fingerprint(token: &str) -> String {
    hex(token).chars().take(16).collect()
}

fn hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(token: &str, sub: &str) -> String {
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","subscriptionType":"{sub}"}}}}"#)
    }

    fn profile(org: &str, name: &str) -> Profile {
        Profile {
            organization_uuid: Some(org.to_string()),
            organization_name: Some(name.to_string()),
            email_address: None,
        }
    }

    #[test]
    fn service_name_uses_config_dir_hash() {
        // Keychain 접미사 규칙이 경로 sha256 앞 8자라는 전제를 고정한다.
        let dirs = vec![PathBuf::from("/Users/example/.claude-work")];
        let services = keychain_services(&dirs);
        let expected = format!("{KEYCHAIN_PREFIX}-{}", sha8("/Users/example/.claude-work"));
        assert!(services.iter().any(|(name, _)| *name == expected));
    }

    #[test]
    fn legacy_service_name_comes_first() {
        // 접미사 없는 항목은 기본 config dir 것이라 항상 조회 대상이다.
        let services = keychain_services(&[]);
        assert_eq!(services[0].0, KEYCHAIN_PREFIX);
    }

    #[test]
    fn profile_paths_prefer_sibling_json() {
        let paths = profile_paths(Path::new("/Users/example/.claude"));
        assert_eq!(paths[0], PathBuf::from("/Users/example/.claude.json"));
    }

    #[test]
    fn only_the_org_claude_code_uses_is_active() {
        // 로컬 JSONL의 주인은 하나뿐이라 그 조직과 맞는 계정만 활성이어야 한다.
        let mine = from_credentials(
            &creds("tok-a", "team"),
            Some(profile("org-a", "A")),
            Some("org-a"),
        )
        .expect("계정이 나와야 한다");
        let other = from_credentials(
            &creds("tok-b", "team"),
            Some(profile("org-b", "B")),
            Some("org-a"),
        )
        .expect("계정이 나와야 한다");
        assert!(mine.is_active);
        assert!(!other.is_active);
    }

    #[test]
    fn unknown_org_is_never_active() {
        // 조직을 못 읽으면 로컬 샘플의 주인인지 확인할 방법이 없다.
        let acc = from_credentials(&creds("tok", "team"), None, Some("org-a"))
            .expect("계정이 나와야 한다");
        assert!(!acc.is_active);
        assert_eq!(acc.label, "Team");
        // 조직을 모르면 토큰 지문이 id가 된다 — 토큰 원문이 새지 않아야 한다.
        assert_ne!(acc.id, "tok");
    }

    #[test]
    fn empty_token_is_rejected() {
        assert!(from_credentials(&creds("", "team"), None, None).is_none());
    }

    #[test]
    fn debug_hides_the_token() {
        let acc = from_credentials(&creds("secret-token", "team"), None, None)
            .expect("계정이 나와야 한다");
        let shown = format!("{acc:?}");
        assert!(!shown.contains("secret-token"));
        assert!(shown.contains("redacted"));
    }
}
