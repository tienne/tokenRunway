//! Google Gemini 사용량 수집기 (P2, AgentBar 방식).
//!
//! Gemini CLI는 로컬에 토큰을 남기지 않으므로 **요청 수(requests)** 를 센다.
//! 데이터 소스: `~/.gemini/tmp/<프로젝트>/logs.json` (JSON 배열).
//! `type == "user"` 항목 1건을 요청 1건으로 집계하고, 윈도우는 일간(24h)이다.

use super::{UsageProvider, UsageSample};
use chrono::DateTime;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

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
                });
            }
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        samples
    }
}
