//! Claude Code 사용량 수집기 (P0).
//!
//! 데이터 소스: `~/.claude/projects/<프로젝트>/<세션>.jsonl`
//! 각 assistant 메시지 라인의 `timestamp` + `message.usage`에서
//! 토큰을 추출해 시계열 샘플로 변환한다.

use super::{find_recent_jsonl, OfficialUsage, SampleCache, UsageProvider, UsageSample};
use chrono::DateTime;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// OAuth 사용량 엔드포인트 (비공식). `/usage` 명령을 구동하는 데이터 소스.
const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// 폴링 최소 간격. 이 엔드포인트는 공격적으로 rate limit을 걸어 180초 미만은 위험.
const USAGE_CACHE_TTL: Duration = Duration::from_secs(180);

/// HTTP 타임아웃. 응답이 멈춰도 폴링 스레드가 무한정 매달리지 않게 한다.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// claude 버전을 얻지 못했을 때의 폴백. User-Agent 프리픽스(claude-code/)가 중요.
const FALLBACK_VERSION: &str = "2.1.178";

struct CachedUsage {
    fetched_at: Instant,
    usage: Option<OfficialUsage>,
    /// 실패 사유 (i18n 키). 성공 시 None.
    error: Option<&'static str>,
}

/// 전역 사용률 캐시. provider가 매 호출마다 새로 생성돼도 rate limit을 넘지 않도록.
static USAGE_CACHE: Mutex<Option<CachedUsage>> = Mutex::new(None);

/// 갱신 중복 방지용. 이 락을 잡은 스레드만 HTTP를 호출한다 —
/// 데이터 락(USAGE_CACHE)은 네트워크 대기 중에 절대 잡고 있지 않는다.
static FETCH_GUARD: Mutex<()> = Mutex::new(());

/// 시계열 샘플 캐시 TTL. 통계 기간 토글·대시보드 폴링이 같은 파싱 결과를 재사용.
const SAMPLES_CACHE_TTL: Duration = Duration::from_secs(60);

/// ~/.claude는 수백 MB라 매 호출 재파싱이 비싸다.
static SAMPLES_CACHE: SampleCache = SampleCache::new(SAMPLES_CACHE_TTL);

/// `claude --version`은 서브프로세스라 비싸다. 프로세스당 1회만 실행한다.
static CLAUDE_VERSION: LazyLock<String> = LazyLock::new(read_claude_version);

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
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

/// 모델별 단가 (USD per MTok): (input, output, cache_read, cache_write).
/// claude-api 가격표 기준 (cache_write는 5분 TTL 1.25x 근사).
fn price_per_mtok(model: &str) -> (f64, f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("opus") {
        (5.0, 25.0, 0.5, 6.25)
    } else if m.contains("haiku") {
        (1.0, 5.0, 0.1, 1.25)
    } else {
        // sonnet 및 기타 기본
        (3.0, 15.0, 0.3, 3.75)
    }
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
        if let Some(hit) = SAMPLES_CACHE.get(since_ms) {
            return hit;
        }

        let mut samples = Vec::new();
        // Claude Code는 세션 재개·worktree·압축 등으로 같은 메시지를 transcript에
        // 여러 번 기록한다. message.id로 중복 제거해야 사용량이 부풀려지지 않는다.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 구조: projects/<프로젝트 디렉토리>/<세션>.jsonl
        for path in find_recent_jsonl(&self.projects_dir, since_ms) {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                if let Some((id, sample)) = parse_line(line, since_ms) {
                    if let Some(id) = id {
                        if !seen.insert(id) {
                            continue; // 이미 집계한 메시지
                        }
                    }
                    samples.push(sample);
                }
            }
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        SAMPLES_CACHE.put(since_ms, &samples);
        samples
    }

    fn official_usage(&self) -> Option<OfficialUsage> {
        fetch_cached().0
    }

    fn status_note(&self) -> Option<String> {
        fetch_cached().1.map(|s| s.to_string())
    }
}

/// 180초 캐시를 적용해 (사용률, 실패사유) 반환. 캐시가 신선하면 HTTP 호출 생략.
///
/// 네트워크 호출 동안 데이터 락을 잡지 않는다 — 잡으면 응답이 늦어질 때
/// UI 폴링과 백그라운드 루프가 전부 그 락에 매달려 앱이 굳는다.
fn fetch_cached() -> (Option<OfficialUsage>, Option<&'static str>) {
    if let Some(fresh) = cached(true) {
        return fresh;
    }

    // 이미 다른 스레드가 갱신 중이면 기다리지 않고 직전 값을 쓴다.
    let Ok(_guard) = FETCH_GUARD.try_lock() else {
        return cached(false).unwrap_or((None, None));
    };
    // 락을 얻는 사이에 그 스레드가 채워놨을 수 있다.
    if let Some(fresh) = cached(true) {
        return fresh;
    }

    let (usage, error) = fetch_official_usage();
    if let Ok(mut cache) = USAGE_CACHE.lock() {
        *cache = Some(CachedUsage {
            fetched_at: Instant::now(),
            usage: usage.clone(),
            error,
        });
    }
    (usage, error)
}

/// 캐시된 값. `require_fresh`면 TTL 안쪽일 때만 반환한다.
fn cached(require_fresh: bool) -> Option<(Option<OfficialUsage>, Option<&'static str>)> {
    let guard = USAGE_CACHE.lock().ok()?;
    let c = guard.as_ref()?;
    if require_fresh && c.fetched_at.elapsed() >= USAGE_CACHE_TTL {
        return None;
    }
    Some((c.usage.clone(), c.error))
}

