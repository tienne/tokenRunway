//! OpenAI Codex 사용량 수집기 (P1).
//!
//! 데이터 소스: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! `event_msg` 타입의 `token_count` 이벤트에서 턴별 토큰을 추출한다.

use super::{find_recent_jsonl, UsageProvider, UsageSample};
use chrono::DateTime;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

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
