//! OpenAI Codex 사용량 수집기 (P1).
//!
//! 데이터 소스: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! `event_msg` 타입의 `token_count` 이벤트에서 턴별 토큰을 추출한다.

use super::{find_recent_jsonl, OfficialUsage, SampleCache, UsageProvider, UsageSample};
use chrono::DateTime;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 최신 rate_limits를 찾기 위해 스캔할 최근 파일 범위.
const RATE_LIMIT_LOOKBACK_MS: i64 = 24 * 3600 * 1000;

/// rate_limits를 아직 못 읽었을 때 쓰는 윈도우.
const DEFAULT_WINDOW_SECS: i64 = 5 * 3600;

/// primary가 이 이상이면 주간 한도로 보고 7일 슬롯에도 같은 값을 넣는다.
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 3600;

/// 파싱 캐시 TTL. 대시보드 폴링과 통계 조회가 같은 결과를 재사용한다.
const CACHE_TTL: Duration = Duration::from_secs(60);

static SAMPLES_CACHE: SampleCache = SampleCache::new(CACHE_TTL);

/// rate_limits 조회 캐시 — (조회시각, 사용률, 윈도우 초).
/// 24시간치 파일 재스캔이 폴링마다 반복되지 않게 한다.
static RATE_LIMIT_CACHE: Mutex<Option<(Instant, Option<OfficialUsage>, i64)>> = Mutex::new(None);

pub struct CodexProvider {
    sessions_dir: PathBuf,
}

