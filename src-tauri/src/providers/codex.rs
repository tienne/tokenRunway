//! OpenAI Codex 사용량 수집기 (P1).
//!
//! 데이터 소스: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! `event_msg` 타입의 `token_count` 이벤트에서 턴별 토큰을 추출한다.

use super::{find_recent_jsonl, OfficialUsage, UsageProvider, UsageSample};
use chrono::DateTime;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// 최신 rate_limits를 찾기 위해 스캔할 최근 파일 범위.
const RATE_LIMIT_LOOKBACK_MS: i64 = 24 * 3600 * 1000;

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
}

/// Codex가 token_count 이벤트에 함께 기록하는 공식 rate limit 상태.
#[derive(Deserialize)]
struct RateLimits {
    /// 5시간 윈도우 (window_minutes ≈ 300).
    primary: Option<RateWindow>,
    /// 주간 윈도우 (window_minutes ≈ 10080).
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
}

impl UsageProvider for CodexProvider {
    fn tool_name(&self) -> &'static str {
        "Codex"
    }

    fn available(&self) -> bool {
        self.sessions_dir.is_dir()
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        let mut samples = Vec::new();

        for path in find_recent_jsonl(&self.sessions_dir, since_ms) {
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
        let now_ms = chrono::Utc::now().timestamp_millis();
        let since_ms = now_ms - RATE_LIMIT_LOOKBACK_MS;

        // 최근 파일들에서 가장 신선한 rate_limits를 찾는다.
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

        let (_, rl) = latest?;
        let plan = rl.plan_type.as_deref().map(capitalize);
        let primary = rl.primary?;
        Some(OfficialUsage {
            five_hour_utilization: primary.used_percent,
            five_hour_resets_at: epoch_to_rfc3339(primary.resets_at),
            seven_day_utilization: rl.secondary.as_ref().map_or(0.0, |s| s.used_percent),
            seven_day_resets_at: rl
                .secondary
                .as_ref()
                .map(|s| epoch_to_rfc3339(s.resets_at))
                .unwrap_or_default(),
            plan,
        })
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
    let tokens = payload.info?.last_token_usage?.total_tokens;
    let ts = parsed.timestamp?;
    let timestamp_ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    if timestamp_ms < since_ms {
        return None;
    }
    Some(UsageSample {
        timestamp_ms,
        amount: tokens,
    })
}
