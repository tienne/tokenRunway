//! Claude Code 사용량 수집기 (P0).
//!
//! 데이터 소스: `~/.claude/projects/<프로젝트>/<세션>.jsonl`
//! 각 assistant 메시지 라인의 `timestamp` + `message.usage`에서
//! 토큰을 추출해 시계열 샘플로 변환한다.

use super::{find_recent_jsonl, OfficialUsage, UsageProvider, UsageSample};
use chrono::DateTime;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// OAuth 사용량 엔드포인트 (비공식). `/usage` 명령을 구동하는 데이터 소스.
const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// 폴링 최소 간격. 이 엔드포인트는 공격적으로 rate limit을 걸어 180초 미만은 위험.
const USAGE_CACHE_TTL: Duration = Duration::from_secs(180);

/// claude 버전을 얻지 못했을 때의 폴백. User-Agent 프리픽스(claude-code/)가 중요.
const FALLBACK_VERSION: &str = "2.1.178";

struct CachedUsage {
    fetched_at: Instant,
    usage: Option<OfficialUsage>,
}

/// 전역 사용률 캐시. provider가 매 호출마다 새로 생성돼도 rate limit을 넘지 않도록.
static USAGE_CACHE: Mutex<Option<CachedUsage>> = Mutex::new(None);

pub struct ClaudeCodeProvider {
    projects_dir: PathBuf,
}

impl ClaudeCodeProvider {
    pub fn new() -> Self {
        let projects_dir = dirs::home_dir()
            .map(|h| h.join(".claude").join("projects"))
            .unwrap_or_default();
        Self { projects_dir }
    }
}

impl Default for ClaudeCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// JSONL 한 줄의 관심 필드만 부분 역직렬화.
#[derive(Deserialize)]
struct TranscriptLine {
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    usage: Option<Usage>,
}

/// Claude Code transcript의 usage 블록.
/// cache_read까지 합산해 한도 소진을 보수적으로 추정한다.
#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl Usage {
    fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}

impl UsageProvider for ClaudeCodeProvider {
    fn tool_name(&self) -> &'static str {
        "Claude Code"
    }

    fn available(&self) -> bool {
        self.projects_dir.is_dir()
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        let mut samples = Vec::new();

        // 구조: projects/<프로젝트 디렉토리>/<세션>.jsonl
        for path in find_recent_jsonl(&self.projects_dir, since_ms) {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                if let Some(sample) = parse_line(line, since_ms) {
                    samples.push(sample);
                }
            }
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        samples
    }

    fn official_usage(&self) -> Option<OfficialUsage> {
        fetch_official_usage_cached()
    }
}

/// 180초 캐시를 적용해 공식 사용률을 반환. 캐시가 신선하면 HTTP 호출을 건너뛴다.
fn fetch_official_usage_cached() -> Option<OfficialUsage> {
    let mut cache = USAGE_CACHE.lock().ok()?;
    if let Some(c) = cache.as_ref() {
        if c.fetched_at.elapsed() < USAGE_CACHE_TTL {
            return c.usage.clone();
        }
    }
    let usage = fetch_official_usage();
    *cache = Some(CachedUsage {
        fetched_at: Instant::now(),
        usage: usage.clone(),
    });
    usage
}

/// OAuth `/api/oauth/usage`를 호출해 5시간/주간 사용률을 가져온다.
fn fetch_official_usage() -> Option<OfficialUsage> {
    let (token, plan) = read_oauth_credentials()?;
    let version = claude_version();
    let user_agent = format!("claude-code/{version}");

    // User-Agent 프리픽스가 없으면 즉시 영구 429 버킷에 빠진다.
    let resp = ureq::get(OAUTH_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", &user_agent)
        .set("Content-Type", "application/json")
        .call()
        .ok()?;

    let parsed: UsageResponse = resp.into_json().ok()?;
    let five = parsed.five_hour?;
    let seven = parsed.seven_day.unwrap_or(UsageWindow {
        utilization: 0.0,
        resets_at: String::new(),
    });

    Some(OfficialUsage {
        five_hour_utilization: five.utilization,
        five_hour_resets_at: five.resets_at,
        seven_day_utilization: seven.utilization,
        seven_day_resets_at: seven.resets_at,
        plan,
    })
}

/// Keychain `Claude Code-credentials`에서 OAuth access token을 읽는다 (macOS).
///
/// macOS의 `security` CLI를 쓴다 — account 이름 추정 없이 service만으로 조회 가능.
/// 다른 OS는 추후 `keyring` crate로 확장한다.
#[cfg(target_os = "macos")]
fn read_oauth_credentials() -> Option<(String, Option<String>)> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let creds: Credentials = serde_json::from_str(raw.trim()).ok()?;
    let o = creds.claude_ai_oauth;
    let plan = format_claude_plan(o.rate_limit_tier.as_deref(), o.subscription_type.as_deref());
    Some((o.access_token, plan))
}

#[cfg(not(target_os = "macos"))]
fn read_oauth_credentials() -> Option<(String, Option<String>)> {
    None
}

/// rateLimitTier("default_claude_max_5x") → "Max 5x". 없으면 subscriptionType.
fn format_claude_plan(tier: Option<&str>, sub: Option<&str>) -> Option<String> {
    if let Some(t) = tier {
        if let Some(rest) = t.strip_prefix("default_claude_") {
            let joined = rest
                .split('_')
                .map(capitalize_word)
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                return Some(joined);
            }
        }
    }
    sub.filter(|s| !s.is_empty()).map(capitalize_word)
}

fn capitalize_word(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `claude --version` 출력에서 시맨틱 버전을 추출. 실패 시 폴백.
fn claude_version() -> String {
    let output = Command::new("claude").arg("--version").output();
    if let Ok(out) = output {
        if let Ok(text) = String::from_utf8(out.stdout) {
            if let Some(v) = extract_semver(&text) {
                return v;
            }
        }
    }
    FALLBACK_VERSION.to_string()
}

/// 문자열에서 첫 X.Y.Z 패턴을 추출.
fn extract_semver(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut dots = 0;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                if bytes[i] == b'.' {
                    dots += 1;
                }
                i += 1;
            }
            if dots >= 2 {
                return Some(text[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    None
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

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    #[serde(default)]
    utilization: f64,
    #[serde(default)]
    resets_at: String,
}

/// 한 줄을 파싱해 윈도우 내 유효 샘플이면 반환.
fn parse_line(line: &str, since_ms: i64) -> Option<UsageSample> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let usage = parsed.message?.usage?;
    let ts = parsed.timestamp?;
    let timestamp_ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    if timestamp_ms < since_ms {
        return None;
    }
    Some(UsageSample {
        timestamp_ms,
        amount: usage.total(),
    })
}