impl CodexProvider {
    pub fn new() -> Self {
        let sessions_dir = dirs::home_dir()
            .map(|h| h.join(".codex").join("sessions"))
            .unwrap_or_default();
        Self { sessions_dir }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct TranscriptLine {
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    info: Option<Info>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct Info {
    /// 직전 턴의 토큰 사용량 (증분). 시계열 합산에 사용.
    last_token_usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    /// Codex의 input_tokens는 cached_input_tokens를 포함한 입력 총량.
    #[serde(default)]
    input_tokens: u64,
}

/// Codex가 token_count 이벤트에 함께 기록하는 공식 rate limit 상태.
#[derive(Deserialize)]
struct RateLimits {
    /// 짧은 쪽 윈도우. 길이는 계정마다 달라 `window_minutes`로 확인해야 한다 —
    /// 5시간(300)인 계정도, 주간(10080) 하나뿐인 계정도 있다.
    primary: Option<RateWindow>,
    /// 긴 쪽 윈도우. primary가 이미 주간이면 없다.
    secondary: Option<RateWindow>,
    /// 구독 등급 (예: "plus", "pro").
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct RateWindow {
    #[serde(default)]
    used_percent: f64,
    /// epoch seconds.
    #[serde(default)]
    resets_at: i64,
    /// 윈도우 길이(분). 300=5시간, 10080=주간.
    window_minutes: Option<i64>,
}

impl UsageProvider for CodexProvider {
    fn tool_name(&self) -> &'static str {
        "Codex"
    }

    fn available(&self) -> bool {
        self.sessions_dir.is_dir()
    }

    /// primary 윈도우 길이를 그대로 따른다 — 계정에 따라 5시간이거나 주간이다.
    /// 5시간으로 고정하면 주간 계정에서 윈도우가 리셋 직전 5시간(=미래)으로 잡혀
    /// 사용량이 0이 되고, 비활성 숨김에 걸려 카드까지 사라진다.
    fn window_secs(&self) -> i64 {
        self.rate_limits().1
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        if let Some(hit) = SAMPLES_CACHE.get(since_ms) {
            return hit;
        }

        let mut samples = Vec::new();
        // 세션을 resume하면 이전 턴의 token_count가 새 rollout 파일에 다시 실린다.
        // Claude transcript와 달리 메시지 id가 없어 (시각, 토큰 조합)으로 구분한다 —
        // 밀리초 타임스탬프까지 같으면서 토큰 수도 같은 별개의 턴은 사실상 없다.
        let mut seen: HashSet<(i64, u64, u64, u64)> = HashSet::new();

        for path in find_recent_jsonl(&self.sessions_dir, since_ms) {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                let Some(sample) = parse_line(line, since_ms) else {
                    continue;
                };
                let key = (
                    sample.timestamp_ms,
                    sample.amount,
                    sample.input_total,
                    sample.cache_read,
                );
                if seen.insert(key) {
                    samples.push(sample);
                }
            }
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        SAMPLES_CACHE.put(since_ms, &samples);
        samples
    }

    fn official_usage(&self) -> Option<OfficialUsage> {
        self.rate_limits().0
    }
}

impl CodexProvider {
    /// (사용률, 윈도우 초) — 60초 캐시.
    fn rate_limits(&self) -> (Option<OfficialUsage>, i64) {
        if let Ok(cache) = RATE_LIMIT_CACHE.lock() {
            if let Some((at, value, window)) = cache.as_ref() {
                if at.elapsed() < CACHE_TTL {
                    return (value.clone(), *window);
                }
            }
        }
        let (fresh, window) = self.scan_rate_limits();
        if let Ok(mut cache) = RATE_LIMIT_CACHE.lock() {
            *cache = Some((Instant::now(), fresh.clone(), window));
        }
        (fresh, window)
    }

    /// 최근 24시간 rollout 파일에서 가장 신선한 rate_limits를 읽는다.
    fn scan_rate_limits(&self) -> (Option<OfficialUsage>, i64) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let since_ms = now_ms - RATE_LIMIT_LOOKBACK_MS;

        let mut latest: Option<(i64, RateLimits)> = None;
        for path in find_recent_jsonl(&self.sessions_dir, since_ms) {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                if let Some((ts, rl)) = parse_rate_limits(line) {
                    if latest.as_ref().is_none_or(|(t, _)| ts > *t) {
                        latest = Some((ts, rl));
                    }
                }
            }
        }

        let Some((_, rl)) = latest else {
            return (None, DEFAULT_WINDOW_SECS);
        };
        let plan = rl.plan_type.as_deref().map(capitalize);
        let Some(primary) = rl.primary else {
            return (None, DEFAULT_WINDOW_SECS);
        };
        let window_secs = primary
            .window_minutes
            .filter(|m| *m > 0)
            .map_or(DEFAULT_WINDOW_SECS, |m| m * 60);
        let resets_at = epoch_to_rfc3339(primary.resets_at);

        // secondary가 없는데 primary가 이미 주간이면 그게 유일한 한도다 —
        // 주간 슬롯을 0으로 두면 "주간 100% 남음"이라는 거짓 여유가 표시된다.
        // 양쪽에 같은 값을 넣고, UI는 윈도우가 7일 이상이면 주간 줄을 숨긴다.
        let weekly_only = rl.secondary.is_none() && window_secs >= WEEKLY_WINDOW_SECS;
        let usage = OfficialUsage {
            five_hour_utilization: primary.used_percent,
            five_hour_resets_at: resets_at.clone(),
            seven_day_utilization: match (&rl.secondary, weekly_only) {
                (Some(s), _) => s.used_percent,
                (None, true) => primary.used_percent,
                (None, false) => 0.0,
            },
            seven_day_resets_at: match (&rl.secondary, weekly_only) {
                (Some(s), _) => epoch_to_rfc3339(s.resets_at),
                (None, true) => resets_at,
                (None, false) => String::new(),
            },
            plan,
            rate_limit_multiplier: None, // Codex는 소비자 사다리 배수 미상.
            is_estimate: false,          // Codex가 직접 기록한 공식 사용률.
        };
        (Some(usage), window_secs)
    }
}

/// 첫 글자만 대문자로 ("plus" → "Plus").
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// epoch seconds → RFC3339 문자열.
fn epoch_to_rfc3339(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

/// token_count 이벤트 줄에서 (timestamp_ms, rate_limits)를 추출.
fn parse_rate_limits(line: &str) -> Option<(i64, RateLimits)> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let payload = parsed.payload?;
    if payload.kind.as_deref() != Some("token_count") {
        return None;
    }
    let rl = payload.rate_limits?;
    let ts = parsed.timestamp?;
    let ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    Some((ms, rl))
}

/// `token_count` 이벤트 줄만 골라 턴 증분 토큰을 샘플로 변환.
fn parse_line(line: &str, since_ms: i64) -> Option<UsageSample> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let payload = parsed.payload?;
    if payload.kind.as_deref() != Some("token_count") {
        return None;
    }
    let usage = payload.info?.last_token_usage?;
    let ts = parsed.timestamp?;
    let timestamp_ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    if timestamp_ms < since_ms {
        return None;
    }
    Some(UsageSample {
        timestamp_ms,
        amount: usage.total_tokens,
        cost_usd: 0.0, // OpenAI 단가 매핑 불확실 — 추후
        cache_read: usage.cached_input_tokens,
        cache_write: 0,
        input_total: usage.input_tokens, // cached 포함된 입력 총량
        model: None,
    })
}
