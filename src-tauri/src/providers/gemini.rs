//! Google Gemini 사용량 수집기 (P2, AgentBar 방식).
//!
//! Gemini CLI는 로컬에 토큰을 남기지 않으므로 **요청 수(requests)** 를 센다.
//! 데이터 소스: `~/.gemini/tmp/<프로젝트>/logs.json` (JSON 배열).
//! `type == "user"` 항목 1건을 요청 1건으로 집계하고, 윈도우는 일간(24h)이다.

use super::{OfficialUsage, UsageProvider, UsageSample};
use chrono::{DateTime, Duration as ChronoDuration, Local};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// Gemini 무료 티어 일일 요청 한도 (추정 가정). AgentBar와 동일.
const DAILY_REQUEST_LIMIT: f64 = 1000.0;

pub struct GeminiProvider {
    tmp_dir: PathBuf,
}

impl GeminiProvider {
    pub fn new() -> Self {
        let tmp_dir = dirs::home_dir()
            .map(|h| h.join(".gemini").join("tmp"))
            .unwrap_or_default();
        Self { tmp_dir }
    }
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct LogEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
}

impl UsageProvider for GeminiProvider {
    fn tool_name(&self) -> &'static str {
        "Gemini"
    }

    fn unit(&self) -> &'static str {
        "requests"
    }

    fn window_secs(&self) -> i64 {
        24 * 3600
    }

    fn available(&self) -> bool {
        self.tmp_dir.is_dir()
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        let mut samples = Vec::new();
        let Ok(projects) = fs::read_dir(&self.tmp_dir) else {
            return samples;
        };

        // 구조: tmp/<프로젝트>/logs.json
        for project in projects.flatten() {
            let logs = project.path().join("logs.json");
            let Ok(content) = fs::read_to_string(&logs) else {
                continue;
            };
            let Ok(entries) = serde_json::from_str::<Vec<LogEntry>>(&content) else {
                continue;
            };
            for entry in entries {
                if entry.kind.as_deref() != Some("user") {
                    continue;
                }
                let Some(ts) = entry.timestamp else { continue };
                let Ok(dt) = DateTime::parse_from_rfc3339(&ts) else {
                    continue;
                };
                let timestamp_ms = dt.timestamp_millis();
                if timestamp_ms < since_ms {
                    continue;
                }
                samples.push(UsageSample {
                    timestamp_ms,
                    amount: 1,
                    cost_usd: 0.0,
                    cache_read: 0,
                    cache_write: 0,
                    input_total: 0,
                    model: None,
                });
            }
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        samples
    }

    fn official_usage(&self) -> Option<OfficialUsage> {
        // 오늘 자정(로컬) 이후 요청 수를 일일 한도로 나눠 사용률을 추정.
        let now = Local::now();
        let today_midnight = now
            .date_naive()
            .and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local)
            .single()?;
        let tomorrow_midnight = today_midnight + ChronoDuration::days(1);

        let since_ms = today_midnight.timestamp_millis();
        let count = self.collect_samples(since_ms).len() as f64;
        let utilization = (count / DAILY_REQUEST_LIMIT * 100.0).min(100.0);

        Some(OfficialUsage {
            five_hour_utilization: utilization,
            five_hour_resets_at: tomorrow_midnight.to_rfc3339(),
            seven_day_utilization: 0.0,
            seven_day_resets_at: String::new(),
            plan: None,
            rate_limit_multiplier: None,
        })
    }
}