/// OAuth `/api/oauth/usage`를 호출. 실패 시 사유(i18n 키)를 함께 반환.
fn fetch_official_usage() -> (Option<OfficialUsage>, Option<&'static str>) {
    let Some((token, plan, rate_mult)) = read_oauth_credentials() else {
        return (None, Some("error.no_token"));
    };
    let user_agent = format!("claude-code/{}", &*CLAUDE_VERSION);

    // User-Agent 프리픽스가 없으면 즉시 영구 429 버킷에 빠진다.
    let resp = ureq::get(OAUTH_USAGE_URL)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", &user_agent)
        .set("Content-Type", "application/json")
        .call();

    let resp = match resp {
        Ok(r) => r,
        // 토큰 만료(401), rate limit(429), 기타 구분
        Err(ureq::Error::Status(401, _)) => return (None, Some("error.expired")),
        Err(ureq::Error::Status(429, _)) => return (None, Some("error.rate_limit")),
        Err(_) => return (None, Some("error.unavailable")),
    };

    let Ok(parsed) = resp.into_json::<UsageResponse>() else {
        return (None, Some("error.unavailable"));
    };
    let Some(five) = parsed.five_hour else {
        return (None, Some("error.unavailable"));
    };
    let seven = parsed.seven_day.unwrap_or(UsageWindow {
        utilization: 0.0,
        resets_at: String::new(),
    });

    (
        Some(OfficialUsage {
            five_hour_utilization: five.utilization,
            five_hour_resets_at: five.resets_at,
            seven_day_utilization: seven.utilization,
            seven_day_resets_at: seven.resets_at,
            plan,
            rate_limit_multiplier: rate_mult,
            is_estimate: false,
        }),
        None,
    )
}

/// Keychain `Claude Code-credentials`에서 OAuth access token을 읽는다 (macOS).
///
/// macOS의 `security` CLI를 쓴다 — account 이름 추정 없이 service만으로 조회 가능.
/// 다른 OS는 추후 `keyring` crate로 확장한다.
#[cfg(target_os = "macos")]
fn read_oauth_credentials() -> Option<(String, Option<String>, Option<f64>)> {
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
    let rate_mult = tier_multiplier(o.rate_limit_tier.as_deref());
    Some((o.access_token, plan, rate_mult))
}

#[cfg(not(target_os = "macos"))]
fn read_oauth_credentials() -> Option<(String, Option<String>, Option<f64>)> {
    None
}

/// rateLimitTier("default_claude_max_5x") → 5.0. "..._pro"→1.0. 모르면 None.
///
/// 엔터프라이즈 좌석도 rateLimitTier(예: max_5x)가 있어, 이 배수로 개인 플랜 환산의
/// 기준선(Pro=1x)을 역산할 수 있다.
fn tier_multiplier(tier: Option<&str>) -> Option<f64> {
    let rest = tier?
        .strip_prefix("default_claude_")
        .unwrap_or("")
        .to_lowercase();
    if rest == "pro" {
        return Some(1.0);
    }
    if rest.contains("max") {
        let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
        return digits.parse::<f64>().ok();
    }
    None
}

/// 플랜 표시명 결정.
///
/// `subscriptionType`(청구 플랜)과 `rateLimitTier`(레이트리밋 등급)는 엔터프라이즈에서
/// 갈린다 — enterprise 좌석도 rateLimitTier는 `default_claude_max_5x`로 내려온다. 이때
/// 등급을 그대로 쓰면 "Max 5x"로 오표시되고 요금제 추천도 오판하므로, 관리형 플랜
/// (enterprise/team)은 subscriptionType을 우선한다. 개인 구독자는 rateLimitTier가 더
/// granular(5x/20x 구분)해서 그대로 쓴다.
fn format_claude_plan(tier: Option<&str>, sub: Option<&str>) -> Option<String> {
    // 관리형(청구) 플랜은 subscriptionType이 진실 — 등급(max_5x)보다 우선.
    if let Some(s) = sub {
        let low = s.to_lowercase();
        if low == "enterprise" || low == "team" {
            return Some(capitalize_word(s));
        }
    }
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
fn read_claude_version() -> String {
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

/// 한 줄을 파싱해 (중복 제거용 message.id, 샘플)을 반환.
fn parse_line(line: &str, since_ms: i64) -> Option<(Option<String>, UsageSample)> {
    // 빠른 사전 필터: usage 블록이 없는 라인(대부분 user/tool 메시지)은
    // JSON 파싱 비용을 들이지 않고 즉시 건너뛴다. (수백 MB 파싱 → usage 라인만)
    if !line.contains("\"usage\"") {
        return None;
    }
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let message = parsed.message?;
    let usage = message.usage?;
    let ts = parsed.timestamp?;
    let timestamp_ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    if timestamp_ms < since_ms {
        return None;
    }
    let (pin, pout, pcr, pcw) = price_per_mtok(message.model.as_deref().unwrap_or(""));
    let cost_usd = (usage.input_tokens as f64 * pin
        + usage.output_tokens as f64 * pout
        + usage.cache_read_input_tokens as f64 * pcr
        + usage.cache_creation_input_tokens as f64 * pcw)
        / 1_000_000.0;

    Some((
        message.id,
        UsageSample {
            timestamp_ms,
            amount: usage.total(),
            cost_usd,
            cache_read: usage.cache_read_input_tokens,
            cache_write: usage.cache_creation_input_tokens,
            input_total: usage.input_tokens
                + usage.cache_creation_input_tokens
                + usage.cache_read_input_tokens,
            model: message.model.clone(),
        },
    ))
}
