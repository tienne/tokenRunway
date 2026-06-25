//! Google Antigravity CLI 사용량 수집기.
//!
//! 데이터 소스: `~/.gemini/antigravity-cli/log/cli-YYYYMMDD_HHMMSS.log`.
//! CLI가 사용자 메시지를 agent로 보낼 때 남기는 로그 한 건을 요청 한 건으로 집계한다.
//! Antigravity의 절대 quota/credit 한도는 로컬 파일이나 공개 API로 제공되지 않으므로
//! 요청 추세만 표시하고 잔여율은 추정하지 않는다.

use super::{UsageProvider, UsageSample};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use std::fs;
use std::path::{Path, PathBuf};

const USER_MESSAGE_MARKER: &str = "Sending user message to conversation ";

pub struct AntigravityProvider {
    app_dir: PathBuf,
}

impl AntigravityProvider {
    pub fn new() -> Self {
        let app_dir = dirs::home_dir()
            .map(|h| h.join(".gemini").join("antigravity-cli"))
            .unwrap_or_default();
        Self { app_dir }
    }
}

impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageProvider for AntigravityProvider {
    fn tool_name(&self) -> &'static str {
        "Antigravity"
    }

    fn unit(&self) -> &'static str {
        "requests"
    }

    fn window_secs(&self) -> i64 {
        24 * 3600
    }

    fn available(&self) -> bool {
        self.app_dir.is_dir()
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        let mut samples = Vec::new();
        let Ok(entries) = fs::read_dir(self.app_dir.join("log")) else {
            return samples;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(year) = log_file_year(&path) else {
                continue;
            };
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };

            for line in content.lines() {
                if let Some(timestamp_ms) = parse_user_message(line, year) {
                    if timestamp_ms >= since_ms {
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
            }
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        samples
    }
}

/// `cli-20260621_114247.log`에서 로그 행에 생략된 연도를 얻는다.
fn log_file_year(path: &Path) -> Option<i32> {
    let name = path.file_name()?.to_str()?;
    if !name.starts_with("cli-") || !name.ends_with(".log") {
        return None;
    }
    name.get(4..8)?.parse().ok()
}

/// glog 형식 `I0621 11:43:24.123456 ...] message`를 로컬 epoch millis로 변환한다.
fn parse_user_message(line: &str, year: i32) -> Option<i64> {
    if !line.contains(USER_MESSAGE_MARKER) {
        return None;
    }

    let prefix = line.get(..21)?;
    let month: u32 = prefix.get(1..3)?.parse().ok()?;
    let day: u32 = prefix.get(3..5)?.parse().ok()?;
    let time = prefix.get(6..21)?;
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let datetime = NaiveDateTime::parse_from_str(
        &format!("{} {time}", date.format("%Y-%m-%d")),
        "%Y-%m-%d %H:%M:%S%.f",
    )
    .ok()?;
    Local
        .from_local_datetime(&datetime)
        .single()
        .map(|d| d.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_message_timestamp() {
        let line = "I0621 11:43:24.552321 69828 manager.go:1] Sending user message to conversation abc (items=1, media=0)";
        let actual = parse_user_message(line, 2026).unwrap();
        let expected = Local
            .with_ymd_and_hms(2026, 6, 21, 11, 43, 24)
            .single()
            .unwrap()
            .timestamp_millis()
            + 552;
        assert_eq!(actual, expected);
    }

    #[test]
    fn ignores_unrelated_and_malformed_lines() {
        assert!(parse_user_message("I0621 11:43:24.552321 server started", 2026).is_none());
        assert!(parse_user_message("bad Sending user message to conversation abc", 2026).is_none());
    }

    #[test]
    fn extracts_year_only_from_antigravity_log_name() {
        assert_eq!(
            log_file_year(Path::new("cli-20260621_114247.log")),
            Some(2026)
        );
        assert_eq!(log_file_year(Path::new("cli.log")), None);
    }
}
